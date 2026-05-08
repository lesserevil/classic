//! Discovery scheduler: two tickers running on a single tokio task.
//!
//! Fast tick (default 1 s): LoadSample, GPU dynamic fields,
//! MemAvailable, task_count.
//! Slow tick (default 60 s) + startup: full PCI / NUMA / CPU / GPU list
//! re-enumeration.
//!
//! `generation` only increments when the assembled `NodeAd` actually
//! differs from the one currently in the store, so unchanged refresh ticks
//! cost a comparison and nothing more — watchers stay quiet (FR-10).

use std::sync::{Arc, Mutex};
use std::time::Duration;

use classic_proto::{NodeId, PROTO_VERSION};

use crate::config::AdConfig;
use crate::discovery::cpu::probe as probe_cpu;
use crate::discovery::gpu::GpuProbe;
use crate::discovery::load::LoadProbe;
use crate::discovery::mem::probe as probe_mem;
use crate::discovery::numa::probe as probe_numa;
use crate::discovery::pci::probe as probe_pci;
use crate::discovery::Sysroot;
use crate::schema::{LoadSample, NodeAd};
use crate::store::AdStore;

pub struct Discovery {
    inner: Arc<DiscoveryInner>,
}

struct DiscoveryInner {
    sysroot: Box<dyn Sysroot>,
    gpu_probe: GpuProbe,
    load_probe: Mutex<LoadProbe>,
    store: AdStore,
    self_id: NodeId,
    hostname: String,
    boot_time: u64,
    cores_online: u32,
    config: AdConfig,
}

impl Discovery {
    /// Run the synchronous initial probe pass, build the AdStore, and
    /// return both. Caller can then `spawn` the recurring loop. This is
    /// split out so a daemon's bring-up sequence has a populated AdStore
    /// before any peer can possibly connect.
    pub fn bootstrap(
        self_id: NodeId,
        hostname: String,
        sysroot: Box<dyn Sysroot>,
        cores_online: u32,
        config: AdConfig,
    ) -> Result<Self, crate::config::AdConfigError> {
        config.validate()?;
        let gpu_probe = GpuProbe::new();
        let load_probe = LoadProbe::new();
        let boot_time = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        let initial = build_ad(
            self_id,
            &hostname,
            boot_time,
            1,
            sysroot.as_ref(),
            &gpu_probe,
            &mut LoadProbe::new(), // first pass uses a fresh probe so cpu_pct=0
            cores_online,
            &config,
        );
        let store = AdStore::new(initial);

        Ok(Self {
            inner: Arc::new(DiscoveryInner {
                sysroot,
                gpu_probe,
                load_probe: Mutex::new(load_probe),
                store,
                self_id,
                hostname,
                boot_time,
                cores_online,
                config,
            }),
        })
    }

    pub fn store(&self) -> &AdStore {
        &self.inner.store
    }

    /// Single fast-tick pass; exposed for tests.
    pub fn refresh_dynamic(&self) -> bool {
        let inner = &self.inner;
        let mut load_probe = inner.load_probe.lock().expect("load_probe poisoned");
        let mut next = inner.store.self_ad();
        // Refresh dynamic fields only.
        next.mem = probe_mem(inner.sysroot.as_ref()).unwrap_or(next.mem);
        let task_count = inner
            .config
            .task_count_fn
            .as_ref()
            .map(|f| f())
            .unwrap_or(0);
        let mem_pct = if next.mem.total_mb == 0 {
            0
        } else {
            (((next.mem.total_mb - next.mem.available_mb.min(next.mem.total_mb)) * 100)
                / next.mem.total_mb) as u32
        };
        next.load = load_probe
            .sample(inner.sysroot.as_ref(), mem_pct, task_count)
            .unwrap_or_else(|_| zero_load(task_count));
        inner.gpu_probe.refresh_dynamic(&mut next.gpus);
        commit_if_changed(&inner.store, next)
    }

    /// Single slow-tick pass; exposed for tests.
    pub fn refresh_static(&self) -> bool {
        let inner = &self.inner;
        let mut next = inner.store.self_ad();
        next.cpu = probe_cpu(inner.sysroot.as_ref(), inner.cores_online).unwrap_or(next.cpu);
        next.pci = probe_pci(inner.sysroot.as_ref()).unwrap_or(next.pci);
        next.numa = probe_numa(inner.sysroot.as_ref()).unwrap_or(next.numa);
        next.gpus = inner.gpu_probe.enumerate();
        commit_if_changed(&inner.store, next)
    }

    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        let inner = self.inner.clone();
        tokio::spawn(async move {
            let mut fast = tokio::time::interval(inner.config.fast_tick);
            let mut slow = tokio::time::interval(inner.config.slow_tick);
            // Discard the immediate ticks both interval timers fire at t=0;
            // the bootstrap pass already populated the store.
            fast.tick().await;
            slow.tick().await;
            let me = Discovery { inner: inner.clone() };
            loop {
                tokio::select! {
                    _ = fast.tick() => { me.refresh_dynamic(); }
                    _ = slow.tick() => { me.refresh_static(); }
                }
            }
        })
    }
}

fn build_ad(
    self_id: NodeId,
    hostname: &str,
    boot_time: u64,
    generation: u64,
    sysroot: &dyn Sysroot,
    gpu_probe: &GpuProbe,
    load_probe: &mut LoadProbe,
    cores_online: u32,
    config: &AdConfig,
) -> NodeAd {
    let cpu = probe_cpu(sysroot, cores_online).unwrap_or_else(|_| empty_cpu(cores_online));
    let mem = probe_mem(sysroot).unwrap_or_else(|_| crate::schema::MemInfo {
        total_mb: 0,
        available_mb: 0,
    });
    let pci = probe_pci(sysroot).unwrap_or_default();
    let numa = probe_numa(sysroot).unwrap_or_default();
    let gpus = gpu_probe.enumerate();
    let task_count = config.task_count_fn.as_ref().map(|f| f()).unwrap_or(0);
    let mem_pct = if mem.total_mb == 0 {
        0
    } else {
        (((mem.total_mb - mem.available_mb.min(mem.total_mb)) * 100) / mem.total_mb) as u32
    };
    let load = load_probe
        .sample(sysroot, mem_pct, task_count)
        .unwrap_or_else(|_| zero_load(task_count));

    NodeAd {
        node_id: self_id,
        hostname: hostname.to_string(),
        proto_version: PROTO_VERSION,
        generation,
        boot_time,
        cpu,
        mem,
        gpus,
        pci,
        numa,
        load,
    }
}

fn empty_cpu(cores_online: u32) -> crate::schema::CpuInfo {
    crate::schema::CpuInfo {
        cores_online,
        cores_physical: cores_online,
        sockets: 1,
        model: String::new(),
        vendor: String::new(),
        arch: std::env::consts::ARCH.to_string(),
        mhz: 0,
    }
}

fn zero_load(task_count: u32) -> LoadSample {
    LoadSample {
        loadavg_1m: 0,
        loadavg_5m: 0,
        loadavg_15m: 0,
        cpu_pct: 0,
        mem_pct: 0,
        task_count,
    }
}

/// Compare `next` against the current self_ad ignoring the `generation`
/// field. If anything else changed, bump generation and write; return true.
/// Otherwise leave the store alone and return false.
fn commit_if_changed(store: &AdStore, mut next: NodeAd) -> bool {
    let prev = store.self_ad();
    let mut prev_for_compare = prev.clone();
    prev_for_compare.generation = 0;
    let mut next_for_compare = next.clone();
    next_for_compare.generation = 0;
    if prev_for_compare == next_for_compare {
        return false;
    }
    next.generation = prev.generation.saturating_add(1);
    store.update_self(next);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::TempdirSysroot;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn seed_minimal_fixture(sr: &TempdirSysroot) {
        sr.write(
            "proc/cpuinfo",
            "processor\t: 0\nvendor_id\t: GenuineIntel\nmodel name\t: Test CPU\nphysical id\t: 0\ncore id\t: 0\ncpu MHz\t: 2400.0\n\n",
        );
        sr.write(
            "proc/meminfo",
            "MemTotal:       16385020 kB\nMemAvailable:   12000000 kB\nMemFree: 8000000 kB\nBuffers: 200000 kB\nCached: 3000000 kB\n",
        );
        sr.write("proc/loadavg", "0.10 0.20 0.30 1/100 1\n");
        sr.write("proc/stat", "cpu  10 0 0 90 0 0 0 0 0 0\n");
    }

    fn id(n: u8) -> NodeId {
        NodeId([n; 16])
    }

    #[tokio::test]
    async fn bootstrap_populates_self_ad_quickly() {
        let sr = TempdirSysroot::new();
        seed_minimal_fixture(&sr);
        let start = std::time::Instant::now();
        let disc = Discovery::bootstrap(
            id(1),
            "host-a".into(),
            Box::new(sr),
            2,
            AdConfig::default(),
        )
        .unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(500),
            "bootstrap took {elapsed:?}, expected < 500ms"
        );
        let ad = disc.store().self_ad();
        assert_eq!(ad.node_id, id(1));
        assert_eq!(ad.hostname, "host-a");
        assert!(ad.cpu.cores_online >= 1);
        assert!(ad.mem.total_mb > 0);
    }

    #[tokio::test]
    async fn slow_tick_no_change_keeps_generation() {
        let sr = TempdirSysroot::new();
        seed_minimal_fixture(&sr);
        let disc = Discovery::bootstrap(
            id(1),
            "h".into(),
            Box::new(sr),
            2,
            AdConfig::default(),
        )
        .unwrap();
        let g0 = disc.store().self_ad().generation;
        let changed = disc.refresh_static();
        assert!(!changed, "no fixture change should not bump generation");
        assert_eq!(disc.store().self_ad().generation, g0);
    }

    #[tokio::test]
    async fn slow_tick_pci_change_bumps_generation() {
        let sr = TempdirSysroot::new();
        seed_minimal_fixture(&sr);
        // Initial: no PCI devices.
        let disc = Discovery::bootstrap(
            id(1),
            "h".into(),
            Box::new(sr),
            2,
            AdConfig::default(),
        )
        .unwrap();
        let g0 = disc.store().self_ad().generation;

        // Add a PCI device into the *same* sysroot tempdir held by Discovery.
        // The Box<dyn Sysroot> in DiscoveryInner is the only owner; we cannot
        // mutate from outside. So we re-bootstrap with a writable tempdir
        // owned by us:
        let sr2 = TempdirSysroot::new();
        seed_minimal_fixture(&sr2);
        let disc2 = Discovery::bootstrap(
            id(2),
            "h".into(),
            Box::new(SharedSysroot {
                root: sr2.path().to_path_buf(),
            }),
            2,
            AdConfig::default(),
        )
        .unwrap();
        let g0_2 = disc2.store().self_ad().generation;
        // Mutate the underlying tempdir so the next slow-tick sees a new device.
        sr2.write(
            "sys/bus/pci/devices/0000:00:00.0/vendor",
            "0x8086\n",
        );
        sr2.write("sys/bus/pci/devices/0000:00:00.0/device", "0x3E1F\n");
        sr2.write("sys/bus/pci/devices/0000:00:00.0/class", "0x060000\n");
        sr2.write("sys/bus/pci/devices/0000:00:00.0/numa_node", "-1\n");

        let changed = disc2.refresh_static();
        assert!(changed, "PCI change should bump generation");
        assert!(disc2.store().self_ad().generation > g0_2);

        // Sanity: original disc unchanged.
        assert_eq!(disc.store().self_ad().generation, g0);
    }

    #[tokio::test]
    async fn fast_tick_invokes_task_count_callback() {
        let sr = TempdirSysroot::new();
        seed_minimal_fixture(&sr);
        let counter = Arc::new(AtomicU32::new(7));
        let counter_clone = counter.clone();
        let cfg = AdConfig {
            task_count_fn: Some(Arc::new(move || counter_clone.load(Ordering::Relaxed))),
            ..AdConfig::default()
        };
        let disc = Discovery::bootstrap(id(1), "h".into(), Box::new(sr), 2, cfg).unwrap();
        let _ = disc.refresh_dynamic();
        assert_eq!(disc.store().self_ad().load.task_count, 7);
        counter.store(11, Ordering::Relaxed);
        let _ = disc.refresh_dynamic();
        assert_eq!(disc.store().self_ad().load.task_count, 11);
    }

    #[tokio::test]
    async fn invalid_config_rejected() {
        let sr = TempdirSysroot::new();
        seed_minimal_fixture(&sr);
        let cfg = AdConfig {
            fast_tick: Duration::from_millis(50),
            ..AdConfig::default()
        };
        assert!(Discovery::bootstrap(id(1), "h".into(), Box::new(sr), 2, cfg).is_err());
    }

    /// Test-only Sysroot that delegates to a fixed path. Lets us mutate
    /// the fixture after Discovery has captured it.
    struct SharedSysroot {
        root: std::path::PathBuf,
    }
    impl Sysroot for SharedSysroot {
        fn read(&self, rel: &std::path::Path) -> std::io::Result<Vec<u8>> {
            std::fs::read(self.root.join(rel))
        }
        fn read_link(&self, rel: &std::path::Path) -> std::io::Result<std::path::PathBuf> {
            std::fs::read_link(self.root.join(rel))
        }
        fn read_dir(&self, rel: &std::path::Path) -> std::io::Result<Vec<String>> {
            let mut out = Vec::new();
            for entry in std::fs::read_dir(self.root.join(rel))? {
                let e = entry?;
                if let Some(s) = e.file_name().to_str() {
                    out.push(s.to_string());
                }
            }
            Ok(out)
        }
    }
}
