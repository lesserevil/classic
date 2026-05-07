# Feature: Spawn Pipeline

> **Status:** draft
> **Epic bead:** `bd-XXX` (filed after this doc lands)
> **Owner:** TBD
> **Last updated:** 2026-05-07

## Scope

End-to-end: `classic spawn ...` on a user's CLI to a process running on a
remote node, stdio streamed back, exit status propagated. The headline v1
feature — first thing that makes Classic look like a single-system image.
Depends on plan 01 (transport, frame mux, identity types) and plan 03
(`place()` returning a ranked candidate `NodeId` list).

**In scope:** new crates `classic-spawn` (orchestrator state machine on
both sides) and `classic-cap` (`DeviceCap` tokens + `CapBroker` +
cgroup-v2 device-controller enforcement); `classic-cli` `spawn`
subcommand; frames `SpawnRequest`/`SpawnAck`/`SpawnDeny`/`ChildStdio`/
`ChildExit` in `0x0300–0x03FF`; local pipeline (cgroup scope, cap
acquire, fork/exec, stdio, exit); remote pipeline (forward request,
relay stdio, propagate exit); cgroup hierarchy
`/sys/fs/cgroup/classicd.slice/task-<MboxId>.scope` with cpu/memory/
pids/devices (BPF) controllers; NVIDIA minor binding via the BPF device
program; RAII cap release on child exit or originator disconnect;
placement fallback (refusal → try next; exhausted → CLI exits non-zero
with reason).

**Out of scope:**

- **Mailbox abstraction (plan 05).** Spawn uses dedicated frames; it
  allocates the new task's `MboxId` but creating the mailbox is plan 05.
- **9P namespace (plan 06).** Children inherit the executor's mount
  namespace.
- **Placement groups (plan 07).** One task per invocation.
- **CRIU / live migration.** Per ARCHITECTURE.md.
- **Binary distribution.** Assume `argv[0]` exists at the same path on
  every node (shared FS or pre-staged). See Open Questions.
- **Node-to-node auth, multi-tenant quotas, per-task UIDs.** Trusted
  cluster; `classicd` runs as root; children inherit daemon UID.

## Reasoning

**Problem.** Classic's pitch is "declare what hardware you need; the cluster
places your job." Plans 01–03 are plumbing; plan 04 is what users actually
type to make the plumbing observable.

**Why dedicated frames instead of going through the mailbox layer (plan 05)?**
Spawn happens *before* the task has a mailbox, so bootstrapping a mailbox
over the mailbox layer is circular. Spawn also has hard ordering (caps
before fork; cleanup on exit) that reads cleanly as a small state machine.
And plan 05 should be allowed to land *after* plan 04, since spawn is the
headline feature. The duplication cost is minor — these frames are simple.

**Alternatives considered and rejected.**

- *systemd-run / nsjail / OCI runtime as a sidecar.* Splits cgroup
  ownership; we already need raw cgroup-v2 access for device binding, so
  doing it ourselves is simpler than splitting it.
- *Cgroup v1 `devices.allow`.* ARCHITECTURE.md mandates v2 unified
  hierarchy on kernel ≥ 6.1.
- *Cgroup v2 `device.allow` pseudo-files.* These don't exist; the kernel
  deliberately removed the `devices` controller from v2 and replaced it
  with `BPF_CGROUP_DEVICE`.
- *Linux capabilities for `DeviceCap`.* Wrong abstraction — Linux caps
  guard syscalls, not "task X may use GPU 3 exclusively." Our `DeviceCap`
  is daemon-internal accounting that *becomes* cgroup state.
- *Have the CLI talk directly to the executor.* Violates plan 01's
  one-TCP-per-peer-pair invariant (the CLI isn't a peer). Going through
  the local daemon also gives us one place to retry on refusal.

**Success in plain English.** A user types
`classic spawn --requires "gpu.vendor == 'nvidia' && gpu.free_mb >= 40000" -- python train.py`.
The job lands on whichever cluster node has a free 40 GiB+ NVIDIA GPU.
Stdout/stderr stream live to the terminal; stdin can be piped in; the
CLI's exit code matches the script's. If no node matches, the CLI prints
`no node matches predicate: <reason>` and exits 2.

## Design

### Architecture

Three processes on the remote-spawn path: the `classic` CLI (one-shot),
the local `classicd` (originator role), and the chosen peer's `classicd`
(executor role; forks the user task into a per-task cgroup scope).

CLI ↔ local daemon: Unix domain socket
(`/run/classicd/control.sock`, fallback `$XDG_RUNTIME_DIR/classicd.sock`)
carrying the same `Frame` codec from `classic-proto`. Daemon ↔ daemon:
the TCP frame mux from plan 01. If the local node is itself the chosen
executor, the originator and executor roles run in the same daemon — same
state machines, no peer hop.

#### Sequence: remote-spawn happy path

```
CLI            local classicd        remote classicd       child
 |--SpawnRequest---->|                     |                 |
 |                   |--place(ads) -> [N2,N1]                |
 |                   |--SpawnRequest------>|                 |
 |                   |                     |--broker.acquire |
 |                   |                     |--cgroup setup   |
 |                   |                     |--fork+exec----->|
 |                   |<-------SpawnAck-----|                 |
 |<--SpawnAck--------|                     |                 |
 |                   |                     |<-stdout-pipe----|
 |                   |<-ChildStdio(Stdout)-|                 |
 |<-ChildStdio-------|                     |                 |
 |--ChildStdio(In)-->|--ChildStdio(In)---->|--write-stdin--->|
 |                   |                     |<-exit(0)--------|
 |                   |                     |--broker.release |
 |                   |                     |--cgroup teardown|
 |                   |<-ChildExit(code=0)--|                 |
 |<-ChildExit--------|                     |                 |
 (CLI exit 0)
```

#### Sequence: placement-failure fallback

```
CLI       local classicd          remote N2         remote N1
 |--SpawnReq->|                       |                 |
 |            |--place() -> [N2, N1]  |                 |
 |            |--SpawnRequest-------->|                 |
 |            |                       |--broker.acquire |
 |            |                       |   -> Err(Taken) |
 |            |<--SpawnDeny(DevTkn)---|                 |
 |            |   (try next)                            |
 |            |--SpawnRequest---------------------------|
 |            |                                         |--ok
 |            |<--------------------SpawnAck------------|
 |<-SpawnAck--|                                         |
 ...
 (If place() returned [] OR every candidate denied, originator sends
  SpawnDeny{NoCandidates|AllCandidatesRefused} to CLI; CLI exits 2.)
```

Originator state machine per outstanding spawn:

```
Submitted -> Placing -> Trying(idx) -> Running(net_id)
                   \          |
                    `------>  Denied(reason)
```

`Trying(idx)` cycles through the ranked candidate list; `SpawnDeny`
goes to `Trying(idx+1)` or, if exhausted, `Denied(AllCandidatesRefused)`.
`SpawnAck` → `Running`; thereafter stdio / exit frames are relayed
verbatim between the CLI socket and the chosen executor connection.

### Data shapes

Frames live in `crates/classic-proto/src/frames/spawn.rs`. All encoded with
`bincode` v2 (fixed-int, little-endian) per ARCHITECTURE.md.

| Kind     | Frame          | Direction                                             |
|----------|----------------|-------------------------------------------------------|
| `0x0300` | `SpawnRequest` | CLI → originator; originator → executor (forwarded).  |
| `0x0301` | `SpawnAck`     | Executor → originator → CLI.                          |
| `0x0302` | `SpawnDeny`    | Executor → originator (per candidate); originator → CLI (terminal). |
| `0x0303` | `ChildStdio`   | Bidirectional, relayed.                               |
| `0x0304` | `ChildExit`    | Executor → originator → CLI.                          |

`0x0305–0x03FF` reserved (PTY resize, signal injection, ...).

```rust
pub struct SpawnRequest {
    pub req_id: u64,                  // originator-chosen correlation id
    pub requires: String,             // plan-03 predicate; "" forbidden, use "true"
    pub rank: String,                 // "" == default (least-loaded)
    pub argv: Vec<String>,            // argv[0] must exist on executor (see Open Q)
    pub env: Vec<String>,             // KEY=VALUE; CLI's own env NOT forwarded
    pub exclusive_device: bool,
    pub stdin_kind: Option<StdinKind>,// None == /dev/null on stdin
    pub hop: u8,                      // dropped if > 2 (loop guard)
}
pub enum StdinKind { Inherit, File }

pub struct SpawnAck { pub req_id: u64, pub net_id: NetId }

pub struct SpawnDeny {
    pub req_id: u64,
    pub reason: DenyReason,
    pub detail: String,               // human-only; never parsed
}
pub enum DenyReason {
    NoCandidates,           // place() returned []
    AllCandidatesRefused,   // every candidate denied
    PredicateNotSatisfied,  // stale ad on executor
    DeviceTaken,            // exclusive cap already held
    CgroupSetupFailed,      // mkdir / BPF attach failed
    ExecFailed,             // binary missing / ENOEXEC — no retry
    HopExceeded,            // hop > 2; programming error
    Internal,
}

pub struct ChildStdio {
    pub req_id: u64,
    pub stream: StdioStream,          // Stdin | Stdout | Stderr
    pub data: Vec<u8>,                // empty == EOF
}
pub enum StdioStream { Stdin, Stdout, Stderr }

pub struct ChildExit {
    pub req_id: u64,
    pub code: Option<i32>,            // Some(0..=255) on normal exit
    pub signal: Option<i32>,          // Some(signum) on signal death
}
```

Every frame derives `Serialize, Deserialize, Debug, Clone` (and `Copy`
on the simple enums).

#### `DeviceCap` and `CapBroker`

In `crates/classic-cap/src/lib.rs`:

```rust
pub enum DeviceKind {
    /// /dev/nvidia<N>. BPF program allows (c, 195, N) + the shared
    /// (195, NVIDIA_CTL_MINOR=255) and (195, 254) control nodes.
    GpuMinor(u32),
    /// Non-NVIDIA passthrough (RDMA NIC, DPU, accelerator). Resolved to
    /// char-device (major,minor) via /sys/bus/pci/devices/<bdf>/uevent.
    PciSlot(BdfAddr),
}
pub struct BdfAddr { pub domain: u16, pub bus: u8, pub device: u8, pub function: u8 }

pub struct DeviceCap {
    pub kind: DeviceKind,
    pub exclusive: bool,
    pub holder: MboxId,
    _release: BrokerHandle,           // Drop calls broker.release_internal
}

pub struct CapBroker { /* Mutex<CapBrokerInner> */ }

impl CapBroker {
    pub fn new() -> Self;

    /// Atomically acquire all `kinds` for `holder` — all-or-nothing.
    /// `exclusive=true` refuses if any kind is held (shared or exclusive).
    /// `exclusive=false` co-holds with other shared holders only.
    pub fn acquire(&self, holder: MboxId, kinds: &[DeviceKind], exclusive: bool)
        -> Result<Vec<DeviceCap>, AcquireError>;

    /// Force-release every cap held by `holder`. Called on child exit or
    /// originator disconnect.
    pub fn release_all(&self, holder: MboxId);

    pub fn snapshot(&self) -> Vec<CapSnapshot>;  // diagnostics
}

pub enum AcquireError {
    Taken { kind: DeviceKind },
    SharedConflict { kind: DeviceKind, n: usize },
}
```

`DeviceCap` has no public constructor other than `CapBroker::acquire` —
caps cannot be forged. Drop on a single cap releases just that cap;
`release_all` is the executor's "task ended, clean up" call.

#### Cgroup setup

Cgroup v2 unified hierarchy (kernel ≥ 6.1). Per-task scope path:
`/sys/fs/cgroup/classicd.slice/task-<MboxId>.scope/`.

Sysfs writes by the executor daemon:

```bash
# One-time, at daemon startup:
mkdir -p /sys/fs/cgroup/classicd.slice
echo "+cpu +memory +pids" > /sys/fs/cgroup/cgroup.subtree_control
echo "+cpu +memory +pids" > /sys/fs/cgroup/classicd.slice/cgroup.subtree_control

# Per task, before fork:
SCOPE=/sys/fs/cgroup/classicd.slice/task-${MBOX}.scope
mkdir "$SCOPE"
echo "max"  > "$SCOPE/memory.max"      # v1 placeholder; quota work is post-v1
echo "max"  > "$SCOPE/cpu.max"
echo 1024   > "$SCOPE/pids.max"
echo $HELPER_PID > "$SCOPE/cgroup.procs"
```

CPU/memory limits are `max` in v1 — predicate-driven quotas are a follow-up;
the hierarchy is in place so they can be added without re-architecting.

**Device controller.** Not a sysfs file in v2 — it's a `BPF_CGROUP_DEVICE`
program attached to the scope's cgroup fd:

1. Compile `classic-cap/src/bpf/devices.bpf.c` at crate build time via
   `libbpf-cargo`. Body is a map-driven allowlist: default-deny; allow
   `(c, 1, *)` (the standard `/dev/null|zero|random|urandom|tty` set);
   allow `(c, 195, 254)` and `(c, 195, NVIDIA_CTL_MINOR=255)` iff any
   `GpuMinor` cap is held; allow `(c, 195, N)` per `GpuMinor(N)`; allow
   PCI device char nodes per `PciSlot` (resolved via
   `/sys/bus/pci/devices/<bdf>/uevent`).
2. Attach with `bpf(BPF_PROG_ATTACH, BPF_CGROUP_DEVICE)` to the scope fd.
3. Update its `BPF_MAP_TYPE_HASH` allowlist on every cap acquire/release.

BPF is the only option (no `devices` controller in v2 unified — and
ARCHITECTURE.md mandates v2). We do *not* borrow systemd/runc
`device_filter` machinery: `classicd` should own the hierarchy outright,
and OCI runtime libs are a heavy dependency for a small feature.

#### File / crate layout

```
crates/
  classic-proto/src/frames/spawn.rs   # NEW — frame structs + FrameKind variants
  classic-proto/src/lib.rs            # MOD  — register 0x0300..=0x0304
  classic-cap/                        # NEW crate
    Cargo.toml, build.rs              # build.rs invokes libbpf-cargo
    src/lib.rs                        # public API (DeviceCap, CapBroker)
    src/broker.rs                     # internal accounting
    src/cgroup.rs                     # sysfs + cgroup_fd helpers
    src/bpf/devices.bpf.c             # cgroup_device BPF program
    src/bpf/loader.rs                 # loads + attaches the program
    src/nvidia.rs                     # minor enumeration via /dev/nvidia*
  classic-spawn/                      # NEW crate
    src/lib.rs                        # originate() / execute() entry points
    src/originator.rs                 # CLI-side state machine
    src/executor.rs                   # executor-side state machine
    src/stdio.rs                      # pipe<->frame relay
    src/cgroup_scope.rs               # scope dir setup/teardown
  classic-cli/src/bin/classic.rs      # MOD  — wire `spawn` subcommand
  classic-cli/src/cmd/spawn.rs        # NEW  — CLI surface, talks to local daemon
  classic-node/src/lib.rs             # MOD  — register spawn handlers on
                                      #         the control socket and peer mux
```

### Interfaces

#### CLI surface

```
classic spawn --requires "<predicate>" [--rank "<expr>"] [--exclusive-device] \
              [--stdin <file>] [--env KEY=VAL]... -- <argv>...
```

- `--requires <expr>` (**required**) — plan-03 predicate. Empty string
  forbidden; use `true` for "any node".
- `--rank <expr>` — optional rank; default `least_loaded` (plan 03).
- `--exclusive-device` — mark every acquired device cap exclusive
  (default: shared).
- `--stdin <file>` — stream `<file>` as child stdin; `-` means inherit
  the CLI's stdin.
- `--env KEY=VAL` — repeatable; passed verbatim. CLI's own env is
  **not** forwarded.
- `--` then `<argv>...` — mandatory separator; everything after is the
  child's argv.

CLI exit code: child's exit code on normal exit (0–255); `128 + signum`
on signal death; `2` on any spawn failure (predicate mismatch, all
candidates refused, exec failure); `1` on CLI usage error.

Examples:

```bash
# Run nvidia-smi on any NVIDIA host.
classic spawn --requires "gpu.vendor == 'nvidia'" -- /usr/bin/nvidia-smi

# Train script — needs >=40 GiB free on one GPU, exclusively.
classic spawn --requires "gpu.vendor == 'nvidia' && gpu.free_mb >= 40000" \
              --exclusive-device --env CUDA_VISIBLE_DEVICES=0 \
              -- /usr/bin/python /shared/train.py --epochs 5

# Pipe a corpus into a tokenizer.
classic spawn --requires "memory.free_mb >= 8000" --stdin /shared/corpus.txt \
              -- /usr/local/bin/tokenize --vocab=32000
```

#### `classic-spawn` public API

```rust
/// Originator side. Driven by the local daemon when a SpawnRequest arrives
/// on the CLI control socket.
pub async fn originate(ctx: &OriginatorCtx, req: SpawnRequest, cli: ControlConn)
    -> Result<(), SpawnError>;

/// Executor side. Driven by the local daemon when a SpawnRequest arrives
/// from a peer.
pub async fn execute(ctx: &ExecutorCtx, req: SpawnRequest, peer: PeerConn)
    -> Result<(), SpawnError>;

pub struct OriginatorCtx {
    pub ad_cache: Arc<AdCache>,        // classic-ad
    pub peers: Arc<PeerSet>,           // classic-proto
    pub local_node: NodeId,
    pub local_executor: ExecutorCtx,   // for self-targeted spawns
}

pub struct ExecutorCtx {
    pub broker: Arc<CapBroker>,        // classic-cap
    pub mbox_alloc: Arc<MboxAllocator>,// monotonic u64
    pub local_node: NodeId,
}
```

### Failure handling

| Stage                  | Failure                  | Outcome                                                          |
|------------------------|--------------------------|------------------------------------------------------------------|
| Originator `place()`   | empty result             | `SpawnDeny{NoCandidates}` → CLI exits 2.                         |
| Originator peer dial   | TCP connect fails        | Skip candidate; all-fail ⇒ `AllCandidatesRefused`.               |
| Executor predicate     | stale ad                 | `SpawnDeny{PredicateNotSatisfied}`; originator tries next.       |
| Executor `CapBroker`   | exclusive held           | `SpawnDeny{DeviceTaken}`; originator tries next.                 |
| Executor cgroup mkdir  | EPERM/ENOSPC             | `SpawnDeny{CgroupSetupFailed, detail: <errno>}`.                 |
| Executor BPF attach    | EINVAL                   | `SpawnDeny{CgroupSetupFailed}`; caps released.                   |
| Executor fork/exec     | ENOEXEC/ENOENT           | `SpawnDeny{ExecFailed}`; caps released; **no retry**.            |
| Child crash            | segfault/SIGKILL         | `ChildExit{signal: Some(n), code: None}`; caps released.         |
| Originator disconnect  | CLI Ctrl-C / socket loss | Executor `release_all`s caps; SIGTERM child, SIGKILL after 10 s. |

Originator retries on `PredicateNotSatisfied`, `DeviceTaken`,
`CgroupSetupFailed`, and connection failures. Never on `ExecFailed`
(binary isn't there) or `HopExceeded` (programming error).

## Requirements

### Functional

- [ ] FR-1: `classic spawn --requires "true" -- /bin/echo hi` on a
      single-node cluster prints `hi` and exits 0.
- [ ] FR-2: 2-node setup, predicate matching only the remote → child
      runs remotely (proved by `hostname`).
- [ ] FR-3: Stdout/stderr/stdin pipe end-to-end byte-for-byte (validated
      with `cat`).
- [ ] FR-4: Normal child exit code propagates as CLI exit code.
- [ ] FR-5: Signal-killed child → CLI exits `128 + signum`.
- [ ] FR-6: Unsatisfiable predicate → CLI exits 2, message contains
      `no node matches predicate`.
- [ ] FR-7: Concurrent `--exclusive-device` for the same device:
      second spawn fails with `DeviceTaken` (v1 fail-fast).
- [ ] FR-8: All `DeviceCap`s for a task released within 1 s of exit
      (proved by a follow-up exclusive spawn succeeding).
- [ ] FR-9: SIGINT-ing the CLI SIGTERMs the remote child within 1 s.
- [ ] FR-10: `SpawnRequest` with `hop > 2` is dropped with `HopExceeded`.

### Non-functional

- **Performance:** end-to-end spawn latency (CLI start → child's first
  stdout byte) ≤ 250 ms on a 2-node loopback cluster with `/bin/echo`.
  Placement decision ≤ 50 ms (plan 03).
- **Compatibility:** Linux 6.1+, x86_64 (aarch64 must compile but is
  not gated). NVML required only on hosts publishing `gpu.vendor='nvidia'`.
- **Security:** `classicd` runs as root; children inherit daemon UID
  in v1; trusted cluster (no wire auth).
- **Hardware target:** Ubuntu 24.04, x86_64, one NVIDIA GPU. Multi-GPU
  and PCI passthrough use the same code paths.
- **Resource bounds:** ≤ 1024 concurrent tasks per executor (`pids.max`
  per scope + daemon cap). Per-stream stdio buffer: 1 MiB.

## Testing plan

### Unit

- `classic-proto` frames: round-trip encode/decode for every spawn
  frame; edge cases (empty argv/env, large stdio at frame-size limit).
- `classic-cap` broker: exclusive vs. shared semantics; double-acquire
  refused; release-on-drop; cross-thread acquire stress.
- `classic-cap` cgroup: tempdir-rooted mock sysfs verifying the exact
  write sequence (`cgroup.subtree_control`, `memory.max`, ...).
- `classic-cap` BPF loader: goldenfile for the compiled BPF object;
  load test that fails fast on kernel rejection (skip on non-Linux CI).
- `classic-spawn` originator: state machine driven by a mocked
  `PeerSet` scripting `SpawnAck`/`SpawnDeny`; assert retry on
  `DeviceTaken`, no-retry on `ExecFailed`.
- `classic-spawn` executor: `cgroup_scope` create/teardown under fault
  injection.
- `classic-cli`: arg parsing — every error case yields exit 1 with the
  expected message.

### Integration

- **Single-process loopback.** Two `classicd` instances in one process
  via `tokio::LocalSet` and the in-memory `Connection` impl from
  `classic-proto` test utils. Exercises every frame path without root
  or BPF.
- **Two daemons over UDS** (no root): no-op `CapBroker`, BPF attach
  stubbed; byte-perfect `cat` of `/dev/urandom`-1 MiB round-trip.
- **Predicate fallback.** Scripted ad cache where node A refuses and
  node B accepts; assert `Trying(0) -> Trying(1) -> Running`.

### End-to-end

On a real Linux 6.1+ host, root:

```bash
# T1
sudo classicd --listen 127.0.0.1:7301 --node-id-file /tmp/n1
# T2
sudo classicd --listen 127.0.0.1:7302 --peer 127.0.0.1:7301 --node-id-file /tmp/n2
# T3
classic --connect /run/classicd/control.sock \
        spawn --requires "true" -- /bin/echo hello-from-classic
# expect: prints "hello-from-classic"; CLI exits 0.

# stdio fidelity
dd if=/dev/urandom bs=1M count=4 of=/tmp/in
classic spawn --requires "true" --stdin /tmp/in -- /bin/cat | sha256sum > /tmp/out
sha256sum /tmp/in   # must match /tmp/out

# exclusive-device contention
classic spawn --requires "gpu.vendor == 'nvidia'" --exclusive-device -- sleep 30 &
classic spawn --requires "gpu.vendor == 'nvidia'" --exclusive-device -- /bin/true
# second: exit 2, DeviceTaken in stderr.
```

### Hardware-dependent

Gated `#[cfg(feature = "hw-gpu")]`:

- BPF blocks `/dev/nvidia1` for a task holding only `GpuMinor(0)`:
  `open("/dev/nvidia1")` returns EPERM.
- NVML's `gpu.free_mb` shrinks during a CUDA allocation inside the
  spawned task and recovers on exit — proves the predicate field is
  live.

## Acceptance criteria

- [ ] AC-1: Frames `0x0300–0x0304` encode/decode under bincode v2; no
      frame outside this range is owned by spawn.
- [ ] AC-2: Single-node `classic spawn --requires "true" -- /bin/echo hi`
      prints `hi`, exits 0.
- [ ] AC-3: 2-node setup with predicate forcing remote placement →
      child runs on remote (`hostname`).
- [ ] AC-4: 4 MiB random round-trip via `--stdin` + `/bin/cat` is
      byte-perfect.
- [ ] AC-5: Child exit code (0, non-zero, signal) propagates correctly.
- [ ] AC-6: Unsatisfiable predicate → CLI exits 2 with
      `no node matches predicate`.
- [ ] AC-7: Concurrent `--exclusive-device` for the same device fails
      the second invocation with `DeviceTaken`.
- [ ] AC-8: SIGINT on the CLI kills the remote child within 1 s and
      releases all its caps.
- [ ] AC-9: `task-<MboxId>.scope` is created on spawn, contains the
      child's pid, and is removed after exit.
- [ ] AC-10 (`hw-gpu`): BPF denies `/dev/nvidia1` to a task holding
      only `GpuMinor(0)`.
- [ ] AC-11: All non-hw tests pass on Linux 6.1+ x86_64 in CI.

## Open questions

- **Binary distribution.** v1 assumes `argv[0]` exists at the same path
  on every node. Lifting this is a post-v1 follow-up — most natural once
  plan 06 (9P namespace) lands: stage the binary into a per-task 9P
  share, exec from `/n/job/bin`. Not designed here.
- **CPU / memory limits from the predicate.** v1 writes `max` to both.
  A future plan should let the predicate carry reservations
  (e.g. `--reserve cpu=4,memory=16Gi`) and translate to cgroup writes.
- **Per-task UID.** v1 runs children as the daemon's UID (root, per
  ARCHITECTURE.md trust model). Multi-tenancy needs a UID-mapping
  policy; deferred.
- **Stdin EOF on reconnect.** If the originator↔executor TCP drops
  mid-stream, v1 kills the child rather than delivering EOF to stdin.
  A future plan may distinguish.
- **PTY support.** No `--tty` in v1. Adding it is a frame extension
  (resize events) plus `forkpty`. Reserved in `0x0305+`.
- **Backpressure.** v1 uses bounded 1 MiB per-stream buffers and drops
  the connection if a writer falls behind. Real credit-based flow
  control is deferred.

## References

- `plans/ARCHITECTURE.md` — identity types, frame ranges, cgroup-v2 mandate.
- `plans/01-skeleton-transport.md` — `Frame` codec, `Connection`, peer mux.
- `plans/03-placement-predicates.md` — `place()`, predicate / rank DSL.
- `plans/05-mailbox-service-directory.md` (future) — mailbox layer that
  will eventually be created *for* the spawned task.
- Linux: `Documentation/admin-guide/cgroup-v2.rst`,
  `Documentation/bpf/prog_cgroup_device.rst`.
- ChrysaLisp `kernel/task.lisp` — placement-at-fork prior art.
- libbpf-cargo <https://github.com/libbpf/libbpf-rs>; bincode v2
  <https://docs.rs/bincode/2>.
