//! Placement-group types and top-level `place_group` dispatcher.
//!
//! Two strategies:
//! - `Pack`  — assign all members to a single node (gang scheduling).
//! - `Spread` — assign at most one member per node (anti-affinity).
//!
//! This module owns the user-facing types and validation. Algorithm
//! bodies for PACK/SPREAD land in follow-up tasks; the stubs here
//! return the appropriate `Infeasible` variant so the dispatcher's
//! error wiring is exercised by tests from day one.
//!
//! See `plans/07-placement-groups.md` for the full design.

use std::collections::HashSet;

use classic_proto::NodeId;

use crate::ast::Requirement;
use crate::eval::matches as predicate_matches;
use crate::free_pool::{place_one_in_pool, FreePool, Picked};
use crate::model::NodeAd;

/// One placeable unit in a group. `label` is unique within the group
/// (validated) and is the key in the returned placement map. `req` is
/// a plan-03 predicate evaluated per node. `argv`/`env` ride along so
/// downstream spawn submission can use the same struct.
#[derive(Clone, Debug)]
pub struct GroupMember {
    pub label: String,
    pub req: Requirement,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// Group placement strategy. PACK collapses to a single node; SPREAD
/// fans out one-per-node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupStrategy {
    Pack,
    Spread,
}

/// A submitted group: strategy + ordered member list. Member order is
/// preserved through to the placement result.
#[derive(Clone, Debug)]
pub struct PlacementGroup {
    pub strategy: GroupStrategy,
    pub members: Vec<GroupMember>,
}

/// Failure modes for `place_group`. Each variant's message is
/// user-facing — `classic submit` renders them directly.
#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum GroupPlaceError {
    #[error("empty placement group")]
    Empty,
    #[error("duplicate member label: {0}")]
    DuplicateLabel(String),
    #[error("PACK: no single node satisfies all {0} member requirements")]
    PackInfeasible(usize),
    #[error("SPREAD: cluster has {nodes} nodes, group needs {needed}")]
    SpreadTooLarge { needed: usize, nodes: usize },
    #[error("SPREAD: no one-member-per-node assignment satisfies all requirements")]
    SpreadInfeasible,
    #[error("member {label}: no node matches requirement")]
    MemberInfeasible { label: String },
}

/// Top-level group placer. Validates the group shape, then dispatches
/// to the strategy-specific algorithm. On success returns an ordered
/// `(label, node_id)` list — same order as `group.members` so callers
/// can zip back to argv/env.
pub fn place_group(
    group: &PlacementGroup,
    ads: &[NodeAd],
) -> Result<Vec<(String, NodeId)>, GroupPlaceError> {
    if group.members.is_empty() {
        return Err(GroupPlaceError::Empty);
    }
    let mut seen: HashSet<&str> = HashSet::with_capacity(group.members.len());
    for m in &group.members {
        if !seen.insert(m.label.as_str()) {
            return Err(GroupPlaceError::DuplicateLabel(m.label.clone()));
        }
    }
    match group.strategy {
        GroupStrategy::Pack => place_pack(group, ads),
        GroupStrategy::Spread => place_spread(group, ads),
    }
}

/// PACK — assign every member to the same single node.
///
/// 1. Sort candidate nodes by total free GPU memory descending; tie
///    by `node_id` bytes ascending. This biases PACK toward
///    GPU-richest hosts and is deterministic.
/// 2. For each candidate, snapshot its `FreePool`, then iterate
///    members in declaration order. For each member call
///    `place_one_in_pool`; if the member places, deduct from the
///    pool and continue. On any miss, abandon this candidate.
/// 3. First fully-satisfying candidate wins. If none satisfies all
///    members, return `PackInfeasible(members.len())`.
fn place_pack(
    group: &PlacementGroup,
    ads: &[NodeAd],
) -> Result<Vec<(String, NodeId)>, GroupPlaceError> {
    let mut ranked: Vec<&NodeAd> = ads.iter().collect();
    ranked.sort_by(rank_pack_desc);
    for ad in ranked {
        if let Some(_picks) = try_pack_on(ad, &group.members) {
            return Ok(group
                .members
                .iter()
                .map(|m| (m.label.clone(), ad.node_id))
                .collect());
        }
    }
    Err(GroupPlaceError::PackInfeasible(group.members.len()))
}

/// Try to place every member on a single ad. Returns `Some(picks)`
/// only if every member is placed; the pool deductions inside the
/// loop enforce intra-group contention.
fn try_pack_on(ad: &NodeAd, members: &[GroupMember]) -> Option<Vec<Picked>> {
    let mut pool = FreePool::from_ad(ad);
    let mut chosen: Vec<Picked> = Vec::with_capacity(members.len());
    for m in members {
        match place_one_in_pool(&m.req, ad, &pool) {
            Some(picked) => {
                pool.deduct(&picked);
                chosen.push(picked);
            }
            None => return None,
        }
    }
    Some(chosen)
}

/// `Vec::sort_by` comparator: ads with the most free GPU memory come
/// first, ties broken by `node_id` ascending.
fn rank_pack_desc(a: &&NodeAd, b: &&NodeAd) -> std::cmp::Ordering {
    let a_free: u64 = a.gpu.iter().filter(|g| !g.in_use).map(|g| g.vram_mb).sum();
    let b_free: u64 = b.gpu.iter().filter(|g| !g.in_use).map(|g| g.vram_mb).sum();
    b_free.cmp(&a_free).then_with(|| a.node_id.0.cmp(&b.node_id.0))
}

/// SPREAD — assign every member to a distinct node.
///
/// 1. Cheap cardinality check: more members than nodes -> `SpreadTooLarge`.
/// 2. For each member, build the list of candidate `NodeId`s whose
///    ad satisfies that member's requirement. Members carry their
///    original index so we can restore caller declaration order at
///    the end.
/// 3. Sort the per-member list by candidate-count ascending — the
///    minimum-remaining-values heuristic from CSP solvers. Branches
///    on the most-constrained member fail fastest, pruning the
///    search tree.
/// 4. Recursive backtracking; first complete assignment wins.
/// 5. Reorder the assignment back to caller's member order before
///    returning. On any backtracking failure: `SpreadInfeasible`.
fn place_spread(
    group: &PlacementGroup,
    ads: &[NodeAd],
) -> Result<Vec<(String, NodeId)>, GroupPlaceError> {
    let needed = group.members.len();
    let nodes = ads.len();
    if needed > nodes {
        return Err(GroupPlaceError::SpreadTooLarge { needed, nodes });
    }
    // Build (original_index, candidate_node_ids) for each member.
    let mut per_member: Vec<(usize, Vec<NodeId>)> = group
        .members
        .iter()
        .enumerate()
        .map(|(i, m)| (i, candidates(&m.req, ads)))
        .collect();
    // MRV: try the most-constrained members first.
    per_member.sort_by_key(|(_, c)| c.len());
    // Each backtracking frame stashes (original_index, picked_node_id).
    let mut used: HashSet<NodeId> = HashSet::new();
    let mut assignment: Vec<(usize, NodeId)> = Vec::with_capacity(needed);
    if !backtrack(&per_member, 0, &mut used, &mut assignment) {
        return Err(GroupPlaceError::SpreadInfeasible);
    }
    // Restore caller declaration order.
    assignment.sort_by_key(|(orig_idx, _)| *orig_idx);
    Ok(assignment
        .into_iter()
        .map(|(orig_idx, node)| (group.members[orig_idx].label.clone(), node))
        .collect())
}

/// Build the list of `NodeId`s in `ads` that satisfy `req`. Preserves
/// the input order so backtracking exploration is deterministic.
fn candidates(req: &Requirement, ads: &[NodeAd]) -> Vec<NodeId> {
    ads.iter()
        .filter(|ad| predicate_matches(req, ad))
        .map(|ad| ad.node_id)
        .collect()
}

/// Recursive bipartite-matching backtrack.
fn backtrack(
    per_member: &[(usize, Vec<NodeId>)],
    i: usize,
    used: &mut HashSet<NodeId>,
    assignment: &mut Vec<(usize, NodeId)>,
) -> bool {
    if i == per_member.len() {
        return true;
    }
    let (orig_idx, cands) = &per_member[i];
    for node in cands {
        if used.contains(node) {
            continue;
        }
        used.insert(*node);
        assignment.push((*orig_idx, *node));
        if backtrack(per_member, i + 1, used, assignment) {
            return true;
        }
        assignment.pop();
        used.remove(node);
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_req;

    fn member(label: &str, req_src: &str) -> GroupMember {
        GroupMember {
            label: label.into(),
            req: parse_req(req_src).expect("test predicate must parse"),
            argv: vec![],
            env: vec![],
        }
    }

    #[test]
    fn empty_group_rejected_before_strategy() {
        let g = PlacementGroup {
            strategy: GroupStrategy::Pack,
            members: vec![],
        };
        assert_eq!(place_group(&g, &[]).unwrap_err(), GroupPlaceError::Empty);
    }

    #[test]
    fn duplicate_label_rejected() {
        let g = PlacementGroup {
            strategy: GroupStrategy::Spread,
            members: vec![member("trainer", "true"), member("trainer", "true")],
        };
        let err = place_group(&g, &[]).unwrap_err();
        assert_eq!(err, GroupPlaceError::DuplicateLabel("trainer".into()));
    }

    #[test]
    fn pack_with_no_ads_returns_pack_infeasible() {
        let g = PlacementGroup {
            strategy: GroupStrategy::Pack,
            members: vec![member("a", "true"), member("b", "true")],
        };
        assert_eq!(
            place_group(&g, &[]).unwrap_err(),
            GroupPlaceError::PackInfeasible(2),
        );
    }

    #[test]
    fn spread_with_no_ads_is_too_large_not_infeasible() {
        // 1 member, 0 nodes: cardinality check fires first.
        let g = PlacementGroup {
            strategy: GroupStrategy::Spread,
            members: vec![member("a", "true")],
        };
        assert_eq!(
            place_group(&g, &[]).unwrap_err(),
            GroupPlaceError::SpreadTooLarge { needed: 1, nodes: 0 },
        );
    }
}
