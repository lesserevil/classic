//! Plan-05 integration test: in-process mail_send → Mailbox round-trip.

mod common;

use std::time::Duration;

use classic_mbox::{init, mail_send, Mailbox};
use classic_proto::NetId;

#[tokio::test(flavor = "current_thread")]
async fn local_mail_send_round_trips() {
    let _g = common::IT_MUTEX.lock().unwrap();
    init(common::nid(1));

    let (mbox, mut rx) = Mailbox::new();
    mail_send(
        NetId { node: common::nid(1), mbox },
        b"plan-05 hello".to_vec(),
    )
    .await
    .unwrap();

    let msg = tokio::time::timeout(Duration::from_millis(100), rx.recv())
        .await
        .expect("recv timed out")
        .expect("channel closed unexpectedly");
    assert_eq!(&msg, b"plan-05 hello");
}

#[tokio::test(flavor = "current_thread")]
async fn local_mail_send_to_dropped_receiver_is_silent() {
    let _g = common::IT_MUTEX.lock().unwrap();
    init(common::nid(1));

    let (mbox, recv) = Mailbox::new();
    drop(recv);

    // Fire-and-forget: no error returned; the message is dropped with
    // a tracing::warn and the LOCAL_MISSING_DROPS counter is bumped.
    mail_send(NetId { node: common::nid(1), mbox }, b"into-void".to_vec())
        .await
        .unwrap();
}
