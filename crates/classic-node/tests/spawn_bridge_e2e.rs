//! End-to-end test: bring up a daemon, connect to its spawn UDS, send
//! a SpawnRequest for `/bin/echo hello`, and assert the daemon streams
//! back ChildStdio + ChildExit{0}.

use std::time::Duration;

use classic_ad::AdConfig;
use classic_node::{spawn_node_with_ad_config, Config, LinkRuntimeConfig, NodeConfig};
use classic_proto::{
    decode_frame, decode_payload, encode_frame, encode_payload, ChildExit, ChildStdio, Frame,
    FrameKind, SpawnRequest, StdioStream,
};
use tempfile::TempDir;
use tokio::net::UnixStream;

fn cfg(state_dir: std::path::PathBuf, listen_addr: &str, peers: Vec<String>) -> Config {
    Config {
        node: NodeConfig {
            listen_addr: listen_addr.to_string(),
            state_dir,
            peers,
        },
        log: Default::default(),
    }
}

fn fast_runtime() -> LinkRuntimeConfig {
    LinkRuntimeConfig {
        heartbeat_period: Duration::from_millis(100),
        miss_threshold: 6,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn spawn_bridge_echo_round_trip() {
    let dir = TempDir::new().unwrap();
    let handle = spawn_node_with_ad_config(
        cfg(dir.path().join("state"), "127.0.0.1:0", vec![]),
        fast_runtime(),
        AdConfig::default(),
    )
    .await
    .unwrap();

    // The spawn UDS lives at <state_dir>/spawn.sock.
    let sock = dir.path().join("state").join("spawn.sock");
    // The daemon's bind happens during spawn_node_with_ad_config, so by
    // the time we get the handle the socket exists.
    let mut stream = UnixStream::connect(&sock)
        .await
        .expect("connect to spawn socket");

    // Send a SpawnRequest for `/bin/echo hello`.
    let req = SpawnRequest {
        req_id: 7,
        requires: "true".into(),
        rank: "".into(),
        argv: vec!["/bin/echo".into(), "hello".into()],
        env: vec![],
        exclusive_device: false,
        stdin_kind: None,
        hop: 0,
    };
    let body = encode_payload(&req).unwrap();
    let frame = Frame::new(FrameKind::SpawnRequest as u16, body.into());
    let (read, write) = (&mut stream).split();
    let mut read = read;
    let mut write = write;
    encode_frame(&mut write, &frame).await.unwrap();

    // Collect frames until ChildExit.
    let mut stdout_bytes = Vec::new();
    let mut got_ack = false;
    let mut exit_code: Option<i32> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let read_fut = decode_frame(&mut read);
        let frame =
            match tokio::time::timeout_at(deadline, read_fut).await {
                Ok(Ok(f)) => f,
                Ok(Err(_)) => break,
                Err(_) => panic!("timed out waiting for ChildExit"),
            };
        match frame.kind {
            k if k == FrameKind::SpawnAck as u16 => got_ack = true,
            k if k == FrameKind::ChildStdio as u16 => {
                let cs: ChildStdio = decode_payload(&frame.payload).unwrap();
                if matches!(cs.stream, StdioStream::Stdout) {
                    stdout_bytes.extend(cs.data);
                }
            }
            k if k == FrameKind::ChildExit as u16 => {
                let ex: ChildExit = decode_payload(&frame.payload).unwrap();
                exit_code = ex.code;
                break;
            }
            _ => {}
        }
    }

    assert!(got_ack, "daemon did not send SpawnAck");
    assert_eq!(stdout_bytes, b"hello\n", "stdout = {stdout_bytes:?}");
    assert_eq!(exit_code, Some(0));

    handle.shutdown(Duration::from_millis(50)).await;
}
