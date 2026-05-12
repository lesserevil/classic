//! PACK end-to-end: gang-scheduling onto a single node with
//! intra-group contention enforced via free-pool deduction.

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

fn node_with_gpus(node_id: NodeId, vrams_mb: Vec<u64>) -> NodeAd {
    let mut a = NodeAd::default();
    a.node_id = node_id;
    a.hostname = "host".into();
    a.cpu.cores = 32;
    a.cpu.threads = 64;
    a.cpu.arch = "x86_64".into();
    a.mem.total_mb = 65_536;
    a.mem.free_mb = 65_536;
    a.gpu = vrams_mb
        .into_iter()
        .enumerate()
        .map(|(i, vram_mb)| GpuAd {
            index: i as u32,
            vendor: 0x10de,
            device: 0x2330,
            model: "h100".into(),
            vram_mb,
            vram_free_mb: vram_mb,
            sm_count: 132,
            in_use: false,
            mig: false,
        })
        .collect();
    a
}

#[test]
fn pack_single_node_satisfies_all() {
    // 2 members each require a free GPU; node has 2 free GPUs.
    let node = node_with_gpus(id(1), vec![80_000, 80_000]);
    let g = PlacementGroup {
        strategy: GroupStrategy::Pack,
        members: vec![
            member("trainer", "any(gpu, !gpu.in_use)"),
            member("eval", "any(gpu, !gpu.in_use)"),
        ],
    };
    let out = place_group(&g, &[node]).unwrap();
    assert_eq!(out.len(), 2);
    assert_eq!(out[0], ("trainer".into(), id(1)));
    assert_eq!(out[1], ("eval".into(), id(1)));
}

#[test]
fn pack_intragroup_contention() {
    // 2 members each require a free 80GB GPU; node has one 80GB + one 40GB.
    // First member takes the 80GB; second has nothing left that satisfies.
    let node = node_with_gpus(id(1), vec![80_000, 40_000]);
    let g = PlacementGroup {
        strategy: GroupStrategy::Pack,
        members: vec![
            member("a", "any(gpu, !gpu.in_use && gpu.vram_mb >= 80000)"),
            member("b", "any(gpu, !gpu.in_use && gpu.vram_mb >= 80000)"),
        ],
    };
    assert_eq!(
        place_group(&g, &[node]).unwrap_err(),
        GroupPlaceError::PackInfeasible(2),
    );
}

#[test]
fn pack_first_member_wins_contention() {
    // Each member needs any free GPU. Node has 80GB then 40GB. First
    // member gets the richer GPU (richer-first picker), then second
    // gets the 40GB.
    let node = node_with_gpus(id(1), vec![80_000, 40_000]);
    let g = PlacementGroup {
        strategy: GroupStrategy::Pack,
        members: vec![
            member("first", "any(gpu, !gpu.in_use)"),
            member("second", "any(gpu, !gpu.in_use)"),
        ],
    };
    let out = place_group(&g, &[node]).unwrap();
    assert_eq!(out.len(), 2);
    // Both land on the same node.
    assert_eq!(out[0].1, id(1));
    assert_eq!(out[1].1, id(1));
}

#[test]
fn pack_member_infeasible_against_all_ads() {
    // No node has any GPU at all, but a member requires one.
    let node = node_with_gpus(id(1), vec![]);
    let g = PlacementGroup {
        strategy: GroupStrategy::Pack,
        members: vec![member("a", "any(gpu, !gpu.in_use)")],
    };
    assert_eq!(
        place_group(&g, &[node]).unwrap_err(),
        GroupPlaceError::PackInfeasible(1),
    );
}

#[test]
fn pack_picks_richest_node_first() {
    // Two PACK-capable nodes; the one with more total free GPU mem
    // wins. Both members land there even though the leaner node
    // could also satisfy.
    let lean = node_with_gpus(id(1), vec![40_000, 40_000]);
    let rich = node_with_gpus(id(2), vec![80_000, 80_000]);
    let g = PlacementGroup {
        strategy: GroupStrategy::Pack,
        members: vec![
            member("a", "any(gpu, !gpu.in_use)"),
            member("b", "any(gpu, !gpu.in_use)"),
        ],
    };
    let out = place_group(&g, &[lean, rich]).unwrap();
    assert_eq!(out[0].1, id(2));
    assert_eq!(out[1].1, id(2));
}
