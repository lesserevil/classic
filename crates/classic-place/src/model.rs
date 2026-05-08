//! In-DSL representation of a node ad. The plan-03 grammar references
//! field names (`cpu.cores`, `gpu.in_use`, `gpu.mig`, `load.load_1m`)
//! that don't all exist in plan-02's `classic_ad::NodeAd`. Rather than
//! deform either side, classic-place owns this idealized struct and
//! callers are expected to project from real ads when they have one.
//!
//! See plan-03 §"Data shapes" for the source of truth.

use std::collections::BTreeMap;

use classic_proto::NodeId;

#[derive(Clone, Debug)]
pub struct NodeAd {
    pub node_id: NodeId,
    pub hostname: String,
    pub gen: u64,
    pub os: OsAd,
    pub cpu: CpuAd,
    pub mem: MemAd,
    pub load: LoadAd,
    pub gpu: Vec<GpuAd>,
    pub pci: Vec<PciAd>,
    pub labels: BTreeMap<String, String>,
}

impl Default for NodeAd {
    fn default() -> Self {
        Self {
            node_id: NodeId([0u8; 16]),
            hostname: String::new(),
            gen: 0,
            os: OsAd::default(),
            cpu: CpuAd::default(),
            mem: MemAd::default(),
            load: LoadAd::default(),
            gpu: Vec::new(),
            pci: Vec::new(),
            labels: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct OsAd {
    pub kernel: String,
    pub distro: String,
}

#[derive(Clone, Debug, Default)]
pub struct CpuAd {
    pub cores: u32,
    pub threads: u32,
    pub arch: String,
    pub model: String,
}

#[derive(Clone, Debug, Default)]
pub struct MemAd {
    pub total_mb: u64,
    pub free_mb: u64,
}

#[derive(Clone, Debug, Default)]
pub struct LoadAd {
    pub cpu_pct: f64,
    pub mem_pct: f64,
    pub load_1m: f64,
}

#[derive(Clone, Debug, Default)]
pub struct GpuAd {
    pub index: u32,
    pub vendor: u32,
    pub device: u32,
    pub model: String,
    pub vram_mb: u64,
    pub vram_free_mb: u64,
    pub sm_count: u32,
    pub in_use: bool,
    pub mig: bool,
}

#[derive(Clone, Debug, Default)]
pub struct PciAd {
    pub bdf: String,
    pub vendor: u32,
    pub device: u32,
    pub class: u32,
}
