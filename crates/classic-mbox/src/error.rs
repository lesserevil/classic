//! Errors produced by the mailbox subsystem.

use classic_proto::NodeId;

#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// Name exceeded `MAX_SVC_NAME` (256 B UTF-8).
    #[error("service name too long: {0} bytes (cap is 256)")]
    NameTooLong(usize),
}

#[derive(Debug, thiserror::Error)]
pub enum MailError {
    /// Wire payload exceeds `MAX_MAIL_BYTES` (8 MiB per plan-05).
    #[error("payload too large: {0} bytes (cap is 8 MiB)")]
    PayloadTooLarge(usize),
    /// No live peer connection to the target node. Cross-node send path
    /// will surface this; the in-process path never does.
    #[error("no live connection to peer node {0:?}")]
    NoPeer(NodeId),
    /// Cross-node dispatch is stubbed pending classic-zgg (Task 3 of
    /// plan-05). Local sends work; remote sends return this.
    #[error("cross-node mail_send not yet wired (classic-zgg follow-up)")]
    NotConnected,
    /// `mail_send` was called before `init(self_node_id)`. Daemon
    /// startup should always init the registry first; tests can call
    /// the init helper directly.
    #[error("classic_mbox::init(self_node_id) has not been called")]
    NotInitialized,
}
