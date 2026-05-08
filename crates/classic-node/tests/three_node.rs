//! Three nodes form a triangle; each `mesh.snapshot()` reports two healthy
//! peers.

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
async fn triangle_each_node_has_two_healthy_peers() {
    let d_a = TempDir::new().unwrap();
    let d_b = TempDir::new().unwrap();
    let d_c = TempDir::new().unwrap();

    // Bring up A first so B/C can dial it; then B (so C can dial); then C.
    let a = spawn_node(
        cfg(d_a.path().join("state"), "127.0.0.1:0", vec![]),
        fast_runtime(),
    )
    .await
    .unwrap();
    let a_addr = format!("127.0.0.1:{}", a.listen_addr.port());

    let b = spawn_node(
        cfg(
            d_b.path().join("state"),
            "127.0.0.1:0",
            vec![a_addr.clone()],
        ),
        fast_runtime(),
    )
    .await
    .unwrap();
    let b_addr = format!("127.0.0.1:{}", b.listen_addr.port());

    let c = spawn_node(
        cfg(
            d_c.path().join("state"),
            "127.0.0.1:0",
            vec![a_addr.clone(), b_addr.clone()],
        ),
        fast_runtime(),
    )
    .await
    .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut converged = false;
    while std::time::Instant::now() < deadline {
        let healthy_count = |snap: &Vec<classic_node::PeerStatus>| {
            snap.iter().filter(|s| matches!(s.state, PeerState::Healthy)).count()
        };
        let a_h = healthy_count(&a.mesh.snapshot());
        let b_h = healthy_count(&b.mesh.snapshot());
        let c_h = healthy_count(&c.mesh.snapshot());
        if a_h == 2 && b_h == 2 && c_h == 2 {
            converged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(converged, "triangle did not converge to 2 healthy peers per node");

    a.shutdown(Duration::from_millis(50)).await;
    b.shutdown(Duration::from_millis(50)).await;
    c.shutdown(Duration::from_millis(50)).await;
}
