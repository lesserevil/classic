//! Shared fixtures for integration tests. Builds the canonical NodeAd
//! shapes the plan-3 §"Test fixtures" section calls out.

#![allow(dead_code)]

use classic_place::{CpuAd, GpuAd, LoadAd, MemAd, NodeAd, OsAd, PciAd};
use classic_proto::NodeId;

pub fn nid(byte: u8) -> NodeId {
    let mut bytes = [0u8; 16];
    bytes[0] = byte;
    NodeId(bytes)
}

fn base(node: u8) -> NodeAd {
    NodeAd {
        node_id: nid(node),
        hostname: format!("node-{node}"),
        gen: 1,
        os: OsAd { kernel: "Linux".into(), distro: "Ubuntu 24.04".into() },
        cpu: CpuAd {
            cores: 16,
            threads: 32,
            arch: "x86_64".into(),
            model: "Generic CPU".into(),
        },
        mem: MemAd { total_mb: 131_072, free_mb: 100_000 },
        load: LoadAd { cpu_pct: 5.0, mem_pct: 10.0, load_1m: 0.5 },
        gpu: Vec::new(),
        pci: Vec::new(),
        labels: Default::default(),
    }
}

fn nvidia_gpu(index: u32, model: &str, vram_mb: u64, busy: bool) -> GpuAd {
    GpuAd {
        index,
        vendor: 0x10de,
        device: if vram_mb >= 80_000 { 0x2330 } else { 0x20F1 },
        model: model.into(),
        vram_mb,
        vram_free_mb: if busy { vram_mb / 4 } else { vram_mb - 1024 },
        sm_count: if vram_mb >= 80_000 { 132 } else { 108 },
        in_use: busy,
        mig: false,
    }
}

/// 16 cores, 64 GB RAM, no GPU. CPU-bound workload target.
pub fn ad_cpu_only(node: u8) -> NodeAd {
    base(node)
}

/// 1× A100 40 GB.
pub fn ad_a100x1(node: u8, busy: bool) -> NodeAd {
    let mut a = base(node);
    a.cpu.cores = 32;
    a.cpu.threads = 64;
    a.cpu.model = "AMD EPYC 7763".into();
    a.gpu.push(nvidia_gpu(0, "NVIDIA A100 40GB", 40_960, busy));
    a
}

/// 2× H100 80 GB.
pub fn ad_h100x2(node: u8, busy_count: u32) -> NodeAd {
    let mut a = base(node);
    a.cpu.cores = 32;
    a.cpu.threads = 64;
    for i in 0..2 {
        a.gpu.push(nvidia_gpu(i, "NVIDIA H100 80GB HBM3", 81_920, i < busy_count));
    }
    a
}

/// 8× H100 80 GB with optional load.
pub fn ad_h100x8(node: u8, busy_count: u32, cpu_pct: f64) -> NodeAd {
    let mut a = base(node);
    a.cpu.cores = 96;
    a.cpu.threads = 192;
    a.cpu.model = "AMD EPYC 9654".into();
    a.mem.total_mb = 1024 * 1024;
    a.mem.free_mb = 900_000;
    a.load.cpu_pct = cpu_pct;
    for i in 0..8 {
        a.gpu.push(nvidia_gpu(i, "NVIDIA H100 80GB HBM3", 81_920, i < busy_count));
    }
    a
}

/// AMD MI300X (vendor 0x1002, 192 GB VRAM).
pub fn ad_amd_mi300(node: u8) -> NodeAd {
    let mut a = base(node);
    a.cpu.cores = 64;
    a.gpu.push(GpuAd {
        index: 0,
        vendor: 0x1002,
        device: 0x74A1,
        model: "AMD Instinct MI300X".into(),
        vram_mb: 196_608,
        vram_free_mb: 196_000,
        sm_count: 304,
        in_use: false,
        mig: false,
    });
    a
}

/// aarch64 CPU-only (no GPU). Useful for arch-discriminating predicates.
pub fn ad_arm_cpu_only(node: u8) -> NodeAd {
    let mut a = base(node);
    a.cpu.arch = "aarch64".into();
    a.cpu.cores = 80;
    a.cpu.threads = 80;
    a.cpu.model = "Ampere Altra".into();
    a
}

/// PCI device list builder for tests that need PCI predicates.
pub fn with_pci(mut a: NodeAd, devices: Vec<PciAd>) -> NodeAd {
    a.pci = devices;
    a
}

pub fn pci_mlx() -> PciAd {
    PciAd {
        bdf: "0000:c0:00.0".into(),
        vendor: 0x15b3,
        device: 0x101e,
        class: 0x020000,
    }
}
