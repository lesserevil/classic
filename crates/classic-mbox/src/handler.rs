//! `FrameHandler` for the mbox range. Decodes inbound 0x02xx frames
//! and routes them: `MailSend` → local delivery (with optional
//! `MailDeliveryFailure` reply on miss / full); `ServiceAd` /
//! `ServiceForget` / `ServiceSync` / `ServiceSyncResponse` →
//! `ServiceDirectory` (Task 4); `MailDeliveryFailure` → tracing log
//! only (best-effort information).

use std::sync::Arc;

use tracing::{debug, warn};

use classic_proto::{Frame, FrameHandler, NodeId};

use crate::frames::{
    decode_mbox_frame, encode_mail_delivery_failure, DeliveryFailureReason,
    MailDeliveryFailure, MailSend, MboxInbound,
};
use crate::mbox;
use crate::send::{self, Peers};

/// FrameHandler installed at slot 0x02 of the FrameMux.
pub struct MboxHandler {
    /// Peers backend used to send `MailDeliveryFailure` replies. None
    /// disables replies (still delivers locally / logs).
    peers: Option<Arc<dyn Peers>>,
}

impl MboxHandler {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { peers: None })
    }

    pub fn with_peers(peers: Arc<dyn Peers>) -> Arc<Self> {
        Arc::new(Self { peers: Some(peers) })
    }
}

impl Default for MboxHandler {
    fn default() -> Self {
        Self { peers: None }
    }
}

impl FrameHandler for MboxHandler {
    fn on_frame(&self, peer: NodeId, frame: Frame) {
        let inbound = match decode_mbox_frame(&frame) {
            Ok(v) => v,
            Err(e) => {
                debug!(?peer, error = %e, "dropping mbox frame");
                return;
            }
        };
        match inbound {
            MboxInbound::MailSend(m) => handle_mail_send(self.peers.as_ref(), m),
            MboxInbound::MailDeliveryFailure(f) => {
                // Informational — no retries.
                warn!(?peer, ?f, "received MailDeliveryFailure");
            }
            // ServiceAd / ServiceForget / ServiceSync / ServiceSyncResponse
            // land in classic-jja (gossip task). For now they're a no-op
            // breadcrumb so tests can see the message type was decoded.
            other => debug!(?peer, ?other, "service frame deferred to gossip task"),
        }
    }
}

fn handle_mail_send(peers: Option<&Arc<dyn Peers>>, m: MailSend) {
    let dest_mbox = m.to.mbox;
    let Some(tx) = mbox::lookup(dest_mbox) else {
        warn!(to = ?m.to, "MailSend: no receiver locally; dropping");
        reply_failure(peers, m.from.node, m.to, DeliveryFailureReason::UnknownMbox);
        return;
    };
    if let Err(_e) = tx.try_send(m.payload) {
        warn!(to = ?m.to, "MailSend: receiver mailbox full or closed; dropping");
        reply_failure(peers, m.from.node, m.to, DeliveryFailureReason::MboxFull);
    }
}

fn reply_failure(
    peers: Option<&Arc<dyn Peers>>,
    sender: NodeId,
    to: classic_proto::NetId,
    reason: DeliveryFailureReason,
) {
    let Some(peers) = peers else { return };
    // Don't bounce a delivery failure back to ourselves (defensive — the
    // local path doesn't go through here, but if it ever did this would
    // be a loop).
    if Some(sender) == send::self_node_id() {
        return;
    }
    let f = MailDeliveryFailure { to, reason };
    if let Ok(frame) = encode_mail_delivery_failure(&f) {
        let _ = peers.send_to(sender, frame);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frames::{encode_mail_send, MailSend};
    use crate::mbox::Mailbox;
    use crate::send::init;
    use classic_proto::{MboxId, NetId, NodeId};

    fn nid(n: u8) -> NodeId {
        NodeId([n; 16])
    }
    fn netid(node: u8, mbox: u64) -> NetId {
        NetId { node: nid(node), mbox: MboxId(mbox) }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inbound_mail_send_delivers_to_local_mailbox() {
        init(nid(1));
        let h = MboxHandler::new();
        let (id, mut rx) = Mailbox::new();
        let m = MailSend {
            from: netid(2, 1),
            to: NetId { node: nid(1), mbox: id },
            payload: b"crossover".to_vec(),
        };
        let frame = encode_mail_send(&m).unwrap();
        h.on_frame(nid(2), frame);
        let msg = rx.recv().await;
        assert_eq!(msg.as_deref(), Some(&b"crossover"[..]));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn inbound_mail_send_missing_mbox_optionally_sends_failure() {
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
        init(nid(1));
        let cap = Arc::new(CapturePeers { sent: Mutex::new(Vec::new()) });
        let h = MboxHandler::with_peers(cap.clone());

        let m = MailSend {
            from: netid(2, 99),
            to: netid(1, u64::MAX), // certainly unknown locally
            payload: b"miss".to_vec(),
        };
        let frame = encode_mail_send(&m).unwrap();
        h.on_frame(nid(2), frame);

        let sent = cap.sent.lock().unwrap();
        assert_eq!(sent.len(), 1);
        let (recipient, frame) = &sent[0];
        assert_eq!(*recipient, nid(2)); // bounce to the from-node
        // Decode and verify.
        match decode_mbox_frame(frame).unwrap() {
            MboxInbound::MailDeliveryFailure(f) => {
                assert_eq!(f.reason, DeliveryFailureReason::UnknownMbox);
                assert_eq!(f.to, netid(1, u64::MAX));
            }
            other => panic!("expected MailDeliveryFailure, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn unknown_kind_in_mbox_range_dropped_no_panic() {
        let h = MboxHandler::new();
        // 0x0242 is in the mbox range but not assigned.
        h.on_frame(nid(2), Frame::new(0x0242, bytes::Bytes::new()));
    }
}
