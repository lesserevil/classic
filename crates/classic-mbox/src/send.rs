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
use std::sync::{Arc, OnceLock, RwLock};

use tracing::warn;

use classic_proto::{Frame, NetId, NodeId};

use crate::error::MailError;
use crate::frames::{encode_mail_send, MailSend};
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
/// Bumped when cross-node send has no live connection to the target.
pub static REMOTE_NO_PEER_DROPS: AtomicU64 = AtomicU64::new(0);

/// Trait the cross-node send path uses to deposit frames on a peer's
/// outbound queue. classic-node's `PeerMesh` implements this in
/// production via a thin adapter; tests use a channel-backed mock.
pub trait Peers: Send + Sync + 'static {
    /// Fire-and-forget. Implementations that can't deliver right now
    /// (no live connection, queue full) should drop the frame and
    /// return false; senders never block.
    fn send_to(&self, peer: NodeId, frame: Frame) -> bool;
}

/// Optional peer backend installed by the daemon at startup. Until set,
/// every cross-node send drops with REMOTE_NO_PEER_DROPS++.
static PEERS: OnceLock<RwLock<Option<Arc<dyn Peers>>>> = OnceLock::new();

fn peers_slot() -> &'static RwLock<Option<Arc<dyn Peers>>> {
    PEERS.get_or_init(|| RwLock::new(None))
}

/// Install (or replace) the cross-node peer backend. Idempotent.
pub fn set_peers(peers: Arc<dyn Peers>) {
    *peers_slot().write().expect("PEERS poisoned") = Some(peers);
}

fn current_peers() -> Option<Arc<dyn Peers>> {
    peers_slot().read().expect("PEERS poisoned").clone()
}

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
///
/// Local path delivers via the in-process registry. Remote path encodes
/// a `MailSend` frame and hands it to the installed `Peers` backend; if
/// the daemon hasn't installed one yet or the peer isn't connected,
/// the frame is dropped with `REMOTE_NO_PEER_DROPS++`. Either case
/// returns `Ok(())` — the contract is fire-and-forget.
pub async fn mail_send(to: NetId, payload: Vec<u8>) -> Result<(), MailError> {
    if payload.len() > MAX_MAIL_BYTES {
        return Err(MailError::PayloadTooLarge(payload.len()));
    }
    let self_id = SELF_NODE.get().ok_or(MailError::NotInitialized)?;
    if to.node == *self_id {
        deliver_local(to, payload);
        return Ok(());
    }
    // Remote.
    let frame = encode_mail_send(&MailSend {
        from: NetId { node: *self_id, mbox: classic_proto::MboxId(0) },
        to,
        payload,
    })
    .map_err(|e| match e {
        crate::frames::MboxFrameError::PayloadTooLarge(n) => MailError::PayloadTooLarge(n),
        other => MailError::PayloadTooLarge(0).tap(|_| {
            warn!(error = %other, "encode MailSend failed");
        }),
    })?;
    match current_peers() {
        Some(peers) => {
            if !peers.send_to(to.node, frame) {
                REMOTE_NO_PEER_DROPS.fetch_add(1, Ordering::Relaxed);
                warn!(?to, "mail_send remote: send_to refused; dropped");
            }
        }
        None => {
            REMOTE_NO_PEER_DROPS.fetch_add(1, Ordering::Relaxed);
            warn!(?to, "mail_send remote: no Peers backend installed; dropped");
        }
    }
    Ok(())
}

trait Tap: Sized {
    fn tap<F: FnOnce(&Self)>(self, f: F) -> Self;
}
impl<T> Tap for T {
    fn tap<F: FnOnce(&Self)>(self, f: F) -> Self {
        f(&self);
        self
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
    async fn remote_send_with_no_peers_backend_drops_silently() {
        init(local_node());
        // Drop any existing peers backend.
        *peers_slot().write().expect("peers poisoned") = None;
        let before = REMOTE_NO_PEER_DROPS.load(Ordering::Relaxed);
        let to = NetId { node: remote_node(), mbox: MboxId(1) };
        mail_send(to, b"hi".to_vec()).await.unwrap();
        let after = REMOTE_NO_PEER_DROPS.load(Ordering::Relaxed);
        assert!(after >= before + 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remote_send_writes_frame_to_peers_backend() {
        use std::sync::Mutex;
        struct CapturePeers {
            sent: Mutex<Vec<(NodeId, Frame)>>,
        }
        impl Peers for CapturePeers {
            fn send_to(&self, peer: NodeId, frame: Frame) -> bool {
                self.sent.lock().unwrap().push((peer, frame));
                true
            }
        }
        let cap = Arc::new(CapturePeers { sent: Mutex::new(Vec::new()) });
        set_peers(cap.clone());
        init(local_node());

        let to = NetId { node: remote_node(), mbox: MboxId(42) };
        mail_send(to, b"crossnode".to_vec()).await.unwrap();

        let sent = cap.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let (peer, frame) = &sent[0];
        assert_eq!(*peer, remote_node());
        assert_eq!(frame.kind, crate::frames::FRAME_MAIL_SEND);
        // Decode the encoded MailSend and validate fields.
        let decoded = crate::frames::decode_mbox_frame(frame).unwrap();
        match decoded {
            crate::frames::MboxInbound::MailSend(m) => {
                assert_eq!(m.to, to);
                assert_eq!(m.payload, b"crossnode");
                assert_eq!(m.from.node, local_node());
            }
            other => panic!("expected MailSend, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn remote_send_when_backend_refuses_drops() {
        struct RefusePeers;
        impl Peers for RefusePeers {
            fn send_to(&self, _peer: NodeId, _frame: Frame) -> bool {
                false
            }
        }
        set_peers(Arc::new(RefusePeers));
        init(local_node());
        let before = REMOTE_NO_PEER_DROPS.load(Ordering::Relaxed);
        let to = NetId { node: remote_node(), mbox: MboxId(1) };
        mail_send(to, b"hi".to_vec()).await.unwrap();
        let after = REMOTE_NO_PEER_DROPS.load(Ordering::Relaxed);
        assert!(after >= before + 1);
    }
}
