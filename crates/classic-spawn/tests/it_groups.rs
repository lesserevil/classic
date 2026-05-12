//! Plan-07 acceptance-criteria integration tests.
//!
//! These exercise the full `submit_group` + `ReservationTable` path
//! through an in-process transport. The transport routes each
//! per-node request to a `NodeFixture` that holds a real
//! `ReservationTable`, runs the same revalidation + token issuance
//! as the production node-side handler will, and on commit hands
//! back synthetic `NetId`s (one per member) so the coord can
//! aggregate. Plan-04's real exec path is exercised by
//! `spawn_bridge_e2e`; here we test the group-2PC state machine
//! end-to-end.
//!
//! Wire framing (frame.rs encode/decode) is covered by the round-trip
//! tests in `group_proto::tests`; this file deliberately skips the
//! wire and connects coord -> table directly so failure modes can be
//! asserted without standing up real sockets.
//!
//! Coverage map:
//! - AC-1: it_pack_2_members_one_node
//! - AC-2: it_spread_3_members_3_nodes
//! - AC-3: it_atomic_failure_no_leaked_caps
//! - AC-4: it_coord_crash_ttl_recovers

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use classic_place::{
    parse_req, GpuAd, GroupMember, GroupStrategy, NodeAd, PlacementGroup,
};
use classic_proto::{MboxId, NetId, NodeId};
use classic_spawn::{
    submit_group, CommitOutcome, CommitResponse, GroupAbort, GroupCfg, GroupCommit,
    GroupCommitFailed, GroupReserveAck, GroupReserveDeny, GroupReserveFrame, GroupSpawnError,
    GroupSpawned, GroupTransport, ReserveOutcome, ReserveResponse, ReservationTable,
};

fn id(n: u8) -> NodeId {
    NodeId([n; 16])
}

fn gpu_node(node_id: NodeId, n_gpus: u32) -> NodeAd {
    let mut a = NodeAd::default();
    a.node_id = node_id;
    a.hostname = format!("node-{:02x}", node_id.0[0]);
    a.cpu.cores = 32;
    a.cpu.threads = 64;
    a.cpu.arch = "x86_64".into();
    a.mem.total_mb = 65_536;
    a.mem.free_mb = 65_536;
    a.gpu = (0..n_gpus)
        .map(|i| GpuAd {
            index: i,
            vendor: 0x10de,
            device: 0x2330,
            model: "h100".into(),
            vram_mb: 80_000,
            vram_free_mb: 80_000,
            sm_count: 132,
            in_use: false,
            mig: false,
        })
        .collect();
    a
}

fn member(label: &str, req_src: &str) -> GroupMember {
    GroupMember {
        label: label.into(),
        req: parse_req(req_src).expect("test predicate must parse"),
        requires_src: req_src.into(),
        argv: vec!["/bin/true".into()],
        env: vec![],
    }
}

/// One simulated node. Holds its own reservation table, an ad-id
/// counter (next NetId for spawned children), and a "policy" knob
/// that lets the test inject Phase-1 denials.
struct NodeFixture {
    node_id: NodeId,
    table: ReservationTable,
    next_mbox: Mutex<u64>,
    spawned_count: Mutex<u32>,
    deny_reserve: Mutex<Option<String>>,
    /// `false` -> Reserve and Commit are answered. `true` -> coord is
    /// considered "dropped" partway; reserves still ack, commits never
    /// arrive. Lets the TTL-recovery test simulate F3.
    drop_after_reserve: Mutex<bool>,
}

impl NodeFixture {
    fn new(node_id: NodeId) -> Arc<Self> {
        Arc::new(Self {
            node_id,
            table: ReservationTable::new(),
            next_mbox: Mutex::new(1),
            spawned_count: Mutex::new(0),
            deny_reserve: Mutex::new(None),
            drop_after_reserve: Mutex::new(false),
        })
    }

    fn next_netid(&self) -> NetId {
        let mut g = self.next_mbox.lock().unwrap();
        let mbox = *g;
        *g += 1;
        NetId {
            node: self.node_id,
            mbox: MboxId(mbox),
        }
    }
}

#[derive(Clone)]
struct InProcessTransport {
    nodes: HashMap<NodeId, Arc<NodeFixture>>,
    /// Records compensating actions so tests can assert them.
    calls: Arc<Mutex<Calls>>,
}

#[derive(Default)]
struct Calls {
    aborts: Vec<NodeId>,
    kills: Vec<NetId>,
}

#[async_trait]
impl GroupTransport for InProcessTransport {
    async fn reserve(&self, node: NodeId, frame: GroupReserveFrame) -> ReserveResponse {
        let f = self.nodes.get(&node).expect("unknown node");
        // Injected deny short-circuit.
        if let Some(reason) = f.deny_reserve.lock().unwrap().clone() {
            return ReserveResponse::Deny(GroupReserveDeny {
                group_id: frame.group_id,
                reason,
            });
        }
        match f.table.reserve(&frame, Instant::now(), |_src| true) {
            ReserveOutcome::Accepted(tokens) => ReserveResponse::Ack(GroupReserveAck {
                group_id: frame.group_id,
                tokens,
            }),
            ReserveOutcome::Denied(reason) => ReserveResponse::Deny(GroupReserveDeny {
                group_id: frame.group_id,
                reason,
            }),
        }
    }

    async fn commit(&self, node: NodeId, frame: GroupCommit) -> CommitResponse {
        let f = self.nodes.get(&node).expect("unknown node");
        if *f.drop_after_reserve.lock().unwrap() {
            // Coord-crash simulation: block indefinitely. The coord's
            // commit_timeout will fire.
            std::future::pending::<()>().await;
            unreachable!()
        }
        match f.table.commit(frame.group_id, &frame.tokens) {
            CommitOutcome::Proceed(slots) => {
                let mut out = Vec::with_capacity(slots.len());
                for slot in slots {
                    *f.spawned_count.lock().unwrap() += 1;
                    out.push(GroupSpawned {
                        group_id: frame.group_id,
                        label: slot.label,
                        net_id: f.next_netid(),
                    });
                }
                let _ = f.table.commit_done(frame.group_id);
                CommitResponse::Spawned(out)
            }
            CommitOutcome::Failed(reason) => CommitResponse::Failed(GroupCommitFailed {
                group_id: frame.group_id,
                reason,
            }),
        }
    }

    async fn abort(&self, node: NodeId, frame: GroupAbort) {
        if let Some(f) = self.nodes.get(&node) {
            // Simulate a crashed coord whose aborts never reach the
            // node: when drop_after_reserve is set, the network
            // "loses" the abort. The TTL sweeper is then the only
            // cleanup path.
            if *f.drop_after_reserve.lock().unwrap() {
                self.calls.lock().unwrap().aborts.push(node);
                return;
            }
            let _ = f.table.abort(frame.group_id);
        }
        self.calls.lock().unwrap().aborts.push(node);
    }

    async fn kill(&self, target: NetId) {
        self.calls.lock().unwrap().kills.push(target);
    }
}

fn cfg_fast() -> GroupCfg {
    GroupCfg {
        reserve_timeout: Duration::from_millis(200),
        commit_timeout: Duration::from_millis(500),
    }
}

// ---------- AC-1 ----------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn it_pack_2_members_one_node() {
    // Single in-process daemon with 2 GPUs. PACK 2 members; both land
    // on the same NodeId.
    let node_a = NodeFixture::new(id(1));
    let transport = InProcessTransport {
        nodes: [(node_a.node_id, node_a.clone())].into_iter().collect(),
        calls: Arc::new(Mutex::new(Calls::default())),
    };
    let group = PlacementGroup {
        strategy: GroupStrategy::Pack,
        members: vec![
            member("trainer", "any(gpu, !gpu.in_use)"),
            member("worker", "any(gpu, !gpu.in_use)"),
        ],
    };
    let ads = vec![gpu_node(node_a.node_id, 2)];
    let out = submit_group(&group, &ads, &cfg_fast(), &transport)
        .await
        .expect("PACK happy path");
    assert_eq!(out.members.len(), 2);
    assert_eq!(out.members[0].1.node, node_a.node_id);
    assert_eq!(out.members[1].1.node, node_a.node_id);
    assert_eq!(*node_a.spawned_count.lock().unwrap(), 2);
    // Reservation should be fully cleaned up after commit_done.
    assert!(node_a.table.live_ids().is_empty());
}

// ---------- AC-2 ----------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn it_spread_3_members_3_nodes() {
    let nodes: Vec<Arc<NodeFixture>> = (1..=3).map(|i| NodeFixture::new(id(i))).collect();
    let transport = InProcessTransport {
        nodes: nodes.iter().map(|n| (n.node_id, n.clone())).collect(),
        calls: Arc::new(Mutex::new(Calls::default())),
    };
    let group = PlacementGroup {
        strategy: GroupStrategy::Spread,
        members: vec![
            member("a", "any(gpu, gpu.vendor == 0x10de)"),
            member("b", "any(gpu, gpu.vendor == 0x10de)"),
            member("c", "any(gpu, gpu.vendor == 0x10de)"),
        ],
    };
    let ads: Vec<NodeAd> = nodes.iter().map(|n| gpu_node(n.node_id, 1)).collect();
    let out = submit_group(&group, &ads, &cfg_fast(), &transport)
        .await
        .expect("SPREAD happy path");
    assert_eq!(out.members.len(), 3);
    let assigned: std::collections::HashSet<NodeId> =
        out.members.iter().map(|(_, n)| n.node).collect();
    assert_eq!(assigned.len(), 3, "all three nodes used exactly once");
    // Each daemon hosts exactly one spawn.
    for n in &nodes {
        assert_eq!(*n.spawned_count.lock().unwrap(), 1);
        assert!(n.table.live_ids().is_empty());
    }
}

// ---------- AC-3 ----------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn it_atomic_failure_no_leaked_caps() {
    let node_a = NodeFixture::new(id(1));
    let node_b = NodeFixture::new(id(2));
    // Node B denies every reserve.
    *node_b.deny_reserve.lock().unwrap() = Some("cap exhausted".into());
    let transport = InProcessTransport {
        nodes: [
            (node_a.node_id, node_a.clone()),
            (node_b.node_id, node_b.clone()),
        ]
        .into_iter()
        .collect(),
        calls: Arc::new(Mutex::new(Calls::default())),
    };
    let group = PlacementGroup {
        strategy: GroupStrategy::Spread,
        members: vec![
            member("a", "any(gpu, gpu.vendor == 0x10de)"),
            member("b", "any(gpu, gpu.vendor == 0x10de)"),
        ],
    };
    let ads = vec![gpu_node(node_a.node_id, 1), gpu_node(node_b.node_id, 1)];
    let err = submit_group(&group, &ads, &cfg_fast(), &transport)
        .await
        .expect_err("expected ReserveDenied");
    match err {
        GroupSpawnError::ReserveDenied { node, reason } => {
            assert_eq!(node, node_b.node_id);
            assert_eq!(reason, "cap exhausted");
        }
        other => panic!("expected ReserveDenied, got {other:?}"),
    }
    // Zero members spawned on either daemon.
    assert_eq!(*node_a.spawned_count.lock().unwrap(), 0);
    assert_eq!(*node_b.spawned_count.lock().unwrap(), 0);
    // Acked node (A) received a compensating Abort.
    let calls = transport.calls.lock().unwrap();
    assert!(calls.aborts.contains(&node_a.node_id));
    // No leaked reservations on A.
    assert!(node_a.table.live_ids().is_empty());
    assert!(node_b.table.live_ids().is_empty());
}

// ---------- AC-4 ----------

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn it_coord_crash_ttl_recovers() {
    // Both nodes ack Phase-1, then the coord "crashes" before
    // commit. The coord's commit_timeout fires and submit_group
    // returns CommitTimeout; the reservations remain Held on each
    // node until the TTL sweeper releases them.
    let node_a = NodeFixture::new(id(1));
    let node_b = NodeFixture::new(id(2));
    *node_a.drop_after_reserve.lock().unwrap() = true;
    *node_b.drop_after_reserve.lock().unwrap() = true;
    let transport = InProcessTransport {
        nodes: [
            (node_a.node_id, node_a.clone()),
            (node_b.node_id, node_b.clone()),
        ]
        .into_iter()
        .collect(),
        calls: Arc::new(Mutex::new(Calls::default())),
    };
    let group = PlacementGroup {
        strategy: GroupStrategy::Spread,
        members: vec![
            member("a", "any(gpu, gpu.vendor == 0x10de)"),
            member("b", "any(gpu, gpu.vendor == 0x10de)"),
        ],
    };
    let ads = vec![gpu_node(node_a.node_id, 1), gpu_node(node_b.node_id, 1)];
    let cfg = GroupCfg {
        reserve_timeout: Duration::from_millis(200),
        commit_timeout: Duration::from_millis(300),
    };
    let err = submit_group(&group, &ads, &cfg, &transport)
        .await
        .expect_err("expected CommitTimeout");
    match err {
        GroupSpawnError::CommitTimeout { .. } => {}
        other => panic!("expected CommitTimeout, got {other:?}"),
    }
    // Reservations are still Held — sweep hasn't run yet because the
    // coord's reserve_ttl_ms hasn't elapsed inside the table's clock.
    // Drive the sweep by advancing the table's clock past deadline.
    // The on-wire TTL is cfg.reserve_timeout + RESERVE_TTL_SLACK
    // (~700 ms here); we wait past that and call tick_ttl.
    let future = Instant::now() + Duration::from_secs(3);
    let released_a = node_a.table.tick_ttl(future);
    let released_b = node_b.table.tick_ttl(future);
    // Both held reservations got swept.
    assert_eq!(released_a.len() + released_b.len(), 2);
    assert!(node_a.table.live_ids().is_empty());
    assert!(node_b.table.live_ids().is_empty());
    // No children were spawned anywhere.
    assert_eq!(*node_a.spawned_count.lock().unwrap(), 0);
    assert_eq!(*node_b.spawned_count.lock().unwrap(), 0);
}
