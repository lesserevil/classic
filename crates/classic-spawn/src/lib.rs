//! Spawn pipeline: originator + executor primitives. Plan-04
//! (`plans/04-spawn-pipeline.md`) is the design source.
//!
//! Public surface:
//! - `SpawnHandler` (FrameMux handler at slot 0x03) — registered by
//!   classic-node. Replaces a stub from Task 5 with the real
//!   originator/executor dispatch in Task 6.
//! - `MboxAllocator` — per-daemon monotonic counter for assigning
//!   `MboxId`s.
//! - `exec_command` / `ChildHandle` — fork+exec a process with three
//!   stdio pumps. cgroup placement is a thin wrapper around this; the
//!   wrapper is gated on root + a real cgroup mount at runtime.

use std::sync::Arc;

use classic_proto::{Frame, FrameHandler, NodeId};
use tracing::{debug, info, warn};

pub mod coord;
pub mod deny;
pub mod error;
pub mod exec;
pub mod executor;
pub mod group_proto;
pub mod mbox_alloc;
pub mod originator;
pub mod reservation;

pub use coord::{
    submit_group, CommitResponse, GroupCfg, GroupSpawnError, GroupSubmitResult, GroupTransport,
    ReserveResponse, RESERVE_TTL_SLACK,
};
pub use reservation::{
    spawn_ttl_sweeper, AbortOutcome, CommitOutcome, ReserveOutcome, ReservationTable,
    ReservedMemberSlot, TTL_SWEEP_PERIOD,
};
pub use deny::{format_candidate_denials, render_deny, terminal_reason, CandidateDenial};
pub use error::SpawnError;
pub use exec::{exec_command, ChildExitInfo, ChildHandle, ChildParts, ExecError, STDIO_CHANNEL_CAP};
pub use executor::{run_executor, LocalAdMatcher, NoOpScopeProvider, ScopeProvider};
pub use group_proto::{
    GroupAbort, GroupAbortAck, GroupCommit, GroupCommitFailed, GroupId, GroupReserveAck,
    GroupReserveDeny, GroupReserveFrame, GroupSpawned, ReservedMember,
};
pub use mbox_alloc::MboxAllocator;
pub use originator::{run_originator, AttemptOutcome, OriginatorState, PeerSpawn, Placer};

/// FrameHandler the daemon installs at slot `0x03` of the FrameMux.
/// Holds whatever state classic-spawn needs to route inbound frames; for
/// the cc8 / Task-5 milestone this is just an Arc<()> placeholder so the
/// type compiles and threads through.
pub struct SpawnHandler {
    inner: Arc<SpawnHandlerInner>,
}

struct SpawnHandlerInner;

impl SpawnHandler {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { inner: Arc::new(SpawnHandlerInner) })
    }
}

impl Default for SpawnHandler {
    fn default() -> Self {
        Self { inner: Arc::new(SpawnHandlerInner) }
    }
}

impl FrameHandler for SpawnHandler {
    fn on_frame(&self, peer: NodeId, frame: Frame) {
        let _ = &self.inner;
        dispatch_frame(peer, frame);
    }
}

/// Stub dispatch entry point. Task 6 (classic-733) replaces this with
/// the real originator + executor routing. For now we log at info level
/// for known kinds and warn for the unknown reserved range so test runs
/// have a breadcrumb.
pub fn dispatch_frame(peer: NodeId, frame: Frame) {
    use classic_proto::FrameKind;
    match frame.kind {
        k if k == FrameKind::SpawnRequest as u16 => {
            info!(?peer, "received SpawnRequest (stub: not yet wired)");
        }
        k if k == FrameKind::SpawnAck as u16 => {
            info!(?peer, "received SpawnAck (stub: not yet wired)");
        }
        k if k == FrameKind::SpawnDeny as u16 => {
            info!(?peer, "received SpawnDeny (stub: not yet wired)");
        }
        k if k == FrameKind::ChildStdio as u16 => {
            debug!(?peer, "received ChildStdio (stub: not yet wired)");
        }
        k if k == FrameKind::ChildExit as u16 => {
            info!(?peer, "received ChildExit (stub: not yet wired)");
        }
        k => warn!(?peer, kind = format!("{:#06x}", k), "unknown spawn-range frame; dropped"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use classic_proto::{encode_payload, FrameKind, NodeId, SpawnAck};

    fn id(n: u8) -> NodeId {
        NodeId([n; 16])
    }

    #[test]
    fn spawn_handler_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<SpawnHandler>();
    }

    #[test]
    fn known_frame_kinds_dispatch_without_panic() {
        let h = SpawnHandler::new();
        let payload = encode_payload(&SpawnAck {
            req_id: 1,
            net_id: classic_proto::NetId {
                node: id(2),
                mbox: classic_proto::MboxId(3),
            },
        })
        .unwrap();
        h.on_frame(
            id(1),
            Frame::new(FrameKind::SpawnAck as u16, Bytes::from(payload)),
        );
        h.on_frame(id(1), Frame::new(FrameKind::ChildExit as u16, Bytes::new()));
        h.on_frame(id(1), Frame::new(FrameKind::ChildStdio as u16, Bytes::new()));
    }

    #[test]
    fn unknown_kind_in_spawn_range_logs_warn() {
        let h = SpawnHandler::new();
        h.on_frame(id(1), Frame::new(0x0399, Bytes::new()));
    }
}
