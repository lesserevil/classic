//! Adapters that bridge `classic-node`'s `PeerMesh` with `classic-ad`'s
//! gossip layer. Live here rather than in either of those crates to keep
//! their dependency edges acyclic.

use std::sync::Arc;

use classic_proto::{Frame, NodeId};

use classic_ad::{Gossip, Peers};

use crate::mesh::{LinkListener, PeerMesh};

/// `Peers` backend that delegates to a `PeerMesh`.
pub struct PeerMeshSink {
    mesh: Arc<PeerMesh>,
}

impl PeerMeshSink {
    pub fn new(mesh: Arc<PeerMesh>) -> Self {
        Self { mesh }
    }
}

impl Peers for PeerMeshSink {
    fn live_peers(&self) -> Vec<NodeId> {
        self.mesh.live_peers()
    }
    fn send_to(&self, peer: NodeId, frame: Frame) {
        self.mesh.send_to(peer, frame);
    }
}

/// Wires `Gossip`'s `on_peer_up` / `on_peer_down` hooks to `PeerMesh`'s
/// link-listener trait so the ad subsystem reacts to mesh events without
/// any polling.
pub struct GossipLinkListener {
    gossip: Arc<Gossip>,
}

impl GossipLinkListener {
    pub fn new(gossip: Arc<Gossip>) -> Self {
        Self { gossip }
    }
}

impl LinkListener for GossipLinkListener {
    fn on_peer_up(&self, peer: NodeId) {
        self.gossip.on_peer_up(peer);
    }
    fn on_peer_down(&self, peer: NodeId) {
        self.gossip.on_peer_down(peer);
    }
}
