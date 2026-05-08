//! Gossip RX (FrameHandler) + TX (broadcaster) for the ad range.
//!
//! Both ends are decoupled from classic-node's `PeerMesh` via the `Peers`
//! trait so they can be unit-tested without spinning up the full daemon.
//! The daemon's wiring (sibling task classic-jf5) implements `Peers` for
//! the real `PeerMesh` and registers a `Gossip` instance with the
//! `FrameMux` at high byte `0x01`.

use std::sync::Arc;
use std::time::Duration;

use classic_proto::{Frame, FrameHandler, NodeId};
use tracing::{debug, warn};

use crate::frames::{decode_ad_frame, encode_ad_request, encode_node_ad, AdInbound};
use crate::store::AdStore;

/// Abstraction over the live-peer table. `PeerMesh` implements this in the
/// daemon; tests use an in-memory channel-backed mock.
pub trait Peers: Send + Sync + 'static {
    fn live_peers(&self) -> Vec<NodeId>;
    /// Fire-and-forget. Senders that can't accept a frame log + drop;
    /// gossip never blocks the supervisor.
    fn send_to(&self, peer: NodeId, frame: Frame);
}

#[derive(Clone)]
pub struct Gossip {
    store: AdStore,
    peers: Arc<dyn Peers>,
    peer_grace: Duration,
}

impl Gossip {
    pub fn new(store: AdStore, peers: Arc<dyn Peers>, peer_grace: Duration) -> Arc<Self> {
        Arc::new(Self { store, peers, peer_grace })
    }

    /// Broadcast our current self_ad to every live peer. Errors per-peer
    /// log + skip; the broadcast does not fail-fast.
    pub fn broadcast_self(&self) {
        let ad = self.store.self_ad();
        let frame = match encode_node_ad(&ad) {
            Ok(f) => f,
            Err(e) => {
                warn!(error = ?e, "failed to encode self_ad for broadcast");
                return;
            }
        };
        for peer in self.peers.live_peers() {
            self.peers.send_to(peer, frame.clone());
        }
    }

    /// Connection-up hook: unicast-send self_ad to the new peer (FR-7).
    pub fn on_peer_up(&self, peer: NodeId) {
        let ad = self.store.self_ad();
        if let Ok(frame) = encode_node_ad(&ad) {
            self.peers.send_to(peer, frame);
        }
    }

    /// Connection-down hook: schedule TTL eviction (FR-9).
    pub fn on_peer_down(&self, peer: NodeId) {
        self.store.mark_stale(peer, self.peer_grace);
    }

    /// Async task that broadcasts every `period`.
    pub fn spawn_ticker(self: Arc<Self>, period: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(period);
            interval.tick().await; // discard the immediate first tick
            loop {
                interval.tick().await;
                self.broadcast_self();
            }
        })
    }
}

impl FrameHandler for Gossip {
    fn on_frame(&self, peer: NodeId, frame: Frame) {
        let inbound = match decode_ad_frame(&frame) {
            Ok(v) => v,
            Err(e) => {
                debug!(?peer, error = ?e, "dropping ad frame");
                return;
            }
        };
        match inbound {
            AdInbound::Ad(ad) => {
                let _ = self.store.upsert(ad);
            }
            AdInbound::Delta { node_id: _, generation: _ } => {
                // v1 emitters never send Delta on the wire, but the schema
                // accepts it for forward compat. Without a backing store of
                // pending deltas we just ignore.
            }
            AdInbound::Request(req) => {
                let ad = self.store.self_ad();
                if let Ok(reply) = encode_node_ad(&ad) {
                    self.peers.send_to(req.from, reply);
                }
            }
        }
        // Also drop-handle: if the frame was a Request, the requester is
        // already known live. If it was an Ad, we received it via this peer
        // — so the peer must be alive; touch any pending eviction.
        let _ = peer; // reserved for future debugging
    }
}

/// Convenience: build an `encode_ad_request` frame for the given peer.
pub fn ad_request_frame(from: NodeId) -> Frame {
    encode_ad_request(&crate::schema::AdRequest { from }).expect("encode AdRequest")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{CpuInfo, LoadSample, MemInfo, NodeAd};
    use std::sync::Mutex;

    struct MockPeers {
        live: Mutex<Vec<NodeId>>,
        sent: Mutex<Vec<(NodeId, Frame)>>,
    }
    impl MockPeers {
        fn new(live: Vec<NodeId>) -> Arc<Self> {
            Arc::new(Self {
                live: Mutex::new(live),
                sent: Mutex::new(Vec::new()),
            })
        }
        fn drain_sent(&self) -> Vec<(NodeId, Frame)> {
            std::mem::take(&mut *self.sent.lock().unwrap())
        }
    }
    impl Peers for MockPeers {
        fn live_peers(&self) -> Vec<NodeId> {
            self.live.lock().unwrap().clone()
        }
        fn send_to(&self, peer: NodeId, frame: Frame) {
            self.sent.lock().unwrap().push((peer, frame));
        }
    }

    fn ad(node: NodeId, gen: u64) -> NodeAd {
        NodeAd {
            node_id: node,
            hostname: "h".into(),
            proto_version: 1,
            generation: gen,
            boot_time: 0,
            cpu: CpuInfo {
                cores_online: 1,
                cores_physical: 1,
                sockets: 1,
                model: "m".into(),
                vendor: "v".into(),
                arch: "x86_64".into(),
                mhz: 1,
            },
            mem: MemInfo { total_mb: 1, available_mb: 1 },
            gpus: vec![],
            pci: vec![],
            numa: vec![],
            load: LoadSample {
                loadavg_1m: 0,
                loadavg_5m: 0,
                loadavg_15m: 0,
                cpu_pct: 0,
                mem_pct: 0,
                task_count: 0,
            },
        }
    }

    fn id(n: u8) -> NodeId {
        NodeId([n; 16])
    }

    #[test]
    fn rx_inserts_ad_via_node_ad_frame() {
        let store = AdStore::new(ad(id(0), 1));
        let peers = MockPeers::new(vec![]);
        let g = Gossip::new(store.clone(), peers, Duration::from_secs(90));

        let inbound_ad = ad(id(1), 5);
        let frame = encode_node_ad(&inbound_ad).unwrap();
        g.on_frame(id(1), frame);

        assert_eq!(store.peer(id(1)).unwrap().generation, 5);
    }

    #[test]
    fn rx_inserts_ad_via_ad_gossip_full() {
        let store = AdStore::new(ad(id(0), 1));
        let peers = MockPeers::new(vec![]);
        let g = Gossip::new(store.clone(), peers, Duration::from_secs(90));

        let inbound_ad = ad(id(1), 7);
        let frame = crate::frames::encode_ad_gossip(&crate::schema::AdGossip::Full(inbound_ad)).unwrap();
        g.on_frame(id(1), frame);
        assert_eq!(store.peer(id(1)).unwrap().generation, 7);
    }

    #[test]
    fn lww_three_ads_keeps_highest_generation() {
        let store = AdStore::new(ad(id(0), 1));
        let peers = MockPeers::new(vec![]);
        let g = Gossip::new(store.clone(), peers, Duration::from_secs(90));

        for gen in [1u64, 3, 2] {
            let frame = encode_node_ad(&ad(id(1), gen)).unwrap();
            g.on_frame(id(1), frame);
        }
        assert_eq!(store.peer(id(1)).unwrap().generation, 3);
    }

    #[test]
    fn ad_request_handler_unicasts_self_ad() {
        let store = AdStore::new(ad(id(0), 42));
        let peers = MockPeers::new(vec![]);
        let g = Gossip::new(store.clone(), peers.clone(), Duration::from_secs(90));

        let req_frame = ad_request_frame(id(7));
        g.on_frame(id(7), req_frame);
        let sent = peers.drain_sent();
        assert_eq!(sent.len(), 1);
        let (recipient, frame) = sent.into_iter().next().unwrap();
        assert_eq!(recipient, id(7));
        match decode_ad_frame(&frame).unwrap() {
            AdInbound::Ad(reply) => assert_eq!(reply.generation, 42),
            other => panic!("expected Ad, got {other:?}"),
        }
    }

    #[test]
    fn broadcast_sends_to_every_live_peer() {
        let store = AdStore::new(ad(id(0), 1));
        let peers = MockPeers::new(vec![id(1), id(2), id(3)]);
        let g = Gossip::new(store, peers.clone(), Duration::from_secs(90));
        g.broadcast_self();
        let sent = peers.drain_sent();
        assert_eq!(sent.len(), 3);
        let recipients: std::collections::HashSet<_> = sent.into_iter().map(|(p, _)| p).collect();
        assert!(recipients.contains(&id(1)));
        assert!(recipients.contains(&id(2)));
        assert!(recipients.contains(&id(3)));
    }

    #[test]
    fn on_peer_up_unicasts_self_ad() {
        let store = AdStore::new(ad(id(0), 1));
        let peers = MockPeers::new(vec![]);
        let g = Gossip::new(store, peers.clone(), Duration::from_secs(90));
        g.on_peer_up(id(5));
        let sent = peers.drain_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, id(5));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn on_peer_down_schedules_eviction() {
        let store = AdStore::new(ad(id(0), 1));
        let peers = MockPeers::new(vec![]);
        let g = Gossip::new(store.clone(), peers, Duration::from_millis(80));
        // Pre-populate a peer ad so eviction has something to remove.
        store.upsert(ad(id(1), 1));
        assert!(store.peer(id(1)).is_some());
        g.on_peer_down(id(1));
        // Wait past peer_grace.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(store.peer(id(1)).is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fresh_ad_during_grace_cancels_eviction() {
        let store = AdStore::new(ad(id(0), 1));
        let peers = MockPeers::new(vec![]);
        let g = Gossip::new(store.clone(), peers, Duration::from_millis(200));
        store.upsert(ad(id(1), 1));
        g.on_peer_down(id(1));
        tokio::time::sleep(Duration::from_millis(40)).await;
        // Fresh ad before peer_grace expires.
        let frame = encode_node_ad(&ad(id(1), 2)).unwrap();
        g.on_frame(id(1), frame);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(store.peer(id(1)).is_some());
    }
}
