//! Table-driven evaluation tests across the six plan-3 example predicates
//! and the six fixture builders. Plus the 5-fixture cluster end-to-end
//! `place_str` test (AC-3) and a determinism smoke loop.

mod common;

use classic_place::{matches, parse_req, place_str};
use common::*;

const PLAN_PREDS: &[&str] = &[
    // 1: NVIDIA + ≥80 GB + idle
    "any(gpu, gpu.vendor == 0x10de && gpu.vram_mb >= 80000 && !gpu.in_use)",
    // 2: ≥2 idle GPUs
    "count(gpu, !gpu.in_use) >= 2",
    // 3: any GPU, exclude MIG
    "any(gpu, !gpu.mig && !gpu.in_use)",
    // 4: AMD MI300X specifically
    "any(gpu, gpu.vendor in [0x1002] && gpu.vram_mb >= 192000)",
    // 5: ≥64 GB RAM + CPU < 50%
    "mem.free_mb >= 65536 && load.cpu_pct < 50.0",
    // 6: x86_64, ≥32 cores, Mellanox NIC
    "cpu.arch == \"x86_64\" && cpu.cores >= 32 && any(pci, pci.vendor == 0x15b3)",
];

#[test]
fn plan_predicate_1_idle_h100_anywhere() {
    let req = parse_req(PLAN_PREDS[0]).unwrap();
    assert!(!matches(&req, &ad_cpu_only(1)));
    assert!(!matches(&req, &ad_a100x1(2, false))); // A100 has 40 GB, not 80
    assert!(matches(&req, &ad_h100x2(3, 0))); // 2× H100 idle
    assert!(matches(&req, &ad_h100x8(4, 0, 10.0)));
    assert!(!matches(&req, &ad_h100x8(5, 8, 10.0))); // all busy
    assert!(!matches(&req, &ad_amd_mi300(6))); // wrong vendor
    assert!(!matches(&req, &ad_arm_cpu_only(7)));
}

#[test]
fn plan_predicate_2_at_least_two_idle_gpus() {
    let req = parse_req(PLAN_PREDS[1]).unwrap();
    assert!(!matches(&req, &ad_cpu_only(1)));
    assert!(!matches(&req, &ad_a100x1(2, false))); // only one GPU
    assert!(matches(&req, &ad_h100x2(3, 0)));
    assert!(matches(&req, &ad_h100x8(4, 0, 10.0)));
    assert!(matches(&req, &ad_h100x8(5, 6, 10.0))); // 2 still idle
    assert!(!matches(&req, &ad_h100x8(6, 7, 10.0))); // only 1 idle
}

#[test]
fn plan_predicate_3_any_gpu_no_mig() {
    let req = parse_req(PLAN_PREDS[2]).unwrap();
    assert!(!matches(&req, &ad_cpu_only(1)));
    assert!(matches(&req, &ad_a100x1(2, false)));
    assert!(matches(&req, &ad_h100x2(3, 0)));
    assert!(!matches(&req, &ad_h100x2(4, 2))); // both busy → no idle non-MIG GPU
    assert!(matches(&req, &ad_amd_mi300(5)));
    assert!(!matches(&req, &ad_arm_cpu_only(6)));
}

#[test]
fn plan_predicate_4_amd_mi300_only() {
    let req = parse_req(PLAN_PREDS[3]).unwrap();
    assert!(matches(&req, &ad_amd_mi300(1)));
    assert!(!matches(&req, &ad_h100x8(2, 0, 10.0)));
    assert!(!matches(&req, &ad_a100x1(3, false)));
    assert!(!matches(&req, &ad_cpu_only(4)));
}

#[test]
fn plan_predicate_5_ram_and_load() {
    let req = parse_req(PLAN_PREDS[4]).unwrap();
    assert!(matches(&req, &ad_cpu_only(1))); // 65000 free, 5% load
    assert!(matches(&req, &ad_h100x8(2, 0, 10.0)));
    assert!(!matches(&req, &ad_h100x8(3, 0, 80.0))); // CPU too high
}

#[test]
fn plan_predicate_6_x86_cores_and_mellanox() {
    let req = parse_req(PLAN_PREDS[5]).unwrap();
    let h_with_nic = with_pci(ad_h100x8(1, 0, 10.0), vec![pci_mlx()]);
    let h_no_nic = ad_h100x8(2, 0, 10.0);
    let arm = with_pci(ad_arm_cpu_only(3), vec![pci_mlx()]);
    let cpu_with_nic = with_pci(ad_cpu_only(4), vec![pci_mlx()]); // only 16 cores
    assert!(matches(&req, &h_with_nic));
    assert!(!matches(&req, &h_no_nic));
    assert!(!matches(&req, &arm)); // wrong arch
    assert!(!matches(&req, &cpu_with_nic)); // not enough cores
}

#[test]
fn five_fixture_cluster_default_rank_picks_documented_best() {
    // Plan §Testing plan / Integration: 5-fixture cluster {cpu_only,
    // a100x1, h100x2, h100x8 idle low-load, h100x8 idle high-load}.
    // For predicate 1 (idle H100), default rank prefers the lowest-CPU
    // 8-idle node — that's `ad_h100x8(4, 0, 10.0)`.
    let cluster = vec![
        ad_cpu_only(1),
        ad_a100x1(2, false),
        ad_h100x2(3, 0),
        ad_h100x8(4, 0, 10.0),
        ad_h100x8(5, 0, 80.0),
    ];

    let out = place_str(PLAN_PREDS[0], None, &cluster).unwrap();
    // Default rank: -cpu_pct - 1000 * idle_gpus.
    //   h100x2(3): -5  - 1000*2 = -2005
    //   h100x8(4): -10 - 1000*8 = -8010
    //   h100x8(5): -80 - 1000*8 = -8080
    // Higher score wins, so h100x2 (-2005) > h100x8(4) (-8010) > h100x8(5).
    assert_eq!(out[0].0, common::nid(3));
    assert_eq!(out[1].0, common::nid(4));
    assert_eq!(out[2].0, common::nid(5));
}

#[test]
fn determinism_1000_runs_yields_identical_output() {
    let cluster = vec![
        ad_cpu_only(1),
        ad_h100x2(3, 0),
        ad_h100x8(4, 0, 10.0),
        ad_h100x8(5, 0, 80.0),
    ];
    let baseline = place_str(PLAN_PREDS[0], None, &cluster).unwrap();
    for _ in 0..1000 {
        let again = place_str(PLAN_PREDS[0], None, &cluster).unwrap();
        assert_eq!(again, baseline);
    }
}

/// Performance smoke test: 100 ads × 8 GPUs × the predicate-1 AST should
/// finish well under 5 ms in release. In debug, just assert it finishes.
#[test]
fn perf_smoke_100_ads_8_gpus() {
    let mut cluster = Vec::with_capacity(100);
    for i in 0..100u8 {
        cluster.push(ad_h100x8(i, (i % 9) as u32, (i as f64) * 0.5));
    }
    let req = parse_req(PLAN_PREDS[0]).unwrap();
    let rank = classic_place::default_rank();

    let start = std::time::Instant::now();
    let _ = classic_place::place(&req, &rank, &cluster);
    let elapsed = start.elapsed();

    // Debug builds are slow; only assert the budget under release.
    if !cfg!(debug_assertions) {
        assert!(
            elapsed.as_millis() < 5,
            "place() over 100 ads took {:?}, > 5 ms budget",
            elapsed
        );
    }
}
