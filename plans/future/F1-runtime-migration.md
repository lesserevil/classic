# Feature: Runtime Process Migration

> **Status:** future
> **Epic bead:** _not filed — this is a roadmap-level future-work doc_
> **Owner:** TBD (post-v1)
> **Last updated:** 2026-05-07

## Scope

v1 (per `ARCHITECTURE.md`) commits to **placement-at-fork only**: a task is
placed when it is spawned and stays there for life. This document sketches
what a follow-on release would look like if we chose to add **runtime process
migration** — the ability to move an already-running task from one node to
another without restarting it. It is roadmap-level: it does **not** generate
epics or tasks, and the v1 platform must stabilize first.

**In scope (v2):**

- Opt-in CRIU-based checkpoint/restore for cooperating processes.
- Load-balance-triggered migration: tasks move off hotspots toward
  less-loaded peers matching their predicate.
- Failure-driven migration: draining nodes (operator command, predicted
  hardware failure, NVML XID) evacuate their migratable tasks.
- CRIUgpu / `cuda-checkpoint` integration so running CUDA work survives
  the move.
- `classic migrate <task-id> <node-id>` CLI verb for explicit moves.
- A new `classic-migrate` crate plus a frame-range extension for the
  source ↔ destination handshake.

**Out of scope (deliberately, even in v2):**

- **Transparent migration of arbitrary Linux processes** (the MOSIX /
  openMosix / Kerrighed line; lessons cited under References). Classic
  tasks opt in and declare what state can survive a move.
- **Migration across CPU architectures** — CRIU does not translate code
  or vendor-specific page formats.
- **Migration of containers Classic did not spawn**, or
  **cross-major-kernel-version migration** with no overlap window.
- **Tasks holding non-NVIDIA accelerator state** (RDMA queue pairs, DPU
  contexts, FPGAs) — per-vendor checkpoint hooks don't yet exist.
- **Live `classicd` upgrade** — daemon graceful drain is a separate
  problem.

## Reasoning

### Why this is v2, not v1

1. **Implementation complexity.** Migration touches the kernel (`ptrace`,
   CRIU, `prctl`), the GPU driver (`cuda-checkpoint` lock/checkpoint/
   restore/unlock), the network stack (TCP repair sockets, IP migration
   or reconnect windows), and our task state machines (a new mid-life
   transition). v1 has *none* of that surface; layering it on before
   plan 04 (spawn pipeline) is solid means debugging two new subsystems
   against each other.
2. **Ecosystem maturity.** CRIU 4.0 (Sept 2025) is the first release with
   first-class GPU support via `cuda-checkpoint`, mainlined into the
   NVIDIA driver in 2024. Linux 6.7 (Jan 2024) is the floor for full
   CRIUgpu. Every dependency landed in the last 18 months — that's a v2
   conversation.
3. **Empirical value.** The Amoeba paper (Douglis & Ousterhout, 1991)
   found placement-at-fork captures ~80% of the value of *live*
   migration on real workloads. Classic's workload (long ML training
   jobs) is the part of the mix where the remaining 20% matters most,
   but building it before plan 04 is robust would be premature.

### The deferred-cost hypothesis, stated honestly

Placement-at-fork commits at `t=0`, when we know least. Migration buys
the ability to recommit at `t > 0`, when we know the task's actual
memory footprint (not a predicate guess), its actual GPU utilization
curve (idle vs. allreduce phases), and whether the node it landed on
is now contended or about to fail (NVML reports XID / ECC errors
minutes-to-hours before a fatal fault).

Concretely, migration buys: hotspot relief, pre-failure evacuation,
maintenance-window drains, and incrementally better packing via a
periodic policy. It does **not** buy: rescuing tasks whose state is
too entangled with the source node (those declare `--migratable=none`);
speeding up tasks not bottlenecked by placement; or solving bin-packing
in general (predicate + plan 04 do most of that already).

### Alternatives considered and rejected

- **DMTCP instead of CRIU.** Broader portability; predates CRIU. We
  pick CRIU: NVIDIA's `cuda-checkpoint` integrates with CRIU (not
  DMTCP), CRIU is in-tree in mainstream distros, and DMTCP's
  coordinator adds another moving part.
- **Transparent migration à la MOSIX/openMosix/Kerrighed.** Beautiful
  research; every implementation died. The abstraction (every syscall
  potentially remote) is too invasive to maintain out-of-tree;
  in-tree maintainers correctly refused to merge it. Classic stays
  user-space and opt-in.
- **Re-spawn from an application-level checkpoint.** What most ML
  training already does (PyTorch / DeepSpeed). Often the *right*
  answer; we do not preclude it. System-level migration is for tasks
  where the application can't checkpoint cheaply, or where rebuilding
  CUDA contexts / JIT caches / memory pools is expensive enough to
  justify it.

## Design

### Architecture

A new crate, `classic-migrate`, sits beside `classic-spawn`:

```
classic-node ─► classic-spawn ─► classic-migrate ─► (libcriu, cuda-checkpoint)
                      │                  │
                      └──────────────────┴─► classic-cap (release/reacquire caps)
```

`classic-migrate` does **not** depend on `classic-spawn`. The migration
state machine runs in the executor daemons on both source and
destination — there is no third party. The user-visible task identity
(`NetId`) survives the move; only its `node` component changes.

#### Migration state machine

Source side:

```
Stable
   │  trigger: load-balance, drain, user, predicted failure
   ▼
Freezing       ── userfaultfd-marked dirty pages captured by CRIU.
   │              GPU state locked via cuda-checkpoint lock/checkpoint.
   ▼
Checkpointed   ── on-disk image at $CLASSIC_STATE_DIR/migrate/<req_id>/
   │              plus device-cap manifest.
   ▼
Transferring   ── content-addressed delivery over the existing peer mux.
   ▼
HandedOff      ── destination acked all chunks; source is read-only,
   │              still holds cgroup scope + caps as rollback escrow.
   ▼
(destination Stable) → Cleanup → gone
(destination failure) → Thaw → Stable    (rollback path)
```

Destination side: `Reserving → Receiving → Restoring → Stable`. Any
pre-`Stable` failure sends `MigrateAbort`; source thaws (CRIU unfreeze
or `cuda-checkpoint unlock` with no checkpoint applied) and the task
keeps running. **No state is lost on a failed migration.**

#### Coordination via spawn-frame extensions

The plan-04 spawn range (`0x0300–0x03FF`) is too small to host both
spawn and migrate. We allocate `0x0700–0x07FF` to `classic-migrate`
(currently in the reserved bucket; ARCHITECTURE.md frame table will be
updated). Frame kinds: `MigrateOffer`, `MigrateAccept`, `MigrateAbort`,
`MigratePage`, `MigrateGpuChunk`, `MigrateFinalize`,
`MigrateRollback`. Point-to-point source ↔ destination over the
peer mux from plan 01.

### Triggers

Three triggers, all routed through one `MigrationPolicy` trait so they
don't fight:

1. **Load-balance.** A scheduling tick (default 60 s) picks the most
   loaded node by a configurable cost function (GPU utilization,
   memory pressure, queue depth) and proposes moving its
   cheapest-to-migrate task to its least-loaded predicate-matching
   peer. Hysteresis threshold prevents thrash.
2. **Pre-failure.** NVML XID errors above a configurable severity, or
   ECC double-bit errors, mark a node `draining`. Tasks evacuate in
   priority order (longest-running first). Operator
   `classicd-ctl drain` shares this path.
3. **Explicit.** `classic migrate <task-id> <node-id>` issues a
   targeted move — useful for test, debug, and operator workflows.

### Opt-in declaration

Migration is opt-in per task. Plan 04's `SpawnRequest` gains a field
(default `None`, so v1 frames decode unchanged):

```rust
pub struct SpawnRequest {
    // ... existing plan-04 fields ...
    pub migratable: Migratable,    // NEW; default = Migratable::None
}
pub enum Migratable {
    None,    // never migrate; trigger refuses with TaskNotMigratable
    Full,    // CPU + memory + open files + sockets via CRIU; no GPU
    Gpu,     // Full + CUDA contexts via cuda-checkpoint
}
```

CLI: `classic spawn --migratable=full|gpu|none ...` (default `none`).

### File-handle reattachment

- **Shared-FS path.** If the file is on a path declared in the
  cluster-wide shared-FS list (NFS / CephFS / Lustre / `/shared`),
  CRIU records path + offset and reopens on restore. Common case
  for ML.
- **Content-addressed delivery.** For paths *not* on a shared FS,
  hash content (capped at a per-task budget, default 1 GiB), ship
  blobs the destination doesn't have, materialize under per-task
  scratch, rewrite the path in the CRIU image. Handles staging
  files but isn't free.

Pipes within the task tree are checkpointed by CRIU directly. Pipes
outside the task refuse the move.

### Socket reattachment

- **TCP repair + IP follow.** If the cluster has an IP-migration
  control plane (BGP, ARP-overload, virtual IPs), `TCP_REPAIR` dumps
  socket state on the source and re-establishes on the destination
  after the IP migrates. Zero observable disconnect. Opt-in; we do
  not assume Classic controls the network.
- **Reconnect-window default.** Sockets close at checkpoint and the
  application reconnects after restore. The "ML training over NCCL"
  pattern: the framework already retries collectives. We document
  the pattern; we do not transparently fake it.

Mailbox connections (plan 05) are special: `NetId` is preserved
across migration. The mailbox layer learns it has moved via a gossip
update and re-routes; hand-off is folded into the migration protocol
so the source releases the mailbox atomically with destination
`Stable`.

### GPU state handoff

CUDA state moves via `cuda-checkpoint`'s four-phase flow, wrapped by
`classic-migrate`: **Lock** (streams pause, in-flight kernels drain),
**Checkpoint** (device memory + driver state to a host-side image),
**Transport** (content-addressed transfer; tens of GiB on H100 jobs),
**Restore** (driver rebuilds context against destination GPU), **Unlock**
(streams resume; the application returns from its CUDA call as if it had
been merely slow).

CUDA driver constraints enforced at migration time: source and destination
NVIDIA driver versions must be compatible per vendor matrix; GPU
architecture family must match (H100 → H100 OK; H100 → A100 NOT OK; H100
→ H200 per vendor matrix), enforced by the `place()` predicate at
migration time; multi-GPU tasks migrate atomically — all GPUs or none, no
splitting across nodes; NCCL connections fall under the reconnect-window
default.

### Crate / file layout

```
crates/
  classic-migrate/                NEW
    src/{lib,state,criu,cudacheckpoint,transport,files,sockets,policy}.rs
  classic-proto/src/frames/migrate.rs   NEW   # 0x0700–0x07FF frames
  classic-proto/src/lib.rs              MOD   # register migrate frames
  classic-spawn/src/lib.rs              MOD   # SpawnRequest.migratable
  classic-cli/src/cmd/migrate.rs        NEW   # `classic migrate`
  classic-cli/src/cmd/spawn.rs          MOD   # --migratable flag
  classic-node/src/lib.rs               MOD   # wire handlers + drain
plans/ARCHITECTURE.md                   MOD   # frame table 0x0700
```

## Requirements

### Functional

- [ ] FR-1: A `--migratable=full` non-GPU task migrates between two
      nodes; observed downtime < 1 s.
- [ ] FR-2: A `--migratable=gpu` CUDA task migrates between two H100
      nodes; observed downtime < 10 s for a typical training step.
- [ ] FR-3: `--migratable=none` refuses migration with a stable error.
- [ ] FR-4: Destination failure rolls back on source with no observable
      change to task behavior.
- [ ] FR-5: Operator drain evacuates all migratable tasks; the
      operator gets a list of non-migratable holdouts.
- [ ] FR-6: Load-balance trigger respects hysteresis; no task migrates
      twice within 5 minutes under steady state.
- [ ] FR-7: `classic migrate <task-id> <node-id>` issues one targeted
      move and reports outcome.
- [ ] FR-8: `NetId` is unchanged across migration; peers continue to
      reach the task within one gossip cycle.
- [ ] FR-9: Device caps move atomically — no window where two nodes
      both believe they own the same exclusive cap.

### Non-functional

- **Performance:** non-GPU downtime < 1 s for ≤ 8 GiB working sets on
  10 GbE; GPU downtime < 10 s for typical fine-tuning step (≤ 80 GiB
  GPU memory) on 100 GbE; load-balance steady-state overhead ≤ 5%.
- **Reliability:** every error edge in the state machine has a
  rollback path.
- **Compatibility:** Linux ≥ 6.7 (mainline `cuda-checkpoint`); CRIU
  ≥ 4.0; NVIDIA driver per vendor matrix (≥ 565 for H100). x86_64.
- **Security:** trusted cluster (same as v1); no new auth surface.
  Page contents traverse the existing peer mux.
- **Hardware:** GPU migration tested on paired H100 hosts.

## Testing plan

### Unit

- `classic-migrate::state`: state-machine transitions exhaustively;
  every error edge produces rollback.
- `classic-migrate::transport`: frame round-trip; chunk reassembly
  under packet loss; large-image streaming under bounded memory.
- `classic-migrate::files`: shared-FS detection on a fixture mount
  table; content-addressed dedup against goldenfile.
- `classic-proto::frames::migrate`: round-trip every new frame.
- `classic-spawn`: `migratable` defaults to `None`; old-format frames
  decode with the default; CLI flag parses.

### Integration

- Two `classicd` instances, no GPU: migrate a sleep loop with
  in-process pipes between two children; assert process tree survives,
  pipes still flow.
- Simulated CRIU failure on destination: assert source-side rollback.
- Load-balance policy under synthetic load matrix: assert hysteresis
  prevents oscillation across 1000 ticks.
- Drain handler: 10 migratable + 1 non-migratable task; drain blocks
  until operator confirms; 10 migrate, 1 reported.

### End-to-end (real hardware)

Two H100 nodes, 100 GbE, Linux ≥ 6.7, CRIU ≥ 4.0:

```bash
classic spawn --requires "gpu.model == 'H100'" --migratable=gpu \
              -- python train.py --steps 10000
TASK=$(classic ls --running | head -1 | awk '{print $1}')
classic migrate $TASK $OTHER_NODE
# Expect: task continues; nvidia-smi on $OTHER_NODE shows the process;
# train.py loss curve is continuous.

classicd-ctl drain $LOCAL_NODE
# Expect: all migratable tasks evacuate within drain timeout.
```

### Hardware-dependent

Gated `#[cfg(feature = "hw-gpu-pair")]` — requires two GPU nodes; not
in default CI.

- Numerical equivalence: deterministic CUDA workload, hash an output
  tensor before and after migration; must match.
- Multi-GPU atomicity: 2-GPU task migrates both or rolls both back.
- Driver mismatch within vendor matrix: succeed cleanly or refuse
  cleanly — never silent corruption.

Some tests need root (CRIU, cgroups, BPF). Mark `#[ignore]` and gate
behind `RUST_TEST_ROOT=1`.

## Acceptance criteria

Aspirational and roadmap-level — exact thresholds tighten when the v2
epic is drafted.

- [ ] AC-1: A long-running CUDA training job survives migration from
      one H100 host to another with < 30 s observable downtime and
      identical numerical output (hash-verified).
- [ ] AC-2: A non-GPU task with 8 GiB working set migrates with < 1 s
      observable downtime over 10 GbE.
- [ ] AC-3: Operator drain evacuates all migratable tasks; non-
      migratable tasks are listed with a kill-or-cancel choice.
- [ ] AC-4: Fault-injected destination failure rolls back on source
      with no observable behavior change.
- [ ] AC-5: A task's `NetId` is unchanged after migration; mailbox
      messages addressed to it continue to be delivered (plan 05).
- [ ] AC-6: Load-balance policy run for one hour on a 4-node cluster
      with synthetic mixed load reduces per-node GPU utilization
      variance without any task migrating twice.
- [ ] AC-7: All non-GPU tests pass on Linux 6.7+ x86_64 in CI; GPU
      tests pass on the paired-H100 lane.

## Open questions

- **Placement-group atomicity.** Plan 07 adds `PACK` / `SPREAD`. Should
  migration of one task in a `PACK` group pull the whole group atomically?
  Probably yes for `PACK`, no for `SPREAD` — but atomic group migration
  is implementation-expensive.
- **DMA-mapped non-NVIDIA accelerators.** RDMA NICs, DPUs, FPGAs.
  v1 caps cover them as opaque PCI slots; we cannot checkpoint state
  inside them. Refuse migration when such a cap is held, or rely on
  the application's reconnect logic?
- **Load-balance heuristic.** Cost function: GPU utilization weighted
  by memory? Queue depth? Power draw? Need at least one experimental
  policy and a way to swap without code change.
- **Hysteresis bound.** Per-task, per-cluster, or per-policy minimum
  interval? Likely per-policy, configurable; default 5 min.
- **Per-task migration budget.** A pathological policy could migrate a
  task continuously. Daemon-enforced budget (e.g. ≤ 10 migrations per
  task lifetime, ≤ 1% of wall time migrating).
- **Frame range.** Provisional `0x0700–0x07FF`. ARCHITECTURE.md reserves
  `0x0600–0x06FF` for auth/telemetry — don't poach. Confirm the next
  free range when this lands.
- **Rollback cost on a partially-transferred GPU image.** Discard and
  resume on source, or hold for a retry to a different destination?
  Simple answer "discard"; better may be policy-driven.
- **Interaction with the 9P namespace (plan 06).** A migrated task's
  per-process namespace must be reattached on the destination. How
  does the namespace follow?
- **Checkpoint storage location.** `$CLASSIC_STATE_DIR/migrate/...`
  works for small images; very large GPU images (≥ 80 GiB) on tmpfs
  may not fit. Streaming without spooling is faster but harder to
  make robust.
- **Migration latency floor.** Practical minimum downtime on target
  hardware — known after the first prototype.

## References

- **CRIU.** <https://criu.org/Main_Page>; CRIU 4.0 release notes
  (Sept 2025) for the GPU-support milestone.
- **CRIUgpu / cuda-checkpoint.** NVIDIA `cuda-checkpoint`:
  <https://github.com/NVIDIA/cuda-checkpoint>; CUDA driver release
  notes for the version that mainlined it.
- **Amoeba migration paper.** Douglis, F. & Ousterhout, J., "Transparent
  Process Migration: Design Alternatives and the Sprite Implementation,"
  *Software—Practice and Experience*, 21(8), 757–785 (Aug 1991). Source
  of the "placement-at-fork captures most of the value" finding cited
  above.
- **MOSIX / openMosix lessons.** LWN, "openMosix shutdown" (2008):
  <https://lwn.net/Articles/281454/>; the surrounding discussion of
  why transparent kernel-resident migration didn't survive merge
  pressure. Kerrighed: <https://en.wikipedia.org/wiki/Kerrighed>.
- **DMTCP.** <http://dmtcp.sourceforge.net/>; the alternative
  coordinated-checkpointer design we did not pick.
- **TCP repair.** Linux `Documentation/networking/tcp.rst`; original
  `TCP_REPAIR` patches by Pavel Emelyanov.
- `plans/ARCHITECTURE.md` — identity types, frame ranges, kernel floor,
  trust model. Frame-range allocation here proposes an extension.
- `plans/04-spawn-pipeline.md` — the v1 spawn pipeline this feature
  extends; `SpawnRequest`, `CapBroker`, cgroup-scope semantics are
  defined there and not redefined here.
- `plans/05-mailbox-service-directory.md` — mailbox layer; mailbox
  hand-off is folded into this design.
- `plans/06-9p-namespace-server.md` — namespace handling (open question).
- `plans/07-placement-groups.md` — placement-group atomicity (open
  question).
