//! Errors produced by the mailbox subsystem. Placeholder shell for plan-05
//! Task 1 — the delivery / gossip / GC tasks fill in the rest of the
//! variants.

#[derive(Debug, thiserror::Error)]
pub enum MailError {
    /// Wire payload exceeds the per-frame cap (8 MiB per plan-05).
    /// Populated by the cross-node send task.
    #[error("payload too large: {0} bytes")]
    PayloadTooLarge(usize),
    /// No live connection to the target node. Surfaced by the cross-node
    /// send task; in-process sends never see this.
    #[error("no live connection to peer node {0:?}")]
    NoPeer(classic_proto::NodeId),
}
