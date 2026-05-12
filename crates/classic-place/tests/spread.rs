//! SPREAD end-to-end: bipartite matching with backtracking and MRV
//! ordering. Covers happy path, cardinality short-circuit, the
//! scenario where greedy ordering would fail but MRV succeeds,
//! no-matching infeasibility, and caller-order preservation.

use std::collections::HashSet;

use classic_place::{
    parse_req, place_group, GpuAd, GroupMember, GroupPlaceError, GroupStrategy, NodeAd,
    PlacementGroup,
};
use classic_proto::NodeId;

fn id(n: u8) -> NodeId {
    NodeId([n; 16])
}

fn member(label: &str, req_src: &str) -> GroupMember {
    GroupMember {
        label: label.into(),
        req: parse_req(req_src).expect("test predicate must parse"),
        argv: vec![],
        env: vec![],
    }
}

fn cpu_node(node_id: NodeId, cores: u32) -> NodeAd {
    let mut a = NodeAd::default();
    a.node_id = node_id;
    a.cpu.cores = cores;
    a.cpu.arch = "x86_64".into();
    a.mem.total_mb = 65_536;
    a.mem.free_mb = 65_536;
    a
}

fn gpu_node(node_id: NodeId, vram_mb: u64) -> NodeAd {
    let mut a = cpu_node(node_id, 32);
    a.gpu.push(GpuAd {
        index: 0,
        vendor: 0x10de,
        device: 0x2330,
        model: "h100".into(),
        vram_mb,
        vram_free_mb: vram_mb,
        sm_count: 132,
        in_use: false,
        mig: false,
    });
    a
}

#[test]
fn spread_too_large() {
    let g = PlacementGroup {
        strategy: GroupStrategy::Spread,
        members: vec![
            member("a", "true"),
            member("b", "true"),
            member("c", "true"),
            member("d", "true"),
            member("e", "true"),
        ],
    };
    let ads = vec![cpu_node(id(1), 4), cpu_node(id(2), 4), cpu_node(id(3), 4)];
    assert_eq!(
        place_group(&g, &ads).unwrap_err(),
        GroupPlaceError::SpreadTooLarge { needed: 5, nodes: 3 },
    );
}

#[test]
fn spread_one_per_node() {
    let g = PlacementGroup {
        strategy: GroupStrategy::Spread,
        members: vec![
            member("a", "any(gpu, gpu.vendor == 0x10de)"),
            member("b", "any(gpu, gpu.vendor == 0x10de)"),
            member("c", "any(gpu, gpu.vendor == 0x10de)"),
        ],
    };
    let ads = vec![
        gpu_node(id(1), 80_000),
        gpu_node(id(2), 80_000),
        gpu_node(id(3), 80_000),
    ];
    let out = place_group(&g, &ads).unwrap();
    assert_eq!(out.len(), 3);
    // Caller order preserved.
    assert_eq!(out[0].0, "a");
    assert_eq!(out[1].0, "b");
    assert_eq!(out[2].0, "c");
    // All distinct nodes.
    let unique: HashSet<NodeId> = out.iter().map(|(_, n)| *n).collect();
    assert_eq!(unique.len(), 3);
}

#[test]
fn spread_backtrack_required() {
    // A matches only node X; B matches X or Y. Greedy [B,A] order
    // would assign B->X then have nothing for A. MRV reorders so A
    // (1 candidate) is tried first, picks X; B then picks Y.
    //
    // We use vram_mb to discriminate: node X has 80GB, node Y has 40GB.
    // Member A requires `gpu.vram_mb >= 80000` (only X matches).
    // Member B requires `gpu.vendor == 0x10de` (both match).
    let g = PlacementGroup {
        strategy: GroupStrategy::Spread,
        // Declare in the order that would fail greedy: B first, A second.
        members: vec![
            member("B", "any(gpu, gpu.vendor == 0x10de)"),
            member("A", "any(gpu, gpu.vram_mb >= 80000)"),
        ],
    };
    let ads = vec![gpu_node(id(1), 80_000), gpu_node(id(2), 40_000)];
    let out = place_group(&g, &ads).unwrap();
    // A must land on the 80GB node (id=1); B must land on the 40GB node (id=2).
    let by_label: std::collections::HashMap<String, NodeId> = out.into_iter().collect();
    assert_eq!(by_label["A"], id(1));
    assert_eq!(by_label["B"], id(2));
}

#[test]
fn spread_infeasible_via_matching() {
    // Three members each requiring the same 80GB GPU; only one node
    // has it. No perfect matching exists.
    let g = PlacementGroup {
        strategy: GroupStrategy::Spread,
        members: vec![
            member("a", "any(gpu, gpu.vram_mb >= 80000)"),
            member("b", "any(gpu, gpu.vram_mb >= 80000)"),
            member("c", "any(gpu, gpu.vram_mb >= 80000)"),
        ],
    };
    let ads = vec![
        gpu_node(id(1), 80_000),
        gpu_node(id(2), 40_000),
        gpu_node(id(3), 40_000),
    ];
    assert_eq!(
        place_group(&g, &ads).unwrap_err(),
        GroupPlaceError::SpreadInfeasible,
    );
}

#[test]
fn spread_result_in_caller_order() {
    // Three members in a specific declaration order; the result must
    // come back in that order regardless of MRV reordering inside.
    let g = PlacementGroup {
        strategy: GroupStrategy::Spread,
        members: vec![
            // m1: matches all 3 nodes -> tried last by MRV.
            member("m1", "any(gpu, gpu.vendor == 0x10de)"),
            // m2: matches only the 80GB node -> tried first by MRV.
            member("m2", "any(gpu, gpu.vram_mb >= 80000)"),
            // m3: matches all 3 nodes.
            member("m3", "any(gpu, gpu.vendor == 0x10de)"),
        ],
    };
    let ads = vec![
        gpu_node(id(1), 80_000),
        gpu_node(id(2), 40_000),
        gpu_node(id(3), 40_000),
    ];
    let out = place_group(&g, &ads).unwrap();
    assert_eq!(out[0].0, "m1");
    assert_eq!(out[1].0, "m2");
    assert_eq!(out[2].0, "m3");
}
