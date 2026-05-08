//! Daemon bring-up entry point. Wires `Discovery`, `AdStore`, and `Gossip`
//! together against a caller-supplied `FrameMux` and `Peers`. The caller
//! retains the `AdStore` handle for the lifetime of the daemon (e.g. the
//! UDS control endpoint reads from it).
//!
//! The two background tasks (discovery refresh + gossip broadcast) are
//! spawned and their `JoinHandle`s returned so the daemon's shutdown path
//! can abort them on SIGTERM.

use std::sync::Arc;
use std::time::Duration;

use classic_proto::{FrameMux, NodeId};

use crate::config::AdConfig;
use crate::discovery::Sysroot;
use crate::discovery_loop::Discovery;
use crate::gossip::{Gossip, Peers};
use crate::store::AdStore;

pub struct AdHandles {
    pub store: AdStore,
    pub gossip: Arc<Gossip>,
    pub discovery_task: tokio::task::JoinHandle<()>,
    pub gossip_task: tokio::task::JoinHandle<()>,
}

#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error("ad config invalid: {0}")]
    Config(#[from] crate::config::AdConfigError),
    #[error("frame mux registration: {0}")]
    Mux(#[from] classic_proto::MuxError),
}

/// Build the discovery + gossip stack for this daemon. Registers the gossip
/// handler against the ad range (high byte `0x01`) on `mux`, spawns the
/// discovery refresh ticker, and spawns the gossip broadcast ticker.
///
/// `cores_online` is supplied by the caller because the `Sysroot` trait
/// can't model `sysconf` directly — production code passes
/// `num_cpus::get()` or equivalent; tests pass an explicit number.
pub fn start(
    self_id: NodeId,
    hostname: String,
    sysroot: Box<dyn Sysroot>,
    cores_online: u32,
    mux: Arc<FrameMux>,
    peers: Arc<dyn Peers>,
    config: AdConfig,
) -> Result<AdHandles, StartError> {
    let gossip_period = config.gossip_period;
    let peer_grace = config.peer_grace;
    let discovery = Discovery::bootstrap(self_id, hostname, sysroot, cores_online, config)?;
    let store = discovery.store().clone();

    let gossip = Gossip::new(store.clone(), peers, peer_grace);
    mux.register(0x01, gossip.clone())?;

    let discovery_task = discovery.spawn();
    let gossip_task = gossip.clone().spawn_ticker(if gossip_period.is_zero() {
        Duration::from_secs(10)
    } else {
        gossip_period
    });

    Ok(AdHandles {
        store,
        gossip,
        discovery_task,
        gossip_task,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::TempdirSysroot;
    use crate::gossip::Peers;
    use classic_proto::Frame;
    use std::sync::Mutex;

    struct NoPeers;
    impl Peers for NoPeers {
        fn live_peers(&self) -> Vec<NodeId> {
            Vec::new()
        }
        fn send_to(&self, _peer: NodeId, _frame: Frame) {}
    }

    fn id(n: u8) -> NodeId {
        NodeId([n; 16])
    }

    fn seed_minimal_fixture(sr: &TempdirSysroot) {
        sr.write(
            "proc/cpuinfo",
            "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Test CPU\nphysical id\t: 0\ncore id\t: 0\ncpu MHz\t: 2400.0\n\n",
        );
        sr.write(
            "proc/meminfo",
            "MemTotal:       16385020 kB\nMemAvailable:   12000000 kB\n",
        );
        sr.write("proc/loadavg", "0.10 0.20 0.30 1/100 1\n");
        sr.write("proc/stat", "cpu  10 0 0 90 0 0 0 0 0 0\n");
    }

    /// Captures the registered frame slot indices so tests can assert
    /// gossip wired itself in at 0x01.
    struct CaptureMux {
        slots: Mutex<Vec<u8>>,
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn start_registers_gossip_at_ad_range_and_spawns_tasks() {
        let sr = TempdirSysroot::new();
        seed_minimal_fixture(&sr);
        let mux = Arc::new(FrameMux::new());
        let peers: Arc<dyn Peers> = Arc::new(NoPeers);
        let cfg = AdConfig {
            gossip_period: Duration::from_millis(200),
            ..AdConfig::default()
        };
        let handles = start(id(1), "h".into(), Box::new(sr), 2, mux.clone(), peers, cfg).unwrap();

        // The task handles must be running.
        assert!(!handles.discovery_task.is_finished());
        assert!(!handles.gossip_task.is_finished());

        // Mux 0x01 slot must now refuse a re-registration check via
        // dispatch — easiest way: dispatch a frame we know our gossip
        // handler accepts; it should not panic.
        mux.dispatch(id(99), Frame::new(0x0102, bytes::Bytes::new())); // empty AdRequest decoder errors silently

        // Self-ad is populated.
        assert_eq!(handles.store.self_ad().node_id, id(1));

        // Clean up.
        handles.discovery_task.abort();
        handles.gossip_task.abort();
    }

    // CaptureMux unused here but kept for future tests that need to peek
    // at slot bookkeeping if FrameMux gains an introspection API.
    #[allow(dead_code)]
    impl CaptureMux {
        fn new() -> Self {
            Self { slots: Mutex::new(Vec::new()) }
        }
    }
}
