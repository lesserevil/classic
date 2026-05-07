# Feature: Placement groups (PACK / SPREAD)

> **Status:** draft
> **Epic bead:** `bd-XXX` (filed after this doc lands)
> **Owner:** unassigned
> **Last updated:** 2026-05-07

## Scope

A **placement group** is an atomic bundle of related processes spawned together
under a co-placement strategy. v1 supports two strategies:

- `PACK` — every member placed on the same node.
- `SPREAD` — every member placed on a different node (one member per distinct
  `NodeId`).

Each member carries its own predicate `Requirement`, argv, and env, so members
may have different hardware needs (e.g. one rank-0 trainer needing an 80 GB
GPU plus N workers needing 40 GB GPUs).

**In scope**

- `PlacementGroup`, `GroupMember`, `GroupStrategy` types in `classic-place`.
- Pure `place_group(&PlacementGroup, &[NodeAd]) -> Result<Vec<(String,
  NodeId)>, GroupPlaceError>` that returns a complete assignment or an error —
  never partial.
- Two-phase commit (2PC) spawn protocol layered on plan 04's frames. New
  frame kinds allocated from the `0x0300–0x03FF` range owned by `classic-spawn`.
  TTL'd reservations (10 s default) so failed coordinators cannot strand caps.
- CLI: `classic submit <group.toml>` reads a group file, resolves placement
  against fresh ads, drives 2PC, prints `{label} -> {node_id}` per member.
- Lifecycle: a group either fully spawns or fully fails. After commit, members
  are independent plain-spawn processes (plan 04 semantics). No post-commit
  group tracking.

**Out of scope (v1)**

- Cross-group bin-packing optimization (greedy first-fit per group; submit
  order wins).
- Group respawn on member crash (let it propagate, user re-submits).
- Anti-affinity beyond `SPREAD` (no "not on these specific nodes",
  "co-locate with task X").
- Group quotas, fair-share.
- Topology-aware strategies (`PACK` within rack, `SPREAD` across racks).
- Persisting group identity across daemon restarts.

## Reasoning

ML jobs frequently spawn N processes that must land together: tensor-parallel
trainers `PACK` on one DGX so NVLink stays on-fabric; data-parallel workers
`SPREAD` so each rank gets its own node bandwidth. Plan 04's single-process
spawn cannot express this — racing N spawns can land 7-of-8 ranks before
discovering the 8th has no home, leaving 7 zombies and orphaned device caps.

**Alternatives considered**

1. *No group primitive — let users retry.* Rejected: races between parallel
   spawns and partial failures make this miserable to operate; user-space
   retry cannot reserve caps atomically.
2. *Optimistic spawn, kill-on-failure.* Rejected: leaks resources during the
   gap, complex partial-kill edge cases, disrupts other tenants competing for
   the same caps.
3. *Coordinator forks siblings via local fork (mpirun-style).* Rejected: cannot
   fork onto another node, so it can't do `SPREAD`. `PACK`-only is a strict
   subset of what we need.
4. *Two-phase commit over plan 04 spawn frames.* **Chosen.** Reservations are
   cheap (caps + slot accounting); failure modes are well-understood;
   timeout-bounded reservations bound worst-case resource hold. Same shape
   as Ray placement groups.

**Success in plain English:** a user writes `group.toml` with N members and a
strategy, runs `classic submit group.toml`, and either every member is running
(CLI prints assignments) or nothing is running (CLI prints why). No
half-spawns. No leaked GPU caps.

## Design

### Architecture

Group placement spans two crates:

- `classic-place` — pure placement algorithm. Given a group + ad snapshot,
  returns complete assignment or an error. No I/O.
- `classic-spawn` — 2PC orchestrator. Drives the protocol against remote
  daemons via new frames; reuses plan 04's local-spawn path inside phase 2.

`classic-cli` parses TOML and calls into both.

```
classic submit ──► classic-cli (parse TOML)
                       │ PlacementGroup
                       ▼
                  classic-place::place_group()
                       │ Vec<(label, NodeId)>
                       ▼
                  classic-spawn::submit_group()  (2PC)
              GroupReserve / GroupCommit / GroupAbort
                       ▼
                  classicd (per-node)
                  reservation table + plan 04 spawn
```

Sequence (success path, 3-member SPREAD across nodes A/B/C):

```
coord            A           B           C
  │ Reserve ──►  │           │           │
  │ Reserve ─────────────►   │           │
  │ Reserve ─────────────────────────►   │
  │ ◄── Ack ──┤  │           │           │
  │ ◄── Ack ─────────────────┤           │
  │ ◄── Ack ─────────────────────────────┤
  │ Commit ──► ─────────► ─────────►
  │ ◄── Spawned(NetId) per node (plan 04 frames)
```

### Data shapes

```rust
// classic-place
pub struct GroupMember {
    pub label: String,                  // unique within group; key in result
    pub req: Requirement,               // plan 03 predicate
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,     // ordered; later wins on duplicate key
}
pub enum GroupStrategy { Pack, Spread }
pub struct PlacementGroup {
    pub strategy: GroupStrategy,
    pub members: Vec<GroupMember>,
}

#[derive(thiserror::Error, Debug)]
pub enum GroupPlaceError {
    #[error("empty placement group")]                                    Empty,
    #[error("duplicate member label: {0}")]                              DuplicateLabel(String),
    #[error("PACK: no single node satisfies all {0} member requirements")] PackInfeasible(usize),
    #[error("SPREAD: cluster has {nodes} nodes, group needs {needed}")]  SpreadTooLarge { needed: usize, nodes: usize },
    #[error("SPREAD: no one-member-per-node assignment satisfies all requirements")] SpreadInfeasible,
    #[error("member {label}: no node matches requirement")]              MemberInfeasible { label: String },
}

pub fn place_group(group: &PlacementGroup, ads: &[NodeAd])
    -> Result<Vec<(String, NodeId)>, GroupPlaceError>;
```

```rust
// classic-spawn
#[derive(Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Debug)]
pub struct GroupId(pub [u8; 16]);     // random per submit; echoed in every frame

#[derive(thiserror::Error, Debug)]
pub enum GroupSpawnError {
    #[error("placement: {0}")]                                Place(#[from] GroupPlaceError),
    #[error("phase 1: node {node:?} denied: {reason}")]       ReserveDenied { node: NodeId, reason: String },
    #[error("phase 1: node {node:?} timed out after {ms} ms")] ReserveTimeout { node: NodeId, ms: u64 },
    #[error("phase 2: commit failed on {node:?}: {reason}")]  CommitFailed { node: NodeId, reason: String },
    #[error("transport: {0}")]                                Transport(String),
}

pub struct GroupSubmitResult {
    pub group_id: GroupId,
    pub members: Vec<(String, NetId)>,   // label -> live NetId
}

pub struct GroupCfg {
    pub reserve_timeout: Duration,       // default 10 s
    pub commit_timeout: Duration,        // default 30 s (covers exec)
}

pub async fn submit_group(group: &PlacementGroup, ads: &[NodeAd], cfg: &GroupCfg)
    -> Result<GroupSubmitResult, GroupSpawnError>;
```

### Wire format additions

Plan 04 owns `0x0300..=0x030F` for single-spawn and reserves
`0x0310..=0x031F` for future single-spawn extensions. **Group frames
allocate `0x0320..=0x0327`** within the same `classic-spawn` band.

| Kind   | Name                | Direction         | Purpose                                             |
|--------|---------------------|-------------------|-----------------------------------------------------|
| 0x0320 | `GroupReserve`      | coord → node      | Reserve caps + slots for members assigned here      |
| 0x0321 | `GroupReserveAck`   | node → coord      | Reservation accepted; held until commit/abort/TTL   |
| 0x0322 | `GroupReserveDeny`  | node → coord      | Reservation refused (cap exhausted, etc.)           |
| 0x0323 | `GroupCommit`       | coord → node      | Promote reservations → live spawns                  |
| 0x0324 | `GroupSpawned`      | node → coord      | Per member: plan 04 spawn succeeded; carries NetId  |
| 0x0325 | `GroupCommitFailed` | node → coord      | Phase-2 exec failure on this node                   |
| 0x0326 | `GroupAbort`        | coord → node      | Drop reservations (idempotent)                      |
| 0x0327 | `GroupAbortAck`     | node → coord      | Reservations released                               |

`GroupReserve` payload (bincode v2, fixed-int LE):

```rust
struct GroupReserveFrame {
    group_id: GroupId,
    members: Vec<ReservedMember>,    // members assigned to *this* node
    reserve_ttl_ms: u32,             // node-side timer; coord adds slack
}
struct ReservedMember {
    label: String,
    req: Requirement,                // node revalidates locally
    argv: Vec<String>,
    env: Vec<(String, String)>,
}
```

`GroupReserveAck` carries `(GroupId, Vec<(label, /*reservation_token*/ u64)>)`.
The token is opaque; coord echoes it in `GroupCommit`.

### Group placement algorithm

Inputs: `group`, plus ads filtered to non-drained, recently heartbeating
nodes (same filter as plan 03's `place()`).

Validation: empty members → `Empty`; duplicate labels → `DuplicateLabel`.

#### PACK — every member on the same node

```text
fn place_pack(group, ads) -> Result<...>:
    candidates = ads.sorted_by(rank_pack_desc)   // most idle GPU mem first;
                                                 // tiebreak by node_id for determinism
    for ad in candidates:
        if let Some(picks) = try_pack_on(ad, &group.members):
            return Ok(zip(labels, repeat(ad.node_id)))
    Err(PackInfeasible(group.members.len()))

fn try_pack_on(ad, members) -> Option<Vec<Picked>>:
    // Mutable copy of the ad's free pool: per-GPU index in-use vs. free,
    // free CPU slots, free RAM bytes. Deducting after each pick is what
    // handles intra-group contention — without it, two members both
    // claiming the lone 80 GB GPU would both succeed against the
    // unmodified ad.
    pool = ad.free_pool().clone()
    chosen = []
    for m in members:                            // declaration order — first wins
        match place_one_in_pool(&m.req, &pool):
            Some(picked) => { pool.deduct(&picked); chosen.push(picked) }
            None         => return None          // back out, try next ad
    Some(chosen)
```

`place_one_in_pool` is a thin variant of plan 03's `place_one()` that scores
against a mutable free-pool. **Member declaration order** breaks contention
ties (first-listed wins) — documented and stable. v1 does not auto-sort;
users put strictest requirements first.

#### SPREAD — every member on a different node

Bipartite matching: members ↔ nodes such that each member's predicate is
satisfied by its assigned node and no two members share a node. v1 uses
greedy backtracking (fine for the ≤ 64-member groups we expect); we can swap
to Hopcroft–Karp under the same API later if profiling warrants.

```text
fn place_spread(group, ads) -> Result<...>:
    if group.members.len() > ads.len():
        return SpreadTooLarge { needed, nodes }

    // Build per-member candidate lists (nodes that satisfy that member).
    // Sort members ascending by candidate-count: minimum-remaining-values
    // heuristic from CSP solvers — prunes backtracking dramatically.
    per_member = members.map(|m| (m, candidates(m, ads)))
                        .sorted_by_key(|x| x.candidates.len())
    used: HashSet<NodeId> = {}
    assignment = []
    if backtrack(per_member, 0, &mut used, &mut assignment):
        return Ok(reorder_to(assignment, group.members))   // restore caller's order
    Err(SpreadInfeasible)

fn backtrack(per_member, i, used, assignment) -> bool:
    if i == per_member.len(): return true
    for node_id in per_member[i].candidates:
        if used.contains(&node_id): continue
        used.insert(node_id); assignment.push((label, node_id))
        if backtrack(per_member, i+1, used, assignment): return true
        assignment.pop(); used.remove(&node_id)
    false
```

### Two-phase commit orchestrator

```text
fn submit_group(group, ads, cfg):
    1. assignment = place_group(group, ads)?
    2. group_id  = random GroupId
    3. by_node   = group_by_node(assignment)            // node -> Vec<member>
    4. // Phase 1 — reserve in parallel
       results = join_all(by_node.map(|(node, members)|
         send GroupReserve { group_id, members, ttl=cfg.reserve_timeout+slack };
         await Ack | Deny | timeout(cfg.reserve_timeout)))
       if any Deny or timeout:
         best-effort GroupAbort to every node that did Ack
         (nodes that didn't Ack will TTL-sweep their own reservation);
         return ReserveDenied / ReserveTimeout
    5. // Phase 2 — commit in parallel
       results = join_all_with_timeout(GroupCommit per node, cfg.commit_timeout)
       if all GroupSpawned:
         return Ok(GroupSubmitResult { group_id, members })
       else:
         best-effort Kill (plan 04 frame) to already-spawned NetIds;
         GroupAbort to nodes whose commit hadn't returned;
         return CommitFailed { node, reason }
```

### Per-node reservation table (`classicd`)

```rust
struct Reservation {
    group_id: GroupId,
    members: Vec<ReservedMemberSlot>,    // device caps + slot accounting held
    deadline: Instant,                   // now + reserve_ttl
    state: ReservationState,             // Held | Committing | Released
}
```

- `GroupReserve`: revalidate every member's `Requirement` against current
  state, attempt to reserve caps + slots. On success push into a
  `HashMap<GroupId, Reservation>` and reply `Ack`. On failure (cap
  exhausted, predicate no longer satisfied, slot limit) reply `Deny` with
  reason.
- `GroupCommit`: locate by `GroupId`, mark `Committing`, drive plan 04's
  local-spawn path per member using the *already-reserved* caps. Reply
  `GroupSpawned` per member as exec completes; drop the reservation entry
  when all done.
- `GroupAbort`: locate, release caps + slots, reply `AbortAck`. Idempotent
  (release of `Released` is a no-op).
- **TTL sweeper thread** wakes every 1 s; any reservation past `deadline` is
  auto-released as if `GroupAbort` had arrived. This is the leak-prevention
  net.

### Failure-mode analysis

**F1 — Phase 1 ack-timeout from a node.** `reserve_timeout` (10 s) fires; coord
sends `GroupAbort` to nodes that did Ack. The slow node either (a) eventually
Acks late — coord ignores; node's TTL sweeper releases since no Commit
arrives; or (b) is partitioned and never recovers — TTL sweep still cleans
up. **No leak possible.**

**F2 — Phase 1 `GroupReserveDeny`.** Some other tenant grabbed a cap between
`place_group()` and `GroupReserve`. Coord aborts the rest, returns
`ReserveDenied { node, reason }`. User retries; place fresh.

**F3 — Coord crashes between `GroupReserve` send and ack/timeout.** Each
daemon's reservation simply ages out via the TTL sweeper after
`reserve_ttl_ms` (~12 s including slack). **No leak.** This is the
load-bearing reason every reservation has a TTL.

**F4 — Phase 2 `GroupCommitFailed` after exec error.** Some members may
already be running on other nodes when the failure arrives. Coord (a) records
the failure, (b) sends `Kill` (plan 04) to each spawned member's `NetId`, (c)
sends `GroupAbort` to nodes whose commit hadn't returned, (d) returns
`CommitFailed { node, reason }`. Brief execution of victim processes is
unavoidable — exec already succeeded — but no resources are stranded.

**F5 — Coord crashes mid-commit.** Daemons that received `GroupCommit`
proceed to spawn; daemons that didn't, TTL out. Already-spawned members
become orphaned independent tasks — same as if the user had run `classic
spawn` and crashed the CLI. Plan 04's `ChildExit` semantics apply. Group
atomicity is **not** preserved across coord crash; this is documented.
Stronger guarantees are an explicit non-goal for v1.

**F6 — Reservation expires *exactly* as `GroupCommit` arrives.** Resolved at
the daemon under a mutex: if state is `Released`, reply
`GroupCommitFailed { reason: "ttl expired" }`; coord treats this like F4.

**F7 — Duplicate `GroupReserve` for the same `GroupId` on the same node.**
Should not happen (coord groups members by node), but if a buggy coord sends
two: the second is rejected with `Deny { reason: "duplicate group_id" }`.

### Example `group.toml`

```toml
strategy = "pack"

[[member]]
label    = "trainer"
requires = "any(gpu, gpu.vram_mb >= 80000)"
argv     = ["./train", "--rank=0"]
env      = [["NCCL_DEBUG", "INFO"], ["RANK", "0"]]

[[member]]
label    = "worker"
requires = "any(gpu, gpu.vram_mb >= 40000)"
argv     = ["./worker"]
```

```bash
classic submit group.toml
# trainer  -> 6f3a-...
# worker   -> 6f3a-...        (same node — strategy=pack)
```

### Comparison sidebar — Ray vs. SLURM vs. Classic

| Aspect              | Ray placement groups            | SLURM heterogeneous jobs           | Classic (this doc)              |
|---------------------|---------------------------------|------------------------------------|---------------------------------|
| Atomicity           | Yes (gang scheduling)           | Yes (single allocation)            | Yes (2PC)                       |
| Granularity         | Resource bundles → actors       | Per-component `--ntasks`/`--gres`  | Per-member predicate + argv     |
| Strategies          | PACK / SPREAD / STRICT_*        | Constraint expressions             | PACK, SPREAD (v1)               |
| Predicate language  | Resource dict (`{"GPU": 1}`)    | `--constraint` expression          | Plan 03 DSL (`any(gpu, ...)`)   |
| Reservation hold    | GCS bundle lease                | Allocation persists till job end   | TTL'd 2PC reservation (10 s)    |
| Failure model       | Bundle stuck PENDING → cancel   | Job fails to start                 | Fully spawn or fully fail       |
| Per-member heterogeneity | Yes (one bundle per actor) | Yes (`:` separates components)    | Yes (each member's own req)     |
| Topology/rack-aware | Custom strategies               | NodeSet / topology features        | Out of scope v1                 |

The shape — atomic gang scheduling with PACK/SPREAD — is shared. Classic's
distinguishing point is that the predicate language is the same as plan 03's
single-spawn DSL, so users carry one mental model from `classic spawn` to
`classic submit`.

### File / crate layout

```
crates/
  classic-place/
    src/
      lib.rs          # MODIFIED — re-export group types
      group.rs        # NEW — PlacementGroup, GroupMember, GroupStrategy,
                      #       place_group, place_pack, place_spread
      free_pool.rs    # NEW — mutable free-pool used by PACK
  classic-spawn/
    src/
      lib.rs          # MODIFIED — re-export submit_group, GroupId
      group_proto.rs  # NEW — GroupReserve/Ack/Deny/Commit/Spawned/Abort/AbortAck
      coord.rs        # NEW — submit_group async orchestrator
      reservation.rs  # NEW — per-node reservation table + TTL sweeper
  classic-cli/
    src/
      submit.rs       # NEW — `classic submit <group.toml>` subcommand
      group_toml.rs   # NEW — TOML schema → PlacementGroup
  classic-proto/
    src/
      frame.rs        # MODIFIED — add 0x0320..0x0327 to FrameKind
```

## Requirements

### Functional

- [ ] FR-1: `place_group` returns a complete assignment or a strategy-specific
  error; never partial.
- [ ] FR-2: PACK satisfies all members on a single node, accounting for
  intra-group contention via free-pool deduction.
- [ ] FR-3: SPREAD assigns each member to a distinct node, exhaustive
  backtracking with MRV ordering.
- [ ] FR-4: `submit_group` either spawns every member or leaves the cluster
  with no live members from this group and no held reservations.
- [ ] FR-5: TTL sweeper releases any reservation older than `reserve_ttl_ms`
  even if coord never sends abort/commit.
- [ ] FR-6: `GroupAbort` is idempotent.
- [ ] FR-7: CLI `classic submit <group.toml>` parses the TOML schema, calls
  `submit_group`, and exits non-zero on failure with a human-readable message
  including the failing member label / node ID.
- [ ] FR-8: Member labels are unique; duplicate labels rejected before any
  network I/O.
- [ ] FR-9: Member declaration order breaks PACK contention ties
  (first-listed wins).

### Non-functional

- **Performance:** `place_group` for a 16-member group on a 100-node cluster
  completes in <100 ms (pure CPU). 2PC submit end-to-end <1 s on a healthy
  cluster.
- **Compatibility:** Linux 6.1+, x86_64 primary. Same as plan 04.
- **Security:** Trusted cluster; no auth on group frames (v1 baseline). A
  malicious node can spam fake `Deny`s but cannot leak other groups'
  resources.
- **Hardware:** Group placement itself does not require GPUs; tests use
  synthetic ad fixtures.

## Testing plan

### Unit (`classic-place`)

- `pack_single_node_satisfies_all` — 2 members, 1 node with 2 GPUs: both
  placed.
- `pack_intragroup_contention` — 2 members each requiring 80 GB GPU; node has
  one 80 GB + one 40 GB: `PackInfeasible`.
- `pack_first_member_wins_contention` — 2 members each requiring "any GPU";
  node has one 80 GB + one 40 GB: first gets 80 GB, second gets 40 GB.
- `spread_too_large` — 5 members, 3 nodes: `SpreadTooLarge`.
- `spread_one_per_node` — 3 members, 3 distinct nodes: 1:1 assignment.
- `spread_backtrack_required` — A matches only X; B matches X or Y; greedy
  without backtrack would assign B→X and fail A; solver must backtrack.
- `spread_infeasible_via_matching` — bipartite graph has no perfect matching:
  `SpreadInfeasible`.
- `duplicate_label`, `empty_group` validation paths.

### Unit (`classic-spawn`)

- `coord::reserve_timeout_aborts_others` — fake transport: 3 nodes, node 2
  silent. After timeout coord aborts 1 and 3; returns `ReserveTimeout`.
- `coord::reserve_deny_aborts_others` — node 2 sends `Deny`; coord aborts 1.
- `coord::commit_failure_kills_spawned` — phase 1 ok; node 2 returns
  `CommitFailed`; coord sends `Kill` to nodes 1 and 3's NetIds.
- `reservation::ttl_sweep_releases_held_caps` — install reservation, advance
  test clock past `ttl`, sweeper drops; re-reserving same caps succeeds.
- `reservation::commit_after_ttl_returns_failed` — race: commit arrives
  after sweep released; reply `CommitFailed { reason: "ttl expired" }`.
- `reservation::abort_idempotent` — two `GroupAbort`s, both Ack.

### Integration (single host, in-process daemons)

- `it_pack_2_members_one_node` — 1 in-proc daemon, 2-member PACK; both spawn.
- `it_spread_3_members_3_nodes` — 3 in-proc daemons; 1 member per daemon.
- `it_atomic_failure_no_leaked_caps` — 2 daemons; second configured to deny
  `GroupReserve`. Submit fails: assert zero spawns and daemon-1's cap pool
  back to original.
- `it_coord_crash_ttl_recovers` — submit, kill coord between Ack and Commit,
  wait `reserve_ttl + 1 s`: assert no held reservations, no spawns.

### End-to-end (multi-node, manual)

Two physical hosts running `classicd`:

```bash
# host A
classicd --bind 0.0.0.0:7000 --peers <hostB>:7000 &
# host B
classicd --bind 0.0.0.0:7000 --peers <hostA>:7000 &

# operator
cat > spread.toml <<'TOML'
strategy = "spread"
[[member]]
label="a"; requires="any(gpu)"; argv=["/usr/bin/sleep","5"]
[[member]]
label="b"; requires="any(gpu)"; argv=["/usr/bin/sleep","5"]
TOML
classic submit spread.toml
# expect: a -> <nodeA>, b -> <nodeB>  (or the swap)
```

### Hardware-dependent

Placement-algorithm tests are pure (synthetic ads). Multi-node e2e exercises
plan 04's NVML cap path under reservation: real GPUs required. Mark e2e
`#[ignore]`, gate behind `CLASSIC_E2E_GPU=1` in CI.

## Acceptance criteria

- [ ] AC-1: A 2-member PACK group with `any(gpu)` lands both members on the
  same node, observable via `classic ps` showing identical `node_id`.
- [ ] AC-2: A 3-member SPREAD group lands one member on each of three
  distinct nodes; `node_id`s are pairwise unique.
- [ ] AC-3: When phase 1 fails on any node, **zero** members of the group
  are spawned anywhere in the cluster (verified via `classic ps`).
- [ ] AC-4: When the coordinator is `kill -9`'d after phase-1 acks but
  before phase-2 commit, all daemons release their held caps within
  `reserve_ttl + 2 s` (verified via cap-pool inspection in `classic
  node-info`).
- [ ] AC-5: `place_group` returns the expected `GroupPlaceError` variant for
  each failure mode covered by unit tests.
- [ ] AC-6: `classic submit nonexistent.toml` exits non-zero with a clean
  message; `classic submit malformed.toml` reports the parse error.
- [ ] AC-7: All unit + integration tests in the testing plan pass on Linux
  x86_64.
- [ ] AC-8: No new clippy warnings; `cargo doc` builds clean for every
  modified crate.

## Open questions

- **Q1 (deferred to post-v1):** Add Ray-style `STRICT_PACK` /
  `STRICT_SPREAD` distinguishing best-effort fallback? v1 is strict-only.
- **Q2 (resolved):** PACK contention tie-break = member declaration order
  (first wins). Documented in CLI help.
- **Q3 (deferred):** SPREAD scaling beyond ~64 members. v1 backtracking is
  acceptable; swap to Hopcroft–Karp under the same API if profiling
  warrants.
- **Q4 (deferred):** Should `submit_group` return a `GroupId` that survives
  to a `classic group-status <id>` command? v1 says no — once committed the
  group dissolves into independent tasks. A future "group-aware supervision"
  feature would reintroduce it.
- **Q5 (open):** Default `reserve_timeout` value. 10 s is right for LAN; do
  we expose `--reserve-timeout` on the CLI for slow links? Lean yes, low
  cost. Track in implementation task.

## References

- `plans/ARCHITECTURE.md` — wire-format ranges, identity types, transport.
- `plans/03-placement-predicates.md` — `Requirement` type, `place()`, the
  predicate DSL.
- `plans/04-spawn-pipeline.md` — single-spawn frames `0x0300..=0x030F`,
  cap/cgroup machinery reused by phase 2.
- Ray placement groups —
  https://docs.ray.io/en/latest/ray-core/scheduling/placement-group.html
- SLURM heterogeneous jobs —
  https://slurm.schedmd.com/heterogeneous_jobs.html
- Gray, J. & Lampson, B., "Notes on Database Operating Systems" (1978) —
  origin of two-phase commit; the failure analysis structure here mirrors
  the classical 2PC failure cases.
