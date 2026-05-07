# Feature: Multi-tenancy, Resource Quotas, and Fair-share Scheduling

> **Status:** future
> **Owner:** unassigned
> **Last updated:** 2026-05-07
> **Depends on:** F4 (node security / authenticated identity). Multi-tenancy
> without authenticated identity is theatre — anyone running `classicd` or
> forging a frame can claim to be any tenant. F5 assumes F4's cap and identity
> model is in place. Pieces of F5 that do not need authenticated tenant
> identity (e.g. the local cgroup hierarchy shape) can land first as
> scaffolding; enforcement waits for F4.

## Scope

**In scope.**

- **Tenant identities** — `TenantId` representing user/team/org, issued
  and signed by the F4 cluster operator key. Each tenant has a public key,
  display name, and quota record.
- **Per-tenant resource quotas** — hard caps on aggregate CPU shares,
  memory bytes, GPU-minutes (rolling 24-h window), running task count, and
  placement-group reservation count.
- **Fair-share scheduling** — extension to plan 03's `Rank` so the ranker
  biases placement away from over-share tenants, with decay-based recovery
  so yesterday's burn doesn't permanently penalise today.
- **Usage accounting + reporting** — per-tenant counters per node, gossiped
  and aggregated; `classic accounting --tenant <T>` reports CPU-seconds,
  GB-hours, GPU-seconds, spawn count.
- **Tenant-scoped namespaces** — plan 06's `/proc/` and `/svc/` show only
  tasks/services owned by the requesting tenant unless an operator cap
  broadens the view. `/dev/gpu` and `/dev/pci` stay cluster-visible —
  quotas (and the cgroup BPF allowlist from plan 04), not visibility,
  gate hardware.
- **Per-tenant priority classes** — every operator-issued cap names a
  priority: `low | normal | high | critical`. A tenant may hold multiple
  caps at different priorities concurrently.
- **Preemption** — a `high` or `critical` spawn that cannot place fresh
  may preempt eligible lower-priority tasks of *other* tenants. SIGTERM
  with configurable grace (default 30 s), then SIGKILL. Observable to the
  user via `ChildExit.preempted_by`.
- **Quota authoring** — operator-signed TOML describing tenants, quotas,
  share weights, and priority classes; gossiped as a signed update.

**Out of scope.** Billing/invoicing (F5 emits usage records; an external
system bills). Chargeback policy beyond "report it" (e.g. refunds for
preempted work). Cross-cluster federation (tenant identity is per-cluster).
Anti-affinity by tenant ("never co-locate A and B"). Resource reclamation
beyond preemption — no CRIU, no checkpoint/restore (ARCHITECTURE.md rules
this out cluster-wide). Per-task UID mapping (plan 04 punts; F5 inherits
whatever F4 settles).

## Reasoning

The v1 trust assumption — *"fully trusted cluster, no multi-tenancy
enforcement"* (ARCHITECTURE.md) — works for a single operator. It breaks the
moment two teams share a cluster: a runaway training job grabs every GPU;
tenant B can `classic ps` and see tenant A's argv and env; "who used the
H100 last week?" has no answer; misbehaving tenants can be killed
individually but not as a group, because "as a group" requires knowing who
owns what.

Multi-tenancy is invasive — it touches placement (plan 03), spawn
(plan 04), namespace (plan 06), and gossip (plan 02). Bundling it into v1
would slow the core SSI proof-of-concept. Punting to F5 keeps v1 small.

**Alternatives.** *SLURM-style account hierarchy* (org → team → user,
recursive QoS): powerful, but recursive checks and merge semantics are real
cost. Partially adopted: flat tenants here, hierarchy as an open question.
*Kubernetes namespaces + ResourceQuota + LimitRange*: simpler; K8s conflates
visibility/quota/priority into one axis, we keep them separate. *Borg
priority + quota*: production/batch/best-effort with preemption between
bands matches our use case; adopted in shape, but Borg's quota is
best-effort soft and we make it admission-rejecting because operators
expect predictable rejection. *Mesos role-based reservations*: static,
doesn't fit gossip-based ads. *Do nothing — defer to SLURM/K8s*: concedes
the integrated-SSI value proposition.

**Success in plain English.** Two teams share a 16-GPU cluster at 50/50
share. Team A floods Friday with low-priority training; Saturday morning
team B's normal-priority eval lands as expected and uses 8 GPUs because
the ranker biased away from A while A's burned share decays. Operators run
`classic accounting --since "2026-05-01"` and hand a per-tenant table to
finance.

## Design

### Architecture

F5 extends existing crates plus one new crate (`classic-acct`) for
accounting. Same acyclic crate dependency invariant.

```
classic-cap   ── tenant cap verification + cgroup tenant-slice setup
classic-ad    ── per-tenant usage subset (gossiped, F4-signed)
classic-place ── tenant_share() built-in for Rank; admission quota check
classic-spawn ── tenant cap on SpawnRequest; preemption orchestrator
classic-fs    ── tenant-scoped /proc and /svc views
classic-acct  ── NEW: per-node counters, gossip merge, query API
classic-cli   ── classic tenant ..., classic accounting ...
```

Spawn-time sequence with quota check + fair-share rank:

```
CLI                originator                    executor
 │ SpawnRequest    │ verify cap signature (F4)    │
 │   {tenant_cap}  │ extract TenantId T, prio P   │
 ├────────────────►│ usage = acct.snapshot(T)     │
 │                 │ if over_quota: → QuotaExceeded
 │                 │ rank = base + share_term(T)  │
 │                 │ candidates = place(req,rank) │
 │                 ├─ pick head; SpawnRequest ───►│ verify cap (F4)
 │                 │                              │ acquire caps in
 │                 │                              │   classicd.slice/T.slice
 │                 │                              │ acct.record_spawn
 │                 │◄─────── SpawnAck ────────────│ fork+exec (plan 04)
 │◄──── SpawnAck ──│                              │
```

### Data shapes

```rust
// classic-proto (extension)
pub struct TenantId(pub [u8; 16]);
pub enum PriorityClass { Low, Normal, High, Critical }

/// Operator-signed cap presented on every SpawnRequest. Verified against
/// the cluster operator key distributed via F4.
pub struct TenantCap {
    pub tenant: TenantId, pub priority: PriorityClass,
    pub not_before: u64, pub not_after: u64,
    pub nonce: [u8; 16], pub signature: [u8; 64],   // ed25519, F4-key-signed
}

// classic-ad (extension) — gossiped as a separate signed update.
pub struct TenantQuota {
    pub tenant: TenantId, pub display_name: String,
    pub share_weight: u32,            // share = weight / sum_all_weights
    pub max_running_tasks: u32, pub max_memory_bytes: u64,
    pub max_gpu_minutes_24h: u64,     // rolling 24-h budget
    pub max_placement_groups: u16,
    pub allowed_priorities: Vec<PriorityClass>,
    pub recovery_half_life_s: u64,    // default 86400
}
pub struct QuotaUpdate {
    pub epoch: u64,                   // monotonic; later wins
    pub tenants: Vec<TenantQuota>,
    pub operator_signature: [u8; 64],
}

// classic-acct
pub struct TenantUsage {
    pub tenant: TenantId, pub running_tasks: u32,
    pub memory_bytes_resident: u64,
    pub cpu_seconds_consumed: u64, pub gpu_seconds_consumed: u64,
    pub spawn_count_since_epoch: u64, pub last_updated_unix: u64,
}
pub struct NodeTenantUsage {
    pub node: NodeId, pub generation: u64,
    pub per_tenant: Vec<TenantUsage>,
    pub signature: [u8; 64],          // F4 node key
}
```

### Cgroup hierarchy

```
/sys/fs/cgroup/classicd.slice/<tenant-T>.slice/
  cpu.weight    # = T.share_weight
  cpu.max       # hard ceiling
  memory.max    # = T.max_memory_bytes
  task-<MboxId>.scope/   # plan 04 per-task scope, parented here
```

Standard cgroup v2 weight + max combo. `cpu.weight` does proportional
sharing under contention; `cpu.max` is the hard ceiling. Per-task scopes
inherit and may set tighter limits but cannot exceed the parent slice.

### Quotas as predicates over usage state

Plan 03's `place()` operates over `&[NodeAd]`. F5 extends the evaluation
context with a read-only handle to the local usage cache:

```rust
// classic-place (extension)
pub trait UsageView {
    fn tenant_usage(&self, t: TenantId) -> TenantUsage;
    fn tenant_quota(&self, t: TenantId) -> Option<TenantQuota>;
    fn cluster_total_share_weight(&self) -> u64;
}

pub fn place_with_tenant(
    req: &Requirement, rank: &Rank, ads: &[NodeAd],
    usage: &dyn UsageView, tenant: TenantId,
) -> Vec<(NodeId, f64)>;
```

New built-ins inside `Rank`: `tenant_share` (float in `[-1.0, +1.0]`,
negative when over share, positive when under, zero at exact share),
`tenant_running` (current task count), `tenant_gpu_seconds_24h`. Default
rank under F5:

```
-load.cpu_pct
  - 1000.0 * (count(gpu) - count(gpu, gpu.in_use))
  + 500.0  * tenant_share
```

The `500.0` is configurable (`fair_share_weight`). Zero disables
fair-share and falls back to the plan 03 default.

**Admission check, before placement:**

```text
fn admit(T, req) -> Result<(), QuotaError>:
    q = quota.lookup(T)?                  # unknown tenant → reject
    u = usage.snapshot_cluster(T)
    if u.running_tasks >= q.max_running_tasks:                   Err(MaxTasks)
    if u.memory_bytes_resident + req.est_mem >= q.max_memory_bytes: Err(MaxMemory)
    if u.gpu_seconds_24h() + req.est_gpu_s >= q.max_gpu_minutes_24h*60: Err(MaxGpuBudget)
    if priority not in q.allowed_priorities:                     Err(PriorityNotAllowed)
    Ok(())
```

Estimates come from `SpawnRequest`'s new `ResourceHints { mem_bytes,
gpu_seconds }`. A missing hint is zero at admission but charges actual
usage post-fact; an alert fires if hints under-state by >2x.

**Fair-share decay.** Exponential, 24-h half-life by default (HTCondor
`PRIORITY_HALFLIFE`). A tenant who consumed 100% of cluster GPU yesterday
recovers half their share by tomorrow, ~75% by the day after —
deliberately slow because fast recovery causes oscillation.

### Tenant-scoped 9P namespaces

Plan 06's namespace assembly gains a tenant filter installed at attach time
(the `classicd` resolves the requesting tenant from F4 attach credentials):
`/proc/` shows only `MboxId`s whose owning tenant equals the requester;
`/svc/` shows only services registered by tasks of the requesting tenant;
`/dev/gpu` and `/dev/pci` are unchanged because the cgroup BPF allowlist
(plan 04 classic-cap) is the actual gatekeeper. An operator cap with an
`audit` flag bypasses the filter for debugging.

### Preemption

When admission accepts a `high`/`critical` spawn but `place()` returns
empty, F5 enters preemption:

```text
fn try_preempt(req, T_r, P_r):
    candidates = ads filtered by predicate (ignoring "device free")
    for ad in candidates by rank desc:
        victims = running tasks on ad where
            v.tenant != T_r && v.priority < P_r && v.caps cover req
        if victims free up enough caps: return Preempt{ad, victims}
    None
```

Victim selection: lowest priority first; within a priority,
longest-running first (LRU; protects newly started tasks). Other policies
considered in Open Questions.

Wire-protocol extensions in plan 04's spawn band:
- `0x0306 PreemptNotice` — coord → executor — SIGTERM these victims; new
  caps will be needed in `grace_seconds`.
- `0x0307 PreemptComplete` — executor → coord — victims gone (SIGKILL'd
  after grace if needed); caps released; new spawn may proceed.

The new spawn then proceeds as a normal plan 04 `SpawnRequest`.

**User-visible contract: preemption is observable.** A preempted task's
`ChildExit` carries `signal = SIGTERM` (then SIGKILL after grace) plus a
new `preempted_by: Option<(TenantId, PriorityClass)>` field, so users can
distinguish preemption from OOM or operator action. Tasks that want to
resume should checkpoint on SIGTERM — the runtime does not checkpoint
automatically. `critical` is never preempted by `low|normal|high` of any
tenant; only another `critical` can preempt it, and only with explicit
operator override.

### Accounting

Each `classicd` keeps per-tenant counters updated continuously: increment
on spawn, decrement on exit, sample memory once per second from cgroup
`memory.current`, derive CPU/GPU seconds from `cpu.stat` and NVML.
Counters gossip as `NodeTenantUsage` once per heartbeat; cluster
aggregation is sum-over-nodes.

```bash
classic accounting --tenant alpha --since 2026-05-01 --until 2026-05-07
# tenant: alpha    period: 2026-05-01..2026-05-07
# spawns: 4892   cpu-seconds: 142381 (39.6 core-h)
# gpu-seconds: 91200 (25.3 gpu-h)   memory GB-h: 382
# peak running: 47 (2026-05-04 14:22)
classic accounting --all --format=csv > usage.csv
```

### Quota authoring

Operator-signed TOML, gossiped on push:

```toml
epoch = 17

[[tenant]]
id = "a3f2c0d1-..."
display_name = "alpha"
share_weight = 60
max_running_tasks = 200
max_memory_bytes = 512_000_000_000
max_gpu_minutes_24h = 14_400
max_placement_groups = 16
allowed_priorities = ["low", "normal", "high"]
recovery_half_life_s = 86400
```

`classic operator quota apply quota.toml --sign-with /etc/classic/op.key`
parses, signs with the F4 operator key, and gossips a `QuotaUpdate` frame.
Updates are atomic — every node accepts the new epoch or none do (reject
if local epoch ≥ incoming).

### File / crate layout

```
crates/
  classic-acct/                       # NEW: counters, gossip merge, decay
  classic-proto/src/tenant.rs         # NEW: TenantId, TenantCap, PriorityClass
  classic-proto/src/frame.rs          # MOD: register 0x0306, 0x0307, 0x0608
  classic-cap/src/tenant_slice.rs     # NEW: classicd.slice/<T>.slice setup
  classic-cap/src/cap_verify.rs       # NEW: TenantCap signature verify (F4)
  classic-place/src/tenant_share.rs   # NEW: UsageView, tenant_share built-in
  classic-place/src/eval.rs           # MOD: context-bound built-ins
  classic-spawn/src/admission.rs      # NEW: over-quota check
  classic-spawn/src/preempt.rs        # NEW: victim selection + protocol
  classic-spawn/src/executor.rs       # MOD: record_spawn / record_exit hooks
  classic-fs/src/tenant_view.rs       # NEW: /proc + /svc filter
  classic-cli/src/{tenant,accounting,operator_quota}.rs  # NEW
```

## Requirements

### Functional

- [ ] FR-1: A `SpawnRequest` without a valid `TenantCap` is rejected with
      `SpawnDeny{Unauthorized}`.
- [ ] FR-2: A spawn over `max_running_tasks`, `max_memory_bytes`, or
      `max_gpu_minutes_24h` is rejected at admission with
      `SpawnDeny{QuotaExceeded(reason)}` naming the violated quota.
- [ ] FR-3: With two contending tenants, the ranker biases toward the
      under-share tenant; steady-state share over a 1-h window matches
      `share_weight` ratios within ±5%.
- [ ] FR-4: A `high`-priority spawn that cannot place fresh preempts at
      least one eligible lower-priority task of another tenant and lands
      within `2*grace + 5 s`.
- [ ] FR-5: Preempted tasks receive SIGTERM, then SIGKILL after grace
      (default 30 s); `ChildExit.preempted_by` is populated.
- [ ] FR-6: `critical` tasks are not preempted by `low|normal|high` of
      any tenant.
- [ ] FR-7: `/proc/` and `/svc/` attached by tenant T show only T's
      tasks/services; an operator `audit` cap shows everything.
- [ ] FR-8: Accounting accurate: `cpu_seconds` matches `cpu.stat` ±1%;
      `gpu_seconds` matches NVML deltas ±1%.
- [ ] FR-9: `QuotaUpdate` with epoch ≤ local or bad signature is rejected.
- [ ] FR-10: `classicd.slice/<T>.slice/` exists when T has running tasks;
      removed when the last task exits.
- [ ] FR-11: `classic accounting` emits a documented schema (CSV/JSON).

### Non-functional

- **Performance:** quota check at admission ≤ 5 ms; fair-share rank
  evaluation adds ≤ 2 ms per ranked node.
- **Accuracy:** accounting counters within ±1% of ground truth over a 1-h
  run.
- **Compatibility:** F5 requires F4. Same Linux runtime as v1.
- **Security:** quota records and tenant caps are operator-signed; nothing
  honored without verifiable F4 signature. Tenants cannot forge caps.
- **Binary compatibility:** no changes to user binaries; v1 spawns without
  a cap are auto-issued a "default tenant" identity (deprecated) for one
  release cycle.

## Testing plan

### Unit

- `classic-acct`: counter increment/decrement under simulated spawns;
  decay math vs. scipy reference; `NodeTenantUsage` merge (idempotent,
  latest-wins by generation).
- `classic-place::tenant_share`: rank with synthetic `UsageView` —
  balanced (zero), over-share (negative), under-share (positive), empty
  cluster (no panic).
- `classic-spawn::admission`: each quota dimension hit independently
  yields the right reason; multiple violations reported in priority order.
- `classic-spawn::preempt`: victim selection — no eligible, single,
  five-eligible (lowest-priority-then-LRU).
- `classic-cap::{tenant_slice, cap_verify}`: cgroup creation with right
  `cpu.weight`/`memory.max`, cleanup on last exit; valid sig accepted,
  tampered/expired/unknown-key rejected.

### Integration

- **Two-tenant fair-share, 1 GPU.** In-process two-daemon harness;
  tenants A (weight 70) and B (weight 30) flood spawns for 10 simulated
  minutes. Observed GPU-second ratio = 70/30 ±5%.
- **Quota rejection.** Tenant A `max_running_tasks=2`; third spawn
  returns `QuotaExceeded(MaxTasks)`.
- **Preemption smoke.** A (low) fills cluster; B (high) submits. One of
  A's tasks preempted within `grace + 5 s`; B lands; preempted's
  `ChildExit.preempted_by` reports B.
- **Critical immunity.** A (critical) running; B (high) submits → denied
  with `NoCandidates` rather than preempting A.
- **Accounting end-to-end.** 60 s synthetic workload across 3 tenants on
  a 3-node in-process cluster. `classic accounting` totals match per-task
  instrumentation ±1%.
- **9P tenant view.** Two tenants × 3 tasks each. A's `/proc/` shows A's
  3 only; operator-cap attach shows all 6.
- **Quota update gossip.** Three-daemon cluster; push epoch 2 to one,
  all three reflect 2 after gossip; push epoch 1, rejected.

### End-to-end (multi-host)

```bash
classic operator key generate /etc/classic/op.key
classic operator quota apply quota.toml --sign-with /etc/classic/op.key
classic operator tenant-cap-issue --tenant alpha --priority normal --validity 24h > alpha.cap
classic operator tenant-cap-issue --tenant beta  --priority normal --validity 24h > beta.cap

# Alpha floods low-pri; beta submits high-pri eval.
CLASSIC_TENANT_CAP=alpha.cap \
  for i in {1..16}; do classic spawn --priority low --requires "any(gpu)" -- ./train.py & done
CLASSIC_TENANT_CAP=beta.cap \
  classic spawn --priority high --requires "any(gpu)" -- ./eval.py
# Expect: lands within ~grace; one alpha task shows preempted_by=beta.
classic accounting --all --format=csv > usage.csv
```

### Hardware-dependent

Gated `#[cfg(feature = "hw-gpu")]`, `CLASSIC_E2E_GPU=1`: real preemption
with NVML (preempted task's GPU allocation fully released to preemptor);
GPU-second accounting matches NVML aggregate within 1% under sustained
CUDA workload.

## Acceptance criteria

- [ ] AC-1: With two tenants competing for GPUs at 50/50 share weights,
      observed GPU-second usage over 1 hour is within ±5% of fair.
      (Fair-share with churn is approximate; ±5% is the realistic bar.)
- [ ] AC-2: A tenant cannot exceed `max_running_tasks` even when colluding
      with multiple operators (multiple `TenantCap`s sum against one
      quota).
- [ ] AC-3: A `QuotaUpdate` not signed by a known operator key is
      rejected by every node; gossip does not propagate it.
- [ ] AC-4: A preempted task receives SIGTERM, has at least
      `grace_seconds` to clean up, then SIGKILL; the preemptor's spawn
      succeeds within `grace + 5 s`.
- [ ] AC-5: A tenant's `/proc/` attach shows only their own tasks; an
      operator `audit` attach sees the full set.
- [ ] AC-6: Accounting totals match instrumented ground truth within ±1%
      over a 1-hour workload.
- [ ] AC-7: `critical` tasks are never preempted by lower priorities
      under any non-operator action.
- [ ] AC-8: A tenant whose 24-h GPU-minute budget is exhausted has
      subsequent spawns rejected at admission with a clear error; budget
      recovers smoothly (decayed) once usage stops.
- [ ] AC-9: All non-hw integration tests pass on Linux 6.1+ x86_64.
- [ ] AC-10: F4 dependency is documented in the F5 implementation epic;
      F5 tasks block on F4 deliverables (operator key distribution,
      cap-signing primitives, signed gossip).

## Open questions

1. **Hierarchical tenants** (SLURM-style org → team → user). Recursive
   checks and merge semantics are real cost. Flat tenancy in v1-future;
   hierarchy as a follow-up pending operator interview data.
2. **Preemption victim policy.** LIFO (protects long jobs);
   lowest-priority then LRU (current default, protects fresh work);
   lowest-progress (workload-defined, not measurable generically). Likely
   a config knob.
3. **Quota carryover across maintenance windows.** If the cluster is
   down 4 h, do GPU-minute budgets advance (lose budget) or pause (carry
   over)? Pause is friendlier; lose-budget is simpler. Defer.
4. **Quota interaction with placement groups (plan 07).** A 16-member
   group exceeding `max_memory_bytes` should fail at admission, not at
   member 14. Plan 07's `place_group()` needs to call F5 admission with
   aggregate estimates — small refactor, track in the F5 epic.
5. **Cluster-wide quota gossip vs. sharded ownership.** F5 v1 gossips
   all quota state to all nodes (~100KB at 1000 tenants); revisit if
   anyone hits 10K-tenant scale.
6. **Estimate hint enforcement.** If a hint under-states by >2x:
   alert-only (current); auto-throttle next spawn; auto-quota-charge as
   penalty. Defer.
7. **"Default tenant" for un-capped legacy spawns.** Auto-issue with a
   deprecation warning; remove after one release cycle.
8. **Wire kind for `QuotaUpdate`.** Tentatively `0x0608` in the
   architecture's `0x0600–0x06FF` "auth/telemetry" band. Decision
   deferred to the F5 epic.

## References

- `plans/ARCHITECTURE.md` — frame ranges, identity types, cgroup-v2
  mandate. F5 reserves frames in the `0x0600–0x06FF` "auth/telemetry" band.
- `plans/03-placement-predicates.md` — `Rank` extended with `tenant_share`
  and related built-ins. Predicate syntax unchanged; evaluator gains a
  context handle (`UsageView`).
- `plans/04-spawn-pipeline.md` — `SpawnRequest` extended with a
  `tenant_cap` field and `ResourceHints`; `SpawnDeny.reason` extended with
  `Unauthorized`, `QuotaExceeded`, `Preempted`. New frames `0x0306` and
  `0x0307` allocate from the spawn band for the preemption protocol.
- `plans/07-placement-groups.md` — group placement under quota pressure
  is non-trivial (Open Question 4); F5 extends `place_group()` admission
  to charge tenant aggregates.
- `plans/06-9p-namespace-server.md` — extended with tenant-scoped views
  in `/proc/` and `/svc/`.
- F4 (future) — node identity, operator key distribution, cap-signing
  primitives. F5 builds on F4 for both tenant caps and quota updates.
  F5 has no meaning without F4.
- SLURM accounts and QoS — https://slurm.schedmd.com/sacctmgr.html,
  https://slurm.schedmd.com/qos.html
- Kubernetes ResourceQuota and LimitRange —
  https://kubernetes.io/docs/concepts/policy/resource-quotas/,
  https://kubernetes.io/docs/concepts/policy/limit-range/
- Borg priority/quota — Verma et al., *Large-scale cluster management at
  Google with Borg* (EuroSys 2015),
  https://research.google/pubs/large-scale-cluster-management-at-google-with-borg/
  — §§2.5–2.6 directly inform the priority hierarchy and admission model.
- HTCondor user priorities —
  https://htcondor.readthedocs.io/en/latest/admin-manual/cm-configuration.html
  — the 24-h half-life default is borrowed from `PRIORITY_HALFLIFE`.
- AWS IAM signed-cap pattern — STS-issued short-lived scoped credentials.
  `TenantCap` is shaped similarly: short-lived (`not_after`), scoped
  (priority class), signed by an authoritative key.
- Mesos role-based reservations —
  http://mesos.apache.org/documentation/latest/reservation/ (rejected
  alternative; see Reasoning).
- Ray fair scheduling —
  https://docs.ray.io/en/latest/ray-core/scheduling/index.html (prior
  art for placement-group-aware fair-share).
