//! End-to-end ad gossip: two daemons converge each other's NodeAd into the
//! local AdStore within a few gossip ticks.

use std::time::Duration;

use classic_ad::AdConfig;
use classic_node::{spawn_node_with_ad_config, Config, LinkRuntimeConfig, NodeConfig};
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

fn fast_ad() -> AdConfig {
    AdConfig {
        gossip_period: Duration::from_millis(150),
        ..AdConfig::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn ads_converge_between_two_nodes() {
    let da = TempDir::new().unwrap();
    let db = TempDir::new().unwrap();

    let a = spawn_node_with_ad_config(
        cfg(da.path().join("state"), "127.0.0.1:0", vec![]),
        fast_runtime(),
        fast_ad(),
    )
    .await
    .unwrap();
    let a_addr = format!("127.0.0.1:{}", a.listen_addr.port());

    let b = spawn_node_with_ad_config(
        cfg(db.path().join("state"), "127.0.0.1:0", vec![a_addr.clone()]),
        fast_runtime(),
        fast_ad(),
    )
    .await
    .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    let mut converged = false;
    while std::time::Instant::now() < deadline {
        let a_has_b = a
            .ad_store
            .all_ads()
            .iter()
            .any(|ad| ad.node_id == b.self_id);
        let b_has_a = b
            .ad_store
            .all_ads()
            .iter()
            .any(|ad| ad.node_id == a.self_id);
        if a_has_b && b_has_a {
            converged = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(converged, "ads did not converge within 3s");

    a.shutdown(Duration::from_millis(50)).await;
    b.shutdown(Duration::from_millis(50)).await;
}
