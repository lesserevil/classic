//! On-wire schema for `NodeAd` and the gossip envelope. Every struct here
//! derives `serde`, `bincode::Encode`, and `bincode::Decode` so the wire
//! format is the canonical bincode-v2 fixed-int LE encoding shared with
//! `classic-proto`.

use bincode::{Decode, Encode};
use serde::{Deserialize, Serialize};

use classic_proto::NodeId;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct NodeAd {
    pub node_id: NodeId,
    pub hostname: String,
    pub proto_version: u32,
    /// Monotonic per-(node_id, boot_time) version counter. Bumped on any
    /// substantive change to `self_ad`.
    pub generation: u64,
    /// Seconds since the Unix epoch at daemon start. Combined with
    /// `generation` to break ties in LWW so a daemon restart cannot
    /// "reset" its ad to an older state.
    pub boot_time: u64,
    pub cpu: CpuInfo,
    pub mem: MemInfo,
    pub gpus: Vec<GpuInfo>,
    pub pci: Vec<PciDevice>,
    pub numa: Vec<NumaNode>,
    pub load: LoadSample,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct CpuInfo {
    pub cores_online: u32,
    pub cores_physical: u32,
    pub sockets: u32,
    pub model: String,
    pub vendor: String,
    pub arch: String,
    pub mhz: u32,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MemInfo {
    pub total_mb: u64,
    pub available_mb: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct GpuInfo {
    pub index: u32,
    pub uuid: String,
    pub name: String,
    pub pci_vendor: u16,
    pub pci_device: u16,
    pub pci_addr: String,
    pub vram_total_mb: u64,
    pub vram_free_mb: u64,
    pub compute_capability: (u32, u32),
    pub nvlink_peers: Vec<String>,
    pub utilization_pct: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct PciDevice {
    pub addr: String,
    pub vendor: u16,
    pub device: u16,
    pub class: u32,
    /// `-1` means "no NUMA association" (single-socket boxes; some PCI
    /// switches expose this as `-1` in sysfs).
    pub numa_node: i32,
    pub iommu_group: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct NumaNode {
    pub id: u32,
    pub cpus: Vec<u32>,
    pub mem_total_mb: u64,
}

#[derive(Copy, Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct LoadSample {
    /// Load average × 1000 (so 1.5 -> 1500). Three fixed decimals avoids
    /// floats on the wire.
    pub loadavg_1m: u32,
    pub loadavg_5m: u32,
    pub loadavg_15m: u32,
    pub cpu_pct: u32,
    pub mem_pct: u32,
    pub task_count: u32,
}

/// Gossip envelope. v1 always sends `Full`; receivers must accept both
/// variants so future deltas are backwards-compatible at decode time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub enum AdGossip {
    Full(NodeAd),
    Delta { node_id: NodeId, generation: u64 },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct AdRequest {
    pub from: NodeId,
}

/// Local-only update stream. Never on the wire. Subscribers receive these
/// from `AdStore::watch()` (sibling task).
#[derive(Clone, Debug, PartialEq)]
pub enum AdUpdate {
    Inserted(NodeAd),
    Updated(NodeAd),
    Removed(NodeId),
}
