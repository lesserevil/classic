# Feature: Node Ad + Hardware Discovery (plan 02)

> **Status:** draft
> **Epic bead:** _filed after this doc lands_
> **Owner:** classic-ad
> **Last updated:** 2026-05-07

Plan 02 builds the `classic-ad` crate. It defines the `NodeAd` schema (the durable description of a node's identity, hardware, and current load), implements the discovery probes that populate it, gossips ads across the mesh, and exposes a small in-process API that plan 03 (predicates) and plan 04 (spawn) consume.

This doc relies on `plans/ARCHITECTURE.md` for cross-cutting decisions: identity types (`NodeId`), frame-format and frame-kind ranges, transport behavior, encoding (bincode v2), and the workspace layout. Anything covered there is **not** redefined here.

## Scope

### In scope

- New crate `crates/classic-ad/` — the only owner of `NodeAd` and gossip frames `0x0100–0x01FF`.
- `NodeAd` schema and derived per-component sub-schemas (`CpuInfo`, `MemInfo`, `GpuInfo`, `PciDevice`, `NumaNode`, `LoadSample`).
- Hardware discovery probes:
  - CPU summary from `/proc/cpuinfo` + `sysconf(_SC_NPROCESSORS_ONLN)`.
  - RAM from `/proc/meminfo` (`MemTotal`, `MemAvailable`).
  - PCI device enumeration from `/sys/bus/pci/devices/*`.
  - NUMA topology from `/sys/devices/system/node/`.
  - GPU enumeration via NVML (`nvml-wrapper` crate). Optional at runtime: a missing `libnvidia-ml.so.1` produces an empty `gpus` list and a single warning log.
  - Process load: `/proc/loadavg`, `/proc/meminfo`, child task count from the daemon's own bookkeeping.
- Periodic refresh:
  - Fast tick (1 s): `LoadSample`, per-GPU `vram_free_mb`, per-GPU `utilization_pct`, `MemAvailable`, `task_count`.
  - Slow tick (60 s) and once at startup: full hardware re-enumeration (PCI list, GPU list, NUMA, CPU info).
- Gossip protocol:
  - Each daemon broadcasts its full `NodeAd` on connection establishment and every 10 s thereafter.
  - Receivers cache peer ads in memory, keyed by `NodeId`.
  - On reconnect after a peer drop, the local store evicts that peer's cached ad after a 90 s grace.
  - Eventually consistent — there is no quorum, no Lamport clock, no anti-entropy beyond the periodic re-broadcast.
- Public API (in-process, used by plan 03/04 in the same daemon):
  - `AdStore::self_ad() -> NodeAd`
  - `AdStore::all_ads() -> Vec<NodeAd>` (own ad always included first)
  - `AdStore::watch() -> impl Stream<Item = AdUpdate>`
- Frame definitions in the `0x0100–0x01FF` range and their bincode v2 codecs.

### Out of scope

The following items are explicitly deferred to other plans or v2 — do **not** design them here:

- **Predicate language and matching** — plan 03. `classic-ad` exposes data only.
- **Spawn / placement decisions** — plan 04.
- **Mailbox and service directory** — plan 05.
- **Persistence of ads to disk.** v1 keeps ads in memory only. A daemon restart drops cached peer ads; they are repopulated on next gossip tick.
- **Authenticated gossip** — fully trusted cluster per ARCHITECTURE.md.
- **Ad signing / replay protection.**
- **Live migration of ads** between transports — gossip rides on whatever `classic-proto` provides.
- **Hot-plug GPU detection** below the 60 s slow-tick granularity.
- **Cross-node clock sync** — `boot_time` is reported as the local node's monotonic boot time in `u64` seconds since UNIX epoch and never compared across nodes.

## Reasoning

### Problem

The cluster's headline feature is hardware-aware spawn placement: a user declares `gpu.compute_capability >= 9.0 and gpu.vram_free_mb >= 40000` and the cluster picks a host. To do that, every daemon must know what every other daemon has.

We need a published, structured, machine-readable description of each node — its static hardware (PCI/GPU/NUMA/CPU) and its dynamic load (free VRAM, CPU%, task count). Plan 03's predicate matcher operates on this description; plan 04's spawn pipeline reads the matcher's output. Plan 02's job is to build the description and keep it fresh.

### Alternatives considered and rejected

1. **Centralized registry (one daemon collects, others query).** Rejected: introduces a single point of failure, contradicts ChrysaLisp's gossiped-directory pattern, and turns a 10 ms local lookup into a network round-trip during placement. Gossip is cheap at v1 cluster sizes (target: ≤256 nodes).
2. **Pull-only model (predicate evaluator queries each node on demand).** Rejected: spawn latency would scale with cluster size; we want sub-50 ms placement decisions per ARCHITECTURE.md non-functional targets. Push gossip lets every daemon have an up-to-date snapshot locally.
3. **Reuse Kubernetes node status / PCI-DB / hwloc XML.** Rejected: heavyweight dependencies for a small, fixed set of fields. We control the schema and want it to evolve with the predicate DSL (plan 03).
4. **Shell out to `lspci` / `nvidia-smi` and parse output.** Rejected: text parsing is fragile, requires those binaries on the host, and `nvidia-smi` startup latency (~200 ms) blows the 1 s fast-tick budget. NVML's library API is a binding away.
5. **Use a CRDT (LWW-set) for the ad store.** Rejected as over-engineered. Each `NodeAd` is owned by exactly one writer (the node it describes); last-writer-wins on `(node_id, generation)` is sufficient and trivial.

### Success criteria (plain English)

- Start three `classicd` daemons on three hosts. Within 15 s of the third joining, each daemon's `AdStore::all_ads()` returns three ads with hardware that matches `lspci` / `nvidia-smi` output on each host.
- Plug a USB device into one host. Within 60 s, the other two daemons see the new PCI device in that host's ad.
- Spawn a tight CPU loop on one host. Within 2 s, the other daemons' cached ad for that host shows the elevated `loadavg_1m`.
- Disable NVIDIA driver on one host (`rmmod nvidia`). That host's daemon starts cleanly, logs one warning, reports `gpus: []`.

## Design

### Architecture

`classic-ad` is a leaf crate above `classic-proto`. It is wired into `classicd` (the `classic-node` binary) by plan 01's startup glue. Its only consumers are `classic-place` (read-only) and the `classicd` binary itself (which runs the gossip task).

```
+-----------+      +-------------+      +------------+
| classicd  | <--> | classic-ad  | <--> | classic-   |
| (binary)  |      | (this crate)|      | proto      |
+-----------+      +-------------+      +------------+
                          ^
                          |
                   +------+------+
                   | classic-    |
                   | place       |  (read-only API)
                   +-------------+
```

Inside `classic-ad`, three subsystems run concurrently:

```
                 +--------------------+
                 |    AdStore         |
                 |  (Arc<RwLock>)     |
                 +--------------------+
                  ^         ^       ^
                  |         |       |
        +---------+         |       +----------+
        |                   |                  |
+---------------+  +----------------+  +----------------+
| Discovery     |  | Gossip RX      |  | Gossip TX      |
| (probes)      |  | (frame handler)|  | (broadcaster)  |
+---------------+  +----------------+  +----------------+
   |       |              |                   |
fast 1s slow 60s     classic-proto       classic-proto
                     conn registry       conn registry
```

**Discovery** owns probes; it writes only the *own-node* slot of `AdStore`. It runs two tickers (1 s and 60 s) on a single tokio task.

**Gossip RX** is a frame-handler trait impl registered with `classic-proto`'s mux. On each `NodeAd` or `AdGossip` frame it updates the corresponding peer slot in `AdStore` (LWW on `generation`).

**Gossip TX** runs a 10 s ticker and a hook on connection-up events. It reads the own-node ad from `AdStore` and broadcasts via `classic-proto` to all currently connected peers.

#### Sequence — bring-up + gossip

```
Time   classicd-A                          classicd-B
 0ms   start                               (already running)
       generate NodeId
       run discovery probes (≤200 ms)
 250ms publish self_ad to AdStore
 300ms TCP connect → B (classic-proto)
 310ms send Hello (proto frame range 0x00xx)
 320ms recv Hello from B
 320ms                                     accept connect
                                           send NodeAd[A=B's ad, gen=N]
 350ms recv NodeAd → AdStore.upsert(B)
 350ms send NodeAd[A=A's ad, gen=1]
 360ms                                     recv NodeAd → AdStore.upsert(A)
 ...
 10s   tick → broadcast NodeAd to B        tick → broadcast NodeAd to A
 ...
 60s   slow probe → re-enum PCI/GPU
       generation += 1
       publish to AdStore
 70s   gossip tick → broadcasts new ad
```

#### Sequence — peer drop

```
Time   classicd-A
 t      detect connection-down to B (classic-proto event)
 t      AdStore.mark_stale(B, ttl=90s)
 t+1s   gossip tick still has B in all_ads() (returned with stale=true flag elided)
 ...
 t+90s  AdStore.evict(B) if no fresh ad seen
        watch() emits AdUpdate::Removed(B)
```

### Data shapes

All structs derive `Clone, Debug, Serialize, Deserialize, bincode::Encode, bincode::Decode`. `NodeAd` itself also derives `PartialEq` for tests; the load-bearing equality is `node_id + generation`.

`bincode` v2 with the **fixed-int, little-endian** configuration matching the rest of `classic-proto`. Strings use `String` (length-prefixed UTF-8). Optional fields use `Option`. No untagged unions on the wire.

```rust
use classic_proto::NodeId;
use serde::{Deserialize, Serialize};
use bincode::{Encode, Decode};

/// A node's full advertisement. Owner-written; consumed read-only by everyone.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct NodeAd {
    /// Node identity. Stable across daemon restarts (persisted by classic-proto).
    pub node_id: NodeId,
    /// Hostname as reported by `gethostname(2)`. Informational only.
    pub hostname: String,
    /// Wire protocol version this node speaks. Mirrors classic_proto::PROTO_VERSION.
    pub proto_version: u32,
    /// Daemon-instance generation counter. Increments on every refresh that
    /// changes any field. Used as the LWW tiebreaker between gossip messages.
    pub generation: u64,
    /// Daemon boot time, seconds since UNIX epoch (CLOCK_REALTIME at startup).
    /// Allows readers to detect daemon restarts independent of generation.
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
    /// Online logical CPUs (sysconf(_SC_NPROCESSORS_ONLN)).
    pub cores_online: u32,
    /// Physical cores, deduplicated by (physical id, core id) from /proc/cpuinfo.
    pub cores_physical: u32,
    /// Sockets, deduplicated by physical id.
    pub sockets: u32,
    /// "model name" from /proc/cpuinfo (CPU 0).
    pub model: String,
    /// "vendor_id" from /proc/cpuinfo (CPU 0). e.g. "GenuineIntel", "AuthenticAMD".
    pub vendor: String,
    /// Host architecture string. "x86_64" | "aarch64" | "riscv64" | other.
    pub arch: String,
    /// Reported max frequency in MHz (CPU 0, "cpu MHz" rounded).
    pub mhz: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct MemInfo {
    /// MemTotal from /proc/meminfo, in MiB.
    pub total_mb: u64,
    /// MemAvailable from /proc/meminfo, in MiB. Refreshed on every fast tick.
    pub available_mb: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct GpuInfo {
    /// NVML index. Stable while the daemon is alive; not stable across restarts.
    pub index: u32,
    /// Vendor-stable identifier (NVML "GPU UUID", e.g. "GPU-1a2b...").
    pub uuid: String,
    /// Marketing name (e.g. "NVIDIA H100 80GB HBM3").
    pub name: String,
    /// PCI vendor id. NVIDIA = 0x10de.
    pub pci_vendor: u16,
    /// PCI device id. e.g. H100 PCIe = 0x2330.
    pub pci_device: u16,
    /// PCI bus address as "DDDD:BB:DD.F" (domain:bus:device.function).
    pub pci_addr: String,
    pub vram_total_mb: u64,
    pub vram_free_mb: u64,
    /// Compute capability, e.g. (9, 0) for Hopper.
    pub compute_capability: (u32, u32),
    /// NVLink peers — list of remote-end GPU UUIDs reachable via active NVLinks.
    pub nvlink_peers: Vec<String>,
    /// Current SM utilization 0..=100. Refreshed on every fast tick.
    pub utilization_pct: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct PciDevice {
    /// "DDDD:BB:DD.F".
    pub addr: String,
    pub vendor: u16,
    pub device: u16,
    /// 24-bit class code (class << 16 | subclass << 8 | prog_if). 0xRRSSPP.
    pub class: u32,
    /// NUMA node id, or -1 if unknown.
    pub numa_node: i32,
    /// IOMMU group id, or None if IOMMU is disabled.
    pub iommu_group: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct NumaNode {
    pub id: u32,
    /// CPUs in this node, derived from /sys/devices/system/node/nodeN/cpulist.
    pub cpus: Vec<u32>,
    /// Total memory on this node, MiB. From /sys/.../meminfo "MemTotal".
    pub mem_total_mb: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Encode, Decode)]
pub struct LoadSample {
    /// loadavg fields from /proc/loadavg, scaled x100 (1.27 -> 127). Avoids floats on the wire.
    pub loadavg_1m: u32,
    pub loadavg_5m: u32,
    pub loadavg_15m: u32,
    /// CPU% averaged over the last fast tick interval, 0..=100*cores_online.
    /// Computed from /proc/stat deltas.
    pub cpu_pct: u32,
    /// Memory used / total * 100.
    pub mem_pct: u32,
    /// Number of tasks (processes) currently spawned by this classicd instance.
    /// Bookkeeping comes from classic-spawn (plan 04). Reported as 0 until plan 04 wires it.
    pub task_count: u32,
}

/// Wrapper used on the wire when a node sends a delta-or-full ad. v1 always sends full.
#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub enum AdGossip {
    Full(NodeAd),
    /// Reserved for v2 deltas. Receivers MUST treat unknown variants as
    /// "ignore + drop connection" per classic-proto's strict-decode contract.
    Delta { node_id: NodeId, generation: u64 /* fields TBD */ },
}

/// Request a peer's current ad. Sent right after Hello, or on demand if a
/// receiver decoded a stale Delta with no Full base. v1 only sends after Hello.
#[derive(Clone, Debug, Serialize, Deserialize, Encode, Decode)]
pub struct AdRequest {
    pub from: NodeId,
}

/// Update event published by AdStore::watch().
#[derive(Clone, Debug)]
pub enum AdUpdate {
    Inserted(NodeAd),
    Updated(NodeAd),
    Removed(NodeId),
}
```

#### Frame-kind allocation (within the `0x0100–0x01FF` range)

| Code     | Name        | Payload type    | Direction   |
|----------|-------------|-----------------|-------------|
| `0x0100` | `NodeAd`    | `NodeAd`        | broadcast   |
| `0x0101` | `AdGossip`  | `AdGossip`      | broadcast   |
| `0x0102` | `AdRequest` | `AdRequest`     | unicast     |
| `0x0103` – `0x01FF` | reserved | — | — |

`NodeAd` and `AdGossip::Full(NodeAd)` carry the same payload; the two frame kinds exist so future deltas can ride `AdGossip` without breaking the simpler `NodeAd` decoder. v1 emitters MAY use either — receivers MUST accept both.

#### Concrete example ad (rendered as JSON for human readability; on the wire it is bincode v2)

```json
{
  "node_id": "0x9f3c0a1de2b14a8e8c2f5a0d7f1e4b21",
  "hostname": "h100-node-3.lab.local",
  "proto_version": 1,
  "generation": 47,
  "boot_time": 1746489600,
  "cpu": {
    "cores_online": 224,
    "cores_physical": 112,
    "sockets": 2,
    "model": "Intel(R) Xeon(R) Platinum 8480+",
    "vendor": "GenuineIntel",
    "arch": "x86_64",
    "mhz": 3800
  },
  "mem": { "total_mb": 1031920, "available_mb": 989411 },
  "gpus": [
    {
      "index": 0,
      "uuid": "GPU-1a2b3c4d-5e6f-7081-9203-abcdef012345",
      "name": "NVIDIA H100 80GB HBM3",
      "pci_vendor": 4318,
      "pci_device": 9008,
      "pci_addr": "0000:1b:00.0",
      "vram_total_mb": 81559,
      "vram_free_mb": 80124,
      "compute_capability": [9, 0],
      "nvlink_peers": ["GPU-2b3c...", "GPU-3c4d..."],
      "utilization_pct": 0
    }
  ],
  "pci": [
    { "addr": "0000:1b:00.0", "vendor": 4318, "device": 9008, "class": 197632, "numa_node": 0, "iommu_group": 17 },
    { "addr": "0000:c1:00.0", "vendor": 5555, "device": 4116, "class": 131328, "numa_node": 1, "iommu_group": 42 }
  ],
  "numa": [
    { "id": 0, "cpus": [0,1,2,3,4,5,6,7], "mem_total_mb": 515960 },
    { "id": 1, "cpus": [8,9,10,11,12,13,14,15], "mem_total_mb": 515960 }
  ],
  "load": {
    "loadavg_1m": 42, "loadavg_5m": 81, "loadavg_15m": 110,
    "cpu_pct": 380, "mem_pct": 4, "task_count": 3
  }
}
```

### Interfaces

Public API of `classic-ad` (everything else `pub(crate)`):

```rust
/// Construct + spawn the discovery and gossip tasks. Returns a clone-cheap
/// handle. Drops of every clone shut the tasks down.
pub fn start(
    self_node_id: NodeId,
    proto: classic_proto::ConnectionRegistry,
    cfg: AdConfig,
) -> AdStore;

#[derive(Clone, Debug)]
pub struct AdConfig {
    /// Default 1 s. Smallest accepted: 250 ms.
    pub fast_tick: std::time::Duration,
    /// Default 60 s. Smallest accepted: 5 s.
    pub slow_tick: std::time::Duration,
    /// Default 10 s.
    pub gossip_period: std::time::Duration,
    /// TTL for peer ads after a connection drop. Default 90 s.
    pub peer_grace: std::time::Duration,
}

#[derive(Clone)]
pub struct AdStore { /* Arc<Inner> */ }

impl AdStore {
    /// This node's currently published ad. Cheap (lock + clone).
    pub fn self_ad(&self) -> NodeAd;

    /// All ads — own first, then peer ads in arbitrary but stable order.
    pub fn all_ads(&self) -> Vec<NodeAd>;

    /// Single peer ad. None if unknown or evicted.
    pub fn peer(&self, id: NodeId) -> Option<NodeAd>;

    /// Stream of inserts/updates/removals. Buffered, lossy on lag (overflow drops oldest).
    pub fn watch(&self) -> impl futures::Stream<Item = AdUpdate> + Send + 'static;
}
```

NVML access is encapsulated:

```rust
pub(crate) struct GpuProbe {
    nvml: Option<nvml_wrapper::Nvml>,
}

impl GpuProbe {
    /// Initializes NVML lazily; returns a probe that yields [] if NVML is missing.
    pub fn new() -> Self {
        match nvml_wrapper::Nvml::init() {
            Ok(n)  => Self { nvml: Some(n) },
            Err(e) => { tracing::warn!(?e, "NVML unavailable; reporting no GPUs"); Self { nvml: None } }
        }
    }
    pub fn enumerate(&self) -> Vec<GpuInfo> { /* … */ }
    pub fn refresh_dynamic(&self, out: &mut [GpuInfo]) { /* vram_free, util */ }
}
```

NVML calls used (all via `nvml-wrapper`):

- `Nvml::init()` — once at process start
- `Nvml::device_count()`
- `Nvml::device_by_index(i)`
- `Device::uuid()`, `Device::name()`, `Device::pci_info()`, `Device::memory_info()`,
  `Device::cuda_compute_capability()`, `Device::utilization_rates()`,
  `Device::nvlink_remote_pci_info(link)` for `link in 0..Device::nvlink_count()` (skip inactive links)

Sysfs paths used:

| Probe        | Path                                                     |
|--------------|----------------------------------------------------------|
| PCI device   | `/sys/bus/pci/devices/<addr>/{vendor,device,class,numa_node}` |
| IOMMU group  | `/sys/bus/pci/devices/<addr>/iommu_group` (symlink basename) |
| NUMA list    | `/sys/devices/system/node/node*/cpulist`                 |
| NUMA mem     | `/sys/devices/system/node/node*/meminfo` (line `MemTotal`) |
| CPU info     | `/proc/cpuinfo`, `/proc/stat`                            |
| Mem info     | `/proc/meminfo`                                          |
| Load         | `/proc/loadavg`                                          |

A small dependency for PCI parsing is acceptable but the spec is simple enough to do directly. Recommended: implement inline (≤120 LOC) using `std::fs`. Avoid `pciutils-rs` and similar — they pull in too much.

### File / crate layout

```
crates/
  classic-ad/
    Cargo.toml          # NEW
    src/
      lib.rs            # NEW — pub re-exports, start(), AdStore
      schema.rs         # NEW — NodeAd & sub-structs, derives
      frames.rs         # NEW — frame-kind constants, encode/decode glue
      store.rs          # NEW — AdStore, watch(), LWW logic
      discovery/
        mod.rs          # NEW — Discovery task, fast/slow tick scheduling
        cpu.rs          # NEW — /proc/cpuinfo, /proc/stat
        mem.rs          # NEW — /proc/meminfo
        load.rs         # NEW — /proc/loadavg, cpu_pct delta
        pci.rs          # NEW — /sys/bus/pci enumeration
        numa.rs         # NEW — /sys/devices/system/node
        gpu.rs          # NEW — GpuProbe (NVML wrapper, optional)
      gossip.rs         # NEW — RX handler + TX broadcaster
    tests/
      schema_roundtrip.rs   # NEW — bincode encode/decode invariants
      discovery_fakes.rs    # NEW — sysfs/procfs fixtures via tempdir
      gossip_pair.rs        # NEW — two AdStores over an in-memory mux
  classic-proto/
    src/lib.rs          # MODIFIED — add 0x0100–0x01FF to FrameKind enum
                        #            (constants only, no payload types)
  classic-node/
    src/main.rs         # MODIFIED — call classic_ad::start() during bring-up
```

The discovery probes accept an injectable `&dyn Sysroot` so unit tests can swap a tempdir fixture for `/`. A trivial default impl reads from `/`.

```rust
pub trait Sysroot: Send + Sync {
    fn read(&self, rel: &Path) -> std::io::Result<Vec<u8>>;
    fn read_link(&self, rel: &Path) -> std::io::Result<PathBuf>;
    fn read_dir(&self, rel: &Path) -> std::io::Result<Vec<PathBuf>>;
}
```

## Requirements

### Functional

- [ ] FR-1: On startup, `AdStore::self_ad()` returns a `NodeAd` populated with hardware data within 500 ms of `start()` returning.
- [ ] FR-2: `gpus` is empty and a single warning is logged when `libnvidia-ml.so.1` is not present. Daemon does not crash, exit, or panic.
- [ ] FR-3: `pci` enumerates every entry in `/sys/bus/pci/devices/`. Order: lexical by `addr`.
- [ ] FR-4: `numa` enumerates every `nodeN` directory under `/sys/devices/system/node/` and assigns each online CPU to exactly one NUMA node.
- [ ] FR-5: `LoadSample` and dynamic GPU fields refresh on a 1 s cadence (configurable via `AdConfig`).
- [ ] FR-6: Static fields (PCI, NUMA, GPU list, CPU info) refresh on a 60 s cadence and at startup. A static field that changes increments `generation`.
- [ ] FR-7: Daemon broadcasts its full ad on every connection-up event and every 10 s.
- [ ] FR-8: Receivers store peer ads keyed by `NodeId`. Conflict resolution is LWW on `(generation, boot_time)`: prefer higher generation; tie-break by higher boot_time.
- [ ] FR-9: On a peer connection-down event, the corresponding ad is marked stale and removed after `peer_grace` (default 90 s) unless a fresh ad arrives.
- [ ] FR-10: `AdStore::watch()` emits `Inserted` / `Updated` / `Removed` events; `Updated` MUST NOT fire when a re-broadcast carries an unchanged `generation`.
- [ ] FR-11: Frame kinds `0x0100`–`0x0102` are registered with `classic-proto`'s `FrameKind` enum. Decoders reject unknown kinds in this range with `Error::UnknownFrameKind`.
- [ ] FR-12: All wire payloads encode and decode via bincode v2 (fixed-int, little-endian) without allocations beyond the payload itself.

### Non-functional

- **Performance:**
  - Startup discovery (own ad) completes in <200 ms on a 2-socket / 4-GPU host.
  - Fast-tick refresh costs <2 ms CPU on the same host.
  - `AdStore::all_ads()` returns in <100 µs for a 256-node cluster (lock + clone of a `HashMap<NodeId, NodeAd>`).
  - Encoded `NodeAd` size ≤ 8 KiB for a host with ≤16 GPUs and ≤256 PCI devices.
- **Compatibility:** Linux kernel ≥ 6.1. Cgroup v2 is irrelevant to this crate. Architecture: x86_64 primary, aarch64 must compile (`gpu` and `pci` probes still apply; some sysfs fields may be absent and produce `numa_node = -1` etc.).
- **Security:** Read-only access to `/proc` and `/sys`. No writes. No network exposure beyond the gossip frames riding on `classic-proto`. Trusted cluster — no auth on incoming `NodeAd` frames.
- **Hardware:** NVML probes target NVIDIA driver ≥ 535 (compute capability reporting + NVLink remote PCI info APIs stable). Tested on H100 (compute 9.0) and L40S (compute 8.9). Hosts without NVIDIA hardware: `gpus = []`, no error.

## Testing plan

### Unit

In `crates/classic-ad/src/discovery/*.rs` and `crates/classic-ad/src/store.rs`:

- **CPU probe:** parse fixture `/proc/cpuinfo` for Xeon 8480+ and Ryzen 9 7950X. Assert socket / core / online-core counts.
- **Mem probe:** fixture `/proc/meminfo` with various `MemAvailable` cases (including pre-3.14 kernels missing the field — fall back to `MemFree + Buffers + Cached`).
- **Load probe:** fixture `/proc/loadavg` ("0.42 0.81 1.10 2/734 1234"); fixture pair of `/proc/stat` snapshots 1 s apart, assert `cpu_pct` delta.
- **PCI probe:** tempdir simulating `/sys/bus/pci/devices/0000:1b:00.0/{vendor,device,class,numa_node}` plus an iommu_group symlink to `…/iommu_groups/17`. Assert exact `PciDevice`.
- **NUMA probe:** tempdir with two `nodeN` dirs, varied `cpulist` formats ("0-7,16-23").
- **GpuProbe::new()** when NVML is absent: assert `enumerate()` returns `[]` and exactly one warning was logged (capture via `tracing-test`).
- **AdStore LWW:** insert ad with generation 5; insert with generation 3 — discarded. Insert with generation 5, boot_time T+1 — accepted (restart).
- **AdStore::watch():** subscribe; insert; assert `Inserted`. Re-insert same `(node_id, generation)` — assert no event. Insert higher generation — assert `Updated`.

### Integration

In `crates/classic-ad/tests/`:

- **schema_roundtrip.rs** — random-fill `NodeAd` (proptest), encode + decode, assert equality. Verify the encoded length is bounded as per the non-functional req.
- **gossip_pair.rs** — spin up two `AdStore`s wired to an in-memory bidi `Connection` from `classic-proto` (no real TCP). Push an own-ad on each side, assert each side sees the other's ad within 1 tick.
- **gossip_lww.rs** — same harness, send three ads with generations 1, 3, 2 in order; assert final stored generation is 3.
- **gossip_eviction.rs** — drop one side of the connection; assert the other side's `peer()` returns `Some` for `peer_grace - 1` and `None` after, and that `watch()` emits `Removed`.

### End-to-end

Manual smoke tests, run on three Ubuntu 24.04 hosts with mixed GPUs (one H100 host, one L40S host, one CPU-only host):

```bash
# on each host, $i in 1..3:
cargo run -p classic-node -- --listen 0.0.0.0:7777 --peers host1:7777,host2:7777,host3:7777
# on host1:
cargo run -p classic-cli -- ad list --json | jq '.[].hostname'
# expected: three lines, one per host, within 15 s of all three running
cargo run -p classic-cli -- ad show host3 | jq '.gpus | length'
# expected: 0 on the CPU-only host, ≥1 elsewhere
```

(`classic-cli ad` subcommands are part of plan 04's CLI but stubs that just dump `AdStore::all_ads()` are added by this plan to support smoke testing.)

### Hardware-dependent

Mark with `#[ignore]` and a `// HW: nvidia` comment so CI can skip:

- Real-NVML enumeration on H100 host: assert `gpus[0].compute_capability == (9, 0)`, `vram_total_mb` ≈ 81920 ± 1024, `nvlink_peers` non-empty on a multi-GPU host.
- vram_free_mb falls and rises around a `cudaMalloc` test stub.

CI runs only the unit + integration tiers; hardware tests are documented for human runners.

## Acceptance criteria

- [ ] AC-1: `cargo test -p classic-ad` is green on a host with no NVIDIA driver installed.
- [ ] AC-2: `cargo test -p classic-ad` is green on a host with NVIDIA driver + at least one GPU; the hardware-gated tests pass when run with `--ignored`.
- [ ] AC-3: On a 3-node cluster, every node's `AdStore::all_ads()` returns 3 ads within 15 s of mesh formation.
- [ ] AC-4: A node with `libnvidia-ml.so.1` removed starts cleanly, logs exactly one warning containing the string "NVML", and reports `gpus: []`.
- [ ] AC-5: Killing one daemon causes its ad to disappear from the survivors' `AdStore` within `peer_grace + gossip_period` (default 100 s).
- [ ] AC-6: Restarting that daemon causes its ad to reappear within `gossip_period` (default 10 s) with `boot_time` strictly greater than before.
- [ ] AC-7: `bincode::encode_to_vec(NodeAd, …)` round-trips byte-equal for the example ad rendered above (modulo string ordering of `Vec` fields, which we declare stable).
- [ ] AC-8: `classic-proto::FrameKind` includes `NodeAd = 0x0100`, `AdGossip = 0x0101`, `AdRequest = 0x0102` and `cargo deny check` passes.
- [ ] AC-9: All tests in the testing plan pass on Linux x86_64.

## Open questions

1. **Should `nvlink_peers` carry bandwidth?** Plan 03 may want to predicate on "GPUs on the same NVSwitch fabric". Deferred to plan 03; if needed, extend `GpuInfo` with `nvlink_bandwidth_gbps: Vec<u32>` then. Not blocking v1.
2. **Is `task_count` plan 02 or plan 04?** The field lives in `LoadSample` (plan 02 owns the schema), but the *value* comes from plan 04's spawn registry. v1 plan 02 reports `0` until plan 04 wires a `TaskCounter` callback into `AdConfig`. Resolution: plan 04 owns wiring, plan 02 owns the field. Add `AdConfig::task_count_fn: Option<Arc<dyn Fn() -> u32 + Send + Sync>>`.
3. **Do we gossip ads to peers we just disconnected from before evicting?** No — gossip TX iterates the live connection set. The `peer_grace` is purely a receiver-side TTL. Confirmed.
4. **Should we gate `nvml-wrapper` behind a Cargo feature?** No — it links to `libnvidia-ml.so.1` lazily via `dlopen`, so the binary builds and runs on hosts without the driver. Confirmed; no feature flag.
5. **Field-presence policy for cross-arch.** On aarch64 hosts, fields like `iommu_group` may be absent. Use `Option`/sentinels (`-1` for `numa_node`, `None` for `iommu_group`). Already encoded in the schema above.

## References

- `plans/ARCHITECTURE.md` — identity types, frame format, frame ranges, transport, encoding.
- `plans/01-skeleton-transport.md` — `classic-proto`, `ConnectionRegistry`, `FrameKind` enum.
- `plans/03-placement-predicates.md` — downstream consumer of `NodeAd` fields. Predicate-language fields must align with field names declared here.
- NVML reference: <https://docs.nvidia.com/deploy/nvml-api/index.html>
- `nvml-wrapper` crate: <https://docs.rs/nvml-wrapper>
- Linux sysfs PCI bus docs: `Documentation/ABI/testing/sysfs-bus-pci`.
- Linux NUMA sysfs docs: `Documentation/ABI/stable/sysfs-devices-node`.
- bincode v2 spec: <https://github.com/bincode-org/bincode/blob/trunk/docs/spec.md>
- ChrysaLisp service-directory gossip (prior art): <https://github.com/vygr/ChrysaLisp> (`gui/farm/`).
