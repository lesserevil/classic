//! Two-phase-commit coordinator for placement-group submissions.
//!
//! `submit_group` is the public entry point. It runs the placer
//! (`classic_place::place_group`), groups the assignment by node,
//! drives Phase 1 (parallel `GroupReserve` with per-call timeout),
//! and Phase 2 (parallel `GroupCommit` with per-call timeout). On
//! failure at either phase it emits compensating `GroupAbort`s to
//! already-acked nodes and `Kill`s to already-spawned children. The
//! return value preserves caller-declaration order of members.
//!
//! The transport layer is abstracted behind [`GroupTransport`] so
//! tests can drive the orchestrator with deterministic mocks; the
//! production implementation will be wired against `PeerMesh::send_to`
//! once cross-node spawn forwarding lands.

use std::collections::HashMap;
use std::time::Duration;

use async_trait::async_trait;
use classic_place::{place_group, GroupPlaceError, NodeAd, PlacementGroup};
use classic_proto::{NetId, NodeId};
use futures::future::join_all;
use tokio::time::timeout;

use crate::group_proto::{
    GroupAbort, GroupCommit, GroupCommitFailed, GroupId, GroupReserveAck, GroupReserveDeny,
    GroupReserveFrame, GroupSpawned, ReservedMember,
};

/// Coordinator-side reservation TTL slack — added to `reserve_timeout`
/// before being baked into the on-wire `reserve_ttl_ms`. Gives the
/// coordinator some headroom over the node-side sweeper so a node
/// never reclaims a reservation the coordinator still considers live.
pub const RESERVE_TTL_SLACK: Duration = Duration::from_millis(500);

/// Tunables for `submit_group`. Each phase has its own deadline; both
/// are wall-clock from the start of the phase, applied per-node.
#[derive(Clone, Copy, Debug)]
pub struct GroupCfg {
    pub reserve_timeout: Duration,
    pub commit_timeout: Duration,
}

impl Default for GroupCfg {
    fn default() -> Self {
        Self {
            reserve_timeout: Duration::from_secs(5),
            commit_timeout: Duration::from_secs(30),
        }
    }
}

/// Successful submission. `members` is in caller-declaration order.
#[derive(Clone, Debug)]
pub struct GroupSubmitResult {
    pub group_id: GroupId,
    pub members: Vec<(String, NetId)>,
}

/// Top-level failure modes for `submit_group`.
#[derive(thiserror::Error, Debug)]
pub enum GroupSpawnError {
    #[error("placement: {0}")]
    Place(#[from] GroupPlaceError),
    #[error("reserve denied by {node:?}: {reason}")]
    ReserveDenied { node: NodeId, reason: String },
    #[error("reserve timeout on {node:?} after {ms} ms")]
    ReserveTimeout { node: NodeId, ms: u64 },
    #[error("commit failed on {node:?}: {reason}")]
    CommitFailed { node: NodeId, reason: String },
    #[error("commit timeout on {node:?} after {ms} ms")]
    CommitTimeout { node: NodeId, ms: u64 },
    #[error("transport: {0}")]
    Transport(String),
}

/// Per-node Phase-1 outcome.
#[derive(Clone, Debug)]
pub enum ReserveResponse {
    Ack(GroupReserveAck),
    Deny(GroupReserveDeny),
    Transport(String),
}

/// Per-node Phase-2 outcome. The transport delivers one frame per
/// successful member spawn (`Spawned`) or a single `Failed` frame.
#[derive(Clone, Debug)]
pub enum CommitResponse {
    Spawned(Vec<GroupSpawned>),
    Failed(GroupCommitFailed),
    Transport(String),
}

/// Abstract transport used by the coordinator. Implementations are
/// expected to handle in-flight correlation; the coordinator only
/// orchestrates one logical request per call.
#[async_trait]
pub trait GroupTransport: Send + Sync {
    async fn reserve(&self, node: NodeId, frame: GroupReserveFrame) -> ReserveResponse;
    async fn commit(&self, node: NodeId, frame: GroupCommit) -> CommitResponse;
    /// Fire-and-forget abort. Implementations should bound their own
    /// retry/backoff; the coordinator does not await success.
    async fn abort(&self, node: NodeId, frame: GroupAbort);
    /// Fire-and-forget kill of a already-spawned child. Used during
    /// Phase-2 cleanup when a sibling node fails commit.
    async fn kill(&self, target: NetId);
}

/// Run the full group-submission pipeline. Returns Ok with all member
/// `NetId`s on success, or an error describing where in the pipeline
/// it failed (with compensating aborts/kills already issued).
pub async fn submit_group<T: GroupTransport + ?Sized>(
    group: &PlacementGroup,
    ads: &[NodeAd],
    cfg: &GroupCfg,
    transport: &T,
) -> Result<GroupSubmitResult, GroupSpawnError> {
    // Phase 0 — placement.
    let assignment = place_group(group, ads)?;
    let group_id = GroupId::random();
    let by_node = group_by_node(&assignment, group);

    // Phase 1 — parallel reserve.
    let reserve_frames: Vec<(NodeId, GroupReserveFrame)> = by_node
        .iter()
        .map(|(node, members)| {
            let frame = GroupReserveFrame {
                group_id,
                members: members.iter().map(|(_, rm)| rm.clone()).collect(),
                reserve_ttl_ms: (cfg.reserve_timeout + RESERVE_TTL_SLACK).as_millis() as u32,
            };
            (*node, frame)
        })
        .collect();

    let reserve_futures = reserve_frames.iter().map(|(node, frame)| {
        let node = *node;
        let frame = frame.clone();
        async move {
            let outcome = timeout(cfg.reserve_timeout, transport.reserve(node, frame)).await;
            (node, outcome)
        }
    });
    let reserve_outcomes: Vec<(NodeId, Result<ReserveResponse, tokio::time::error::Elapsed>)> =
        join_all(reserve_futures).await;

    // Collect tokens from acked nodes; remember the first failure (if any).
    let mut acked: HashMap<NodeId, GroupReserveAck> = HashMap::new();
    let mut phase1_err: Option<GroupSpawnError> = None;
    for (node, outcome) in reserve_outcomes {
        match outcome {
            Ok(ReserveResponse::Ack(ack)) => {
                acked.insert(node, ack);
            }
            Ok(ReserveResponse::Deny(deny)) => {
                if phase1_err.is_none() {
                    phase1_err = Some(GroupSpawnError::ReserveDenied {
                        node,
                        reason: deny.reason,
                    });
                }
            }
            Ok(ReserveResponse::Transport(reason)) => {
                if phase1_err.is_none() {
                    phase1_err = Some(GroupSpawnError::Transport(reason));
                }
            }
            Err(_) => {
                if phase1_err.is_none() {
                    phase1_err = Some(GroupSpawnError::ReserveTimeout {
                        node,
                        ms: cfg.reserve_timeout.as_millis() as u64,
                    });
                }
            }
        }
    }
    if let Some(err) = phase1_err {
        // Compensate: abort every node that acked.
        let abort_futures = acked.keys().map(|node| {
            let node = *node;
            async move { transport.abort(node, GroupAbort { group_id }).await }
        });
        join_all(abort_futures).await;
        return Err(err);
    }

    // Phase 2 — parallel commit.
    let commit_frames: Vec<(NodeId, GroupCommit)> = acked
        .iter()
        .map(|(node, ack)| {
            (
                *node,
                GroupCommit {
                    group_id,
                    tokens: ack.tokens.clone(),
                },
            )
        })
        .collect();
    let commit_futures = commit_frames.iter().map(|(node, frame)| {
        let node = *node;
        let frame = frame.clone();
        async move {
            let outcome = timeout(cfg.commit_timeout, transport.commit(node, frame)).await;
            (node, outcome)
        }
    });
    let commit_outcomes: Vec<(NodeId, Result<CommitResponse, tokio::time::error::Elapsed>)> =
        join_all(commit_futures).await;

    let mut spawned: Vec<GroupSpawned> = Vec::new();
    let mut completed_nodes: Vec<NodeId> = Vec::new();
    let mut phase2_err: Option<GroupSpawnError> = None;
    for (node, outcome) in commit_outcomes {
        match outcome {
            Ok(CommitResponse::Spawned(items)) => {
                completed_nodes.push(node);
                spawned.extend(items);
            }
            Ok(CommitResponse::Failed(failed)) => {
                if phase2_err.is_none() {
                    phase2_err = Some(GroupSpawnError::CommitFailed {
                        node,
                        reason: failed.reason,
                    });
                }
            }
            Ok(CommitResponse::Transport(reason)) => {
                if phase2_err.is_none() {
                    phase2_err = Some(GroupSpawnError::Transport(reason));
                }
            }
            Err(_) => {
                if phase2_err.is_none() {
                    phase2_err = Some(GroupSpawnError::CommitTimeout {
                        node,
                        ms: cfg.commit_timeout.as_millis() as u64,
                    });
                }
            }
        }
    }
    if let Some(err) = phase2_err {
        // Kill anything that already spawned on other nodes.
        let kill_targets: Vec<NetId> = spawned.iter().map(|s| s.net_id).collect();
        let kill_futures = kill_targets.into_iter().map(|target| async move {
            transport.kill(target).await
        });
        join_all(kill_futures).await;
        // Abort nodes that acked Phase 1 but whose commit hadn't completed.
        let completed: std::collections::HashSet<NodeId> = completed_nodes.iter().copied().collect();
        let abort_futures = acked.keys().filter_map(|node| {
            if completed.contains(node) {
                None
            } else {
                let node = *node;
                Some(async move { transport.abort(node, GroupAbort { group_id }).await })
            }
        });
        join_all(abort_futures).await;
        return Err(err);
    }

    // Restore caller-declaration order. Per-node Phase-2 streams may
    // interleave; look members up by label and re-emit in submitted
    // order.
    let mut by_label: HashMap<String, NetId> =
        spawned.into_iter().map(|s| (s.label, s.net_id)).collect();
    let mut ordered: Vec<(String, NetId)> = Vec::with_capacity(group.members.len());
    for m in &group.members {
        let net_id = by_label
            .remove(&m.label)
            .ok_or_else(|| GroupSpawnError::Transport(format!("missing spawn for {}", m.label)))?;
        ordered.push((m.label.clone(), net_id));
    }

    Ok(GroupSubmitResult {
        group_id,
        members: ordered,
    })
}

/// Group `(label, node_id)` pairs into `node -> Vec<(member_idx, ReservedMember)>`,
/// preserving caller-declaration order within each node's bucket.
fn group_by_node(
    assignment: &[(String, NodeId)],
    group: &PlacementGroup,
) -> HashMap<NodeId, Vec<(usize, ReservedMember)>> {
    let mut out: HashMap<NodeId, Vec<(usize, ReservedMember)>> = HashMap::new();
    for (i, (label, node)) in assignment.iter().enumerate() {
        let member = &group.members[i];
        let env = member
            .env
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let rm = ReservedMember {
            label: label.clone(),
            requires_src: member.requires_src.clone(),
            argv: member.argv.clone(),
            env,
        };
        out.entry(*node).or_default().push((i, rm));
    }
    out
}


