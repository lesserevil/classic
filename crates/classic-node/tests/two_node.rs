//! End-to-end test: two `classic-node` instances on ephemeral 127.0.0.1
//! ports dial each other; both reach Healthy within 1 s and exchange at
//! least 2 heartbeats per the plan-01 acceptance criteria.

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
        miss_threshold: 6,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_nodes_become_healthy_and_exchange_heartbeats() {
    let dir_a = TempDir::new().unwrap();
    let dir_b = TempDir::new().unwrap();

    // Bring up A first with no peers; learn its port.
    let a = spawn_node(
        cfg(dir_a.path().join("state"), "127.0.0.1:0", vec![]),
        fast_runtime(),
    )
    .await
    .unwrap();
    let a_port = a.listen_addr.port();

    let b = spawn_node(
        cfg(
            dir_b.path().join("state"),
            "127.0.0.1:0",
            vec![format!("127.0.0.1:{a_port}")],
        ),
        fast_runtime(),
    )
    .await
    .unwrap();

    // Both nodes should report a healthy peer within 1 s.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut both_healthy = false;
    while std::time::Instant::now() < deadline {
        let a_status = a.mesh.snapshot();
        let b_status = b.mesh.snapshot();
        let a_healthy = a_status
            .iter()
            .any(|s| matches!(s.state, PeerState::Healthy) && s.node_id == b.self_id);
        let b_healthy = b_status
            .iter()
            .any(|s| matches!(s.state, PeerState::Healthy) && s.node_id == a.self_id);
        if a_healthy && b_healthy {
            both_healthy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(both_healthy, "both nodes did not reach Healthy within 2s");

    // Wait long enough for >= 2 heartbeats per side at 100 ms cadence.
    tokio::time::sleep(Duration::from_millis(350)).await;
    let a_status = a.mesh.snapshot();
    let b_status = b.mesh.snapshot();
    assert!(a_status.iter().all(|s| matches!(s.state, PeerState::Healthy)));
    assert!(b_status.iter().all(|s| matches!(s.state, PeerState::Healthy)));

    a.shutdown(Duration::from_millis(50)).await;
    b.shutdown(Duration::from_millis(50)).await;
}
