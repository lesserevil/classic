//! `mail_send` — fire-and-forget delivery to a `NetId`. The local case
//! looks up the destination `MboxId` in the in-process registry and
//! `try_send`s on the bounded mpsc; the remote case is stubbed pending
//! `classic-zgg` (cross-node MailSend frames).
//!
//! "Fire-and-forget" — `Ok(())` means the payload was *handed off*, NOT
//! that the receiver saw it. Failures are limited to local validation
//! (payload size, missing self-node init); receiver-side drops are
//! silent (with a tracing::warn breadcrumb).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

use tracing::warn;

use classic_proto::{NetId, NodeId};

use crate::error::MailError;
use crate::mbox;

/// Largest payload `mail_send` accepts. Larger payloads are caller-side
/// validation errors. The frame layer enforces a separate 16 MiB cap
/// but this layer is stricter so daemons under heavy gossip don't have
/// every send teeter on the frame boundary.
pub const MAX_MAIL_BYTES: usize = 8 * 1024 * 1024;

static SELF_NODE: OnceLock<NodeId> = OnceLock::new();

/// Drop counters — bumped on local channel-full / missing-mbox so
/// tests and ops dashboards can spot mail loss without parsing logs.
pub static LOCAL_FULL_DROPS: AtomicU64 = AtomicU64::new(0);
pub static LOCAL_MISSING_DROPS: AtomicU64 = AtomicU64::new(0);

/// Initialize the registry with the daemon's NodeId so `mail_send` can
/// distinguish local from remote `NetId`s. Idempotent — repeated calls
/// after the first are silently ignored (so test runs don't have to
/// coordinate teardown).
pub fn init(self_node_id: NodeId) {
    let _ = SELF_NODE.set(self_node_id);
}

/// Returns the NodeId previously installed via `init`, or `None` if
/// `init` has not yet been called.
pub fn self_node_id() -> Option<NodeId> {
    SELF_NODE.get().copied()
}

/// Fire-and-forget send to `to`. See module docs for semantics.
pub async fn mail_send(to: NetId, payload: Vec<u8>) -> Result<(), MailError> {
    if payload.len() > MAX_MAIL_BYTES {
        return Err(MailError::PayloadTooLarge(payload.len()));
    }
    let self_id = SELF_NODE.get().ok_or(MailError::NotInitialized)?;
    if to.node == *self_id {
        deliver_local(to, payload);
        Ok(())
    } else {
        // Cross-node dispatch lives in classic-zgg (Task 3). Until it
        // lands we surface a clear "not wired" error so callers don't
        // silently lose messages to peers.
        Err(MailError::NotConnected)
    }
}

fn deliver_local(to: NetId, payload: Vec<u8>) {
    let Some(tx) = mbox::lookup(to.mbox) else {
        LOCAL_MISSING_DROPS.fetch_add(1, Ordering::Relaxed);
        warn!(?to, "mail_send local: no receiver; dropped");
        return;
    };
    if let Err(_e) = tx.try_send(payload) {
        LOCAL_FULL_DROPS.fetch_add(1, Ordering::Relaxed);
        warn!(?to, "mail_send local: mailbox full or closed; dropped");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mbox::Mailbox;
    use classic_proto::{MboxId, NodeId};

    fn local_node() -> NodeId {
        NodeId([1u8; 16])
    }

    fn remote_node() -> NodeId {
        NodeId([2u8; 16])
    }

    fn snapshot_counters() -> (u64, u64) {
        (
            LOCAL_FULL_DROPS.load(Ordering::Relaxed),
            LOCAL_MISSING_DROPS.load(Ordering::Relaxed),
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn errors_when_not_initialized() {
        // Don't init for this test. Use a unique NodeId so we don't
        // collide with parallel tests that DID init.
        //
        // SELF_NODE is a OnceLock — once set it can't be unset. Other
        // tests in this module DO call init(). To keep this assertion
        // honest, only run when SELF_NODE is empty (i.e. when this is
        // the first send test to run).
        if SELF_NODE.get().is_some() {
            // Already initialized by another test; skip.
            return;
        }
        let err = mail_send(
            NetId { node: local_node(), mbox: MboxId(1) },
            vec![],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, MailError::NotInitialized));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn payload_too_large_rejected_even_pre_init() {
        // Size check fires before the SELF_NODE check.
        let huge = vec![0u8; MAX_MAIL_BYTES + 1];
        let err = mail_send(NetId { node: local_node(), mbox: MboxId(1) }, huge)
            .await
            .unwrap_err();
        assert!(matches!(err, MailError::PayloadTooLarge(_)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn local_delivery_round_trips() {
        init(local_node());
        let (id, mut rx) = Mailbox::new();
        mail_send(
            NetId { node: local_node(), mbox: id },
            b"hello".to_vec(),
        )
        .await
        .unwrap();
        let msg = rx.recv().await;
        assert_eq!(msg.as_deref(), Some(&b"hello"[..]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn missing_local_mbox_is_ok_increments_counter() {
        init(local_node());
        let before = snapshot_counters().1;
        // mbox id far above any allocator output.
        let to = NetId { node: local_node(), mbox: MboxId(u64::MAX) };
        mail_send(to, b"void".to_vec()).await.unwrap();
        let after = snapshot_counters().1;
        assert_eq!(after, before + 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn channel_full_increments_drop_counter() {
        init(local_node());
        let (id, _rx) = Mailbox::new();
        // Saturate the bounded channel without recv()'ing.
        for _ in 0..super::super::mbox::MBOX_CAPACITY {
            mail_send(
                NetId { node: local_node(), mbox: id },
                b"x".to_vec(),
            )
            .await
            .unwrap();
        }
        let before = snapshot_counters().0;
        // One more — must drop.
        mail_send(
            NetId { node: local_node(), mbox: id },
            b"overflow".to_vec(),
        )
        .await
        .unwrap();
        let after = snapshot_counters().0;
        assert!(
            after >= before + 1,
            "expected at least one full-drop; before={before} after={after}"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remote_send_stubbed_with_not_connected() {
        init(local_node());
        let to = NetId { node: remote_node(), mbox: MboxId(1) };
        let err = mail_send(to, b"hi".to_vec()).await.unwrap_err();
        assert!(matches!(err, MailError::NotConnected));
    }
}
