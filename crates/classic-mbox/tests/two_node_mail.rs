//! Plan-05 integration test: cross-node mail_send hands off a properly-
//! encoded MailSend frame to the registered Peers backend, which a
//! "B-side" decoder routes back to the local registry.

mod common;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use classic_mbox::{
    decode_mbox_frame, init, mail_send, set_gossip_sink, set_peers, Mailbox, MboxHandler,
    MboxInbound, Peers,
};
use classic_proto::{Frame, FrameHandler, NodeId};

/// A `Peers` impl that pushes outbound frames into a shared queue.
/// The other test "node" pulls from this queue and feeds frames into
/// its `MboxHandler`, simulating the wire round-trip without any real
/// sockets.
struct LoopbackPeers {
    queue: Arc<Mutex<Vec<(NodeId, Frame)>>>,
}

impl Peers for LoopbackPeers {
    fn send_to(&self, peer: NodeId, frame: Frame) -> bool {
        self.queue.lock().unwrap().push((peer, frame));
        true
    }
}

#[tokio::test(flavor = "current_thread")]
async fn two_node_mail_round_trips_via_frame() {
    let _g = common::IT_MUTEX.lock().unwrap();

    // Node A sends; node B receives.
    let node_a = common::nid(1);
    let node_b = common::nid(2);

    init(node_a);

    // Wire a capture-Peers backend for node A's send path.
    let outbound: Arc<Mutex<Vec<(NodeId, Frame)>>> = Arc::new(Mutex::new(Vec::new()));
    set_peers(Arc::new(LoopbackPeers { queue: outbound.clone() }));
    // Drop any prior gossip sink (other tests may have installed one).
    set_gossip_sink(|_| {});

    // Allocate the receiving mailbox in this process (treating it as
    // node B's mailbox table — the test is a single process simulating
    // two daemons).
    let (mbox_b, mut rx_b) = Mailbox::new();

    // From node A's view, the destination is "node B's mailbox".
    let dest = classic_proto::NetId { node: node_b, mbox: mbox_b };
    mail_send(dest, b"crossnode plan-05".to_vec())
        .await
        .unwrap();

    // The backend captured one frame headed to node B.
    let sent = outbound.lock().unwrap();
    assert_eq!(sent.len(), 1);
    let (peer, frame) = sent[0].clone();
    drop(sent);
    assert_eq!(peer, node_b);

    // Simulate node B receiving and routing it through MboxHandler.
    // Re-init the local NodeId as if it were node B before delivery,
    // so the handler's local-delivery branch fires.
    init(node_b);
    let handler = MboxHandler::new();
    handler.on_frame(node_a, frame.clone());

    let msg = tokio::time::timeout(Duration::from_millis(100), rx_b.recv())
        .await
        .expect("recv timed out")
        .expect("channel closed");
    assert_eq!(&msg, b"crossnode plan-05");

    // Sanity: the encoded frame really is a MailSend with the expected
    // fields, not just a coincidental byte sequence.
    match decode_mbox_frame(&frame).unwrap() {
        MboxInbound::MailSend(m) => {
            assert_eq!(m.from.node, node_a);
            assert_eq!(m.to, dest);
            assert_eq!(m.payload, b"crossnode plan-05");
        }
        other => panic!("expected MailSend, got {other:?}"),
    }
}
