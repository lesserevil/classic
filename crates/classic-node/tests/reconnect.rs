//! Shut a peer down and bring it back up at the same listen address; the
//! survivor's dialer reconnects within the backoff window.

use std::time::Duration;

use classic_node::{spawn_node, Config, LinkRuntimeConfig, NodeConfig, PeerState};
use tempfile::TempDir;

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
        miss_threshold: 4,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconnect_after_peer_restart() {
    let d_survivor = TempDir::new().unwrap();
    let d_peer = TempDir::new().unwrap();

    // Survivor binds first so we can pin the port; the dial-target node
    // binds after on its own ephemeral port we'll capture for the dialer.
    let survivor = spawn_node(
        cfg(d_survivor.path().join("state"), "127.0.0.1:0", vec![]),
        fast_runtime(),
    )
    .await
    .unwrap();
    let survivor_addr = format!("127.0.0.1:{}", survivor.listen_addr.port());

    // Peer 1: dials the survivor. Bind a port we control so we can reuse
    // it after restart.
    let peer1 = spawn_node(
        cfg(
            d_peer.path().join("state"),
            "127.0.0.1:0",
            vec![survivor_addr.clone()],
        ),
        fast_runtime(),
    )
    .await
    .unwrap();
    let peer_listen_port = peer1.listen_addr.port();
    let peer1_id = peer1.self_id;

    // Wait for the link to come up.
    wait_for_healthy_peer(&survivor.mesh, peer1_id, Duration::from_secs(2)).await;

    // Tear down peer1.
    peer1.shutdown(Duration::from_millis(50)).await;
    drop(peer1);

    // The survivor side should observe the peer leaving; it might briefly
    // see Closed state or the entry might disappear. Either is acceptable
    // — what we assert is recovery on restart.

    // Restart peer at the same listen port (so survivor's stable identity
    // is preserved across restart through the persisted node_id) AND
    // configured to dial the survivor.
    let peer2 = spawn_node(
        cfg(
            d_peer.path().join("state"),
            &format!("127.0.0.1:{peer_listen_port}"),
            vec![survivor_addr.clone()],
        ),
        fast_runtime(),
    )
    .await
    .unwrap();

    // Within ~5 s the survivor should see the peer Healthy again under the
    // same NodeId (preserved via state_dir/node_id).
    wait_for_healthy_peer(&survivor.mesh, peer2.self_id, Duration::from_secs(5)).await;
    assert_eq!(peer1_id, peer2.self_id, "NodeId must persist across restart");

    survivor.shutdown(Duration::from_millis(50)).await;
    peer2.shutdown(Duration::from_millis(50)).await;
}

async fn wait_for_healthy_peer(
    mesh: &std::sync::Arc<classic_node::PeerMesh>,
    peer: classic_proto::NodeId,
    budget: Duration,
) {
    let deadline = std::time::Instant::now() + budget;
    while std::time::Instant::now() < deadline {
        if mesh
            .snapshot()
            .iter()
            .any(|s| s.node_id == peer && matches!(s.state, PeerState::Healthy))
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }
    let snap = mesh.snapshot();
    panic!("peer {:?} did not become Healthy within {:?}; snapshot={:?}", peer, budget, snap);
}
