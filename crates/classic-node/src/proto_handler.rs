//! `FrameHandler` for the proto range (`0x0000..=0x00FF`).
//!
//! One `ProtoHandler` is shared by every `PeerLink`. Links register a
//! per-peer `mpsc::Sender<ProtoSignal>` so the handler can push lifecycle
//! events (peer-initiated Bye, fatal Error) back to the right link without
//! taking any global mutex on the dispatch path.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use tokio::sync::mpsc;
use tracing::{debug, warn};

use classic_proto::{decode_payload, ErrorPayload, Frame, FrameHandler, FrameKind, NodeId};

/// Lifecycle signal a link must act on, surfaced from a proto-range frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtoSignal {
    /// Peer sent Bye — initiate clean close.
    PeerBye,
    /// Peer sent Error with a code we treat as fatal — close with PeerError.
    PeerError(ErrorPayload),
}

pub struct ProtoHandler {
    routes: RwLock<HashMap<NodeId, mpsc::Sender<ProtoSignal>>>,
}

impl ProtoHandler {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { routes: RwLock::new(HashMap::new()) })
    }

    /// Wire a per-peer signal sender. Replaces any prior registration for
    /// the same peer.
    pub fn register(&self, peer: NodeId, tx: mpsc::Sender<ProtoSignal>) {
        self.routes
            .write()
            .expect("ProtoHandler routes poisoned")
            .insert(peer, tx);
    }

    pub fn deregister(&self, peer: NodeId) {
        self.routes
            .write()
            .expect("ProtoHandler routes poisoned")
            .remove(&peer);
    }

    /// Test-only inspection: how many peers are currently routed.
    #[cfg(test)]
    pub fn route_count(&self) -> usize {
        self.routes.read().unwrap().len()
    }

    fn signal(&self, peer: NodeId, sig: ProtoSignal) {
        let tx = {
            let routes = self.routes.read().expect("ProtoHandler routes poisoned");
            routes.get(&peer).cloned()
        };
        match tx {
            Some(tx) => {
                if tx.try_send(sig.clone()).is_err() {
                    // Receiver gone or channel full. The latter is a bug
                    // (the link drains promptly), the former a benign race
                    // with link teardown. Log and drop.
                    warn!(peer = %peer, "proto signal {:?} could not be delivered", sig);
                }
            }
            None => debug!(peer = %peer, "proto signal {:?} for unknown peer; dropped", sig),
        }
    }
}

impl FrameHandler for ProtoHandler {
    fn on_frame(&self, peer: NodeId, frame: Frame) {
        match frame.kind {
            k if k == FrameKind::Heartbeat as u16 => {
                // Liveness reset is handled by the link's per-frame counter;
                // the proto handler observes nothing here.
            }
            k if k == FrameKind::Bye as u16 => self.signal(peer, ProtoSignal::PeerBye),
            k if k == FrameKind::Error as u16 => {
                let err: ErrorPayload = match decode_payload(&frame.payload) {
                    Ok(e) => e,
                    Err(_) => {
                        warn!(peer = %peer, "malformed Error payload; dropping");
                        return;
                    }
                };
                self.signal(peer, ProtoSignal::PeerError(err));
            }
            k if k == FrameKind::Hello as u16 => {
                // Hello after handshake completion is a protocol error in
                // v1 — but since closing here would race with the
                // peer-disconnect path, just log. The connection will be
                // dropped by the next transport error.
                warn!(peer = %peer, "received Hello after handshake; ignored");
            }
            other => debug!(peer = %peer, kind = format!("{:#06x}", other), "proto-range frame with unrecognized kind"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use classic_proto::{encode_payload, ErrorCode, HeartbeatPayload};

    fn id(n: u8) -> NodeId {
        NodeId([n; 16])
    }

    #[test]
    fn bye_routes_to_correct_peer() {
        let handler = ProtoHandler::new();
        let (tx_a, mut rx_a) = mpsc::channel(8);
        let (tx_b, mut rx_b) = mpsc::channel(8);
        handler.register(id(1), tx_a);
        handler.register(id(2), tx_b);
        handler.on_frame(id(1), Frame::new(FrameKind::Bye as u16, Bytes::new()));
        assert_eq!(rx_a.try_recv().unwrap(), ProtoSignal::PeerBye);
        assert!(rx_b.try_recv().is_err());
    }

    #[test]
    fn error_decoded_and_routed() {
        let handler = ProtoHandler::new();
        let (tx, mut rx) = mpsc::channel(8);
        handler.register(id(1), tx);
        let payload = encode_payload(&ErrorPayload {
            code: ErrorCode::ProtoVersionMismatch,
            message: "v1 vs v2".into(),
        })
        .unwrap();
        handler.on_frame(
            id(1),
            Frame::new(FrameKind::Error as u16, Bytes::from(payload)),
        );
        match rx.try_recv().unwrap() {
            ProtoSignal::PeerError(e) => assert_eq!(e.code, ErrorCode::ProtoVersionMismatch),
            other => panic!("expected PeerError, got {other:?}"),
        }
    }

    #[test]
    fn heartbeat_does_not_signal() {
        let handler = ProtoHandler::new();
        let (tx, mut rx) = mpsc::channel(8);
        handler.register(id(1), tx);
        let payload =
            encode_payload(&HeartbeatPayload { seq: 1, send_time_ns: 0 }).unwrap();
        handler.on_frame(
            id(1),
            Frame::new(FrameKind::Heartbeat as u16, Bytes::from(payload)),
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn malformed_error_payload_is_dropped() {
        let handler = ProtoHandler::new();
        let (tx, mut rx) = mpsc::channel(8);
        handler.register(id(1), tx);
        // Random bytes that won't decode as ErrorPayload.
        handler.on_frame(
            id(1),
            Frame::new(FrameKind::Error as u16, Bytes::from_static(&[0xFF; 3])),
        );
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn deregister_drops_route() {
        let handler = ProtoHandler::new();
        let (tx, _rx) = mpsc::channel(8);
        handler.register(id(1), tx);
        assert_eq!(handler.route_count(), 1);
        handler.deregister(id(1));
        assert_eq!(handler.route_count(), 0);
    }
}
