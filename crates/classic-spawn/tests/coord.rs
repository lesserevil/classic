//! Coordinator integration tests with a mock transport.
//!
//! Scenarios covered:
//! - Phase-1 timeout from a silent node → coord emits Abort to acked
//!   nodes + returns ReserveTimeout.
//! - Phase-1 Deny → coord emits Abort to other acked nodes + returns
//!   ReserveDenied.
//! - Phase-2 CommitFailed → coord emits Kill to already-spawned NetIds
//!   on sibling nodes + returns CommitFailed.
//! - Happy path → result members.len() == group.members.len() and
//!   labels are in caller declaration order.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use classic_place::{
    parse_req, GpuAd, GroupMember, GroupStrategy, NodeAd, PlacementGroup,
};
use classic_proto::{MboxId, NetId, NodeId};
use classic_spawn::{
    submit_group, CommitResponse, GroupAbort, GroupCfg, GroupCommit, GroupCommitFailed,
    GroupReserveAck, GroupReserveDeny, GroupReserveFrame, GroupSpawnError, GroupSpawned,
    GroupTransport, ReserveResponse,
};

fn id(n: u8) -> NodeId {
    NodeId([n; 16])
}

fn gpu_node(node_id: NodeId, vram_mb: u64) -> NodeAd {
    let mut a = NodeAd::default();
    a.node_id = node_id;
    a.hostname = "host".into();
    a.cpu.cores = 32;
    a.cpu.threads = 64;
    a.cpu.arch = "x86_64".into();
    a.mem.total_mb = 65_536;
    a.mem.free_mb = 65_536;
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

fn member(label: &str, req_src: &str) -> GroupMember {
    GroupMember {
        label: label.into(),
        req: parse_req(req_src).expect("test predicate must parse"),
        requires_src: req_src.into(),
        argv: vec![],
        env: vec![],
    }
}

fn cfg() -> GroupCfg {
    GroupCfg {
        reserve_timeout: Duration::from_millis(200),
        commit_timeout: Duration::from_millis(500),
    }
}

/// Per-node behavior the test wants to inject.
enum Behavior {
    AckThenSpawn,
    AckThenCommitFail(String),
    Deny(String),
    Silent, // timeout
}

/// Records calls so tests can assert compensating abort/kill emissions.
#[derive(Default)]
struct Calls {
    aborts: Vec<NodeId>,
    kills: Vec<NetId>,
}

struct MockTransport {
    plan: HashMap<NodeId, Behavior>,
    calls: Mutex<Calls>,
}

impl MockTransport {
    fn new(plan: Vec<(NodeId, Behavior)>) -> Self {
        Self {
            plan: plan.into_iter().collect(),
            calls: Mutex::new(Calls::default()),
        }
    }
}

#[async_trait]
impl GroupTransport for MockTransport {
    async fn reserve(&self, node: NodeId, frame: GroupReserveFrame) -> ReserveResponse {
        match self.plan.get(&node).expect("unmocked node") {
            Behavior::Silent => {
                // Block forever — let the coord's timeout fire.
                std::future::pending::<()>().await;
                unreachable!()
            }
            Behavior::Deny(reason) => ReserveResponse::Deny(GroupReserveDeny {
                group_id: frame.group_id,
                reason: reason.clone(),
            }),
            Behavior::AckThenSpawn | Behavior::AckThenCommitFail(_) => {
                let tokens = frame
                    .members
                    .iter()
                    .enumerate()
                    .map(|(i, m)| (m.label.clone(), i as u64 + 1000))
                    .collect();
                ReserveResponse::Ack(GroupReserveAck {
                    group_id: frame.group_id,
                    tokens,
                })
            }
        }
    }

    async fn commit(&self, node: NodeId, frame: GroupCommit) -> CommitResponse {
        match self.plan.get(&node).expect("unmocked node") {
            Behavior::AckThenSpawn => {
                let spawns = frame
                    .tokens
                    .iter()
                    .enumerate()
                    .map(|(i, (label, _))| GroupSpawned {
                        group_id: frame.group_id,
                        label: label.clone(),
                        net_id: NetId {
                            node,
                            mbox: MboxId((i as u64) + 100),
                        },
                    })
                    .collect();
                CommitResponse::Spawned(spawns)
            }
            Behavior::AckThenCommitFail(reason) => CommitResponse::Failed(GroupCommitFailed {
                group_id: frame.group_id,
                reason: reason.clone(),
            }),
            _ => unreachable!("commit called on a non-ack'd node"),
        }
    }

    async fn abort(&self, node: NodeId, _frame: GroupAbort) {
        self.calls.lock().unwrap().aborts.push(node);
    }

    async fn kill(&self, target: NetId) {
        self.calls.lock().unwrap().kills.push(target);
    }
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn happy_path_returns_all_netids() {
    // SPREAD across 3 nodes: every node acks and spawns its single member.
    let group = PlacementGroup {
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
    let transport = MockTransport::new(vec![
        (id(1), Behavior::AckThenSpawn),
        (id(2), Behavior::AckThenSpawn),
        (id(3), Behavior::AckThenSpawn),
    ]);
    let out = submit_group(&group, &ads, &cfg(), &transport).await.unwrap();
    assert_eq!(out.members.len(), 3);
    assert_eq!(out.members[0].0, "a");
    assert_eq!(out.members[1].0, "b");
    assert_eq!(out.members[2].0, "c");
    // No compensating actions.
    let calls = transport.calls.lock().unwrap();
    assert!(calls.aborts.is_empty(), "no abort expected on happy path");
    assert!(calls.kills.is_empty(), "no kill expected on happy path");
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn reserve_timeout_aborts_others() {
    // 3 nodes; node 2 silent. Phase-1 timeout -> abort other acked nodes.
    let group = PlacementGroup {
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
    let transport = MockTransport::new(vec![
        (id(1), Behavior::AckThenSpawn),
        (id(2), Behavior::Silent),
        (id(3), Behavior::AckThenSpawn),
    ]);
    let err = submit_group(&group, &ads, &cfg(), &transport).await.unwrap_err();
    match err {
        GroupSpawnError::ReserveTimeout { node, ms } => {
            assert_eq!(node, id(2));
            assert_eq!(ms, 200);
        }
        other => panic!("expected ReserveTimeout, got {other:?}"),
    }
    let calls = transport.calls.lock().unwrap();
    // Acked nodes (1 and 3) get aborted; the silent node (2) is left to its
    // own TTL sweep.
    let aborted: std::collections::HashSet<NodeId> = calls.aborts.iter().copied().collect();
    assert!(aborted.contains(&id(1)));
    assert!(aborted.contains(&id(3)));
    assert!(!aborted.contains(&id(2)));
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn reserve_deny_aborts_others() {
    let group = PlacementGroup {
        strategy: GroupStrategy::Spread,
        members: vec![
            member("a", "any(gpu, gpu.vendor == 0x10de)"),
            member("b", "any(gpu, gpu.vendor == 0x10de)"),
        ],
    };
    let ads = vec![gpu_node(id(1), 80_000), gpu_node(id(2), 80_000)];
    let transport = MockTransport::new(vec![
        (id(1), Behavior::AckThenSpawn),
        (id(2), Behavior::Deny("cap exhausted".into())),
    ]);
    let err = submit_group(&group, &ads, &cfg(), &transport).await.unwrap_err();
    match err {
        GroupSpawnError::ReserveDenied { node, reason } => {
            assert_eq!(node, id(2));
            assert_eq!(reason, "cap exhausted");
        }
        other => panic!("expected ReserveDenied, got {other:?}"),
    }
    let calls = transport.calls.lock().unwrap();
    assert_eq!(calls.aborts, vec![id(1)]);
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn commit_failure_kills_spawned() {
    // 3 nodes; all ack; node 2's commit fails. Coord kills siblings'
    // already-spawned NetIds and returns CommitFailed.
    let group = PlacementGroup {
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
    let transport = MockTransport::new(vec![
        (id(1), Behavior::AckThenSpawn),
        (id(2), Behavior::AckThenCommitFail("execve failed".into())),
        (id(3), Behavior::AckThenSpawn),
    ]);
    let err = submit_group(&group, &ads, &cfg(), &transport).await.unwrap_err();
    match err {
        GroupSpawnError::CommitFailed { node, reason } => {
            assert_eq!(node, id(2));
            assert_eq!(reason, "execve failed");
        }
        other => panic!("expected CommitFailed, got {other:?}"),
    }
    let calls = transport.calls.lock().unwrap();
    // Two siblings spawned successfully → both killed.
    assert_eq!(calls.kills.len(), 2);
    let killed_nodes: std::collections::HashSet<NodeId> =
        calls.kills.iter().map(|n| n.node).collect();
    assert!(killed_nodes.contains(&id(1)));
    assert!(killed_nodes.contains(&id(3)));
}
