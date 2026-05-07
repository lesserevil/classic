# Classic SSI — Architecture Foundations

Single source of truth for cross-cutting decisions. Every per-feature plan in this directory references this doc; **do not redefine the types or wire-format ranges below per-feature.**

> **Status:** approved (v1)
> **Last updated:** 2026-05-07

## Mission

Classic is a single-system image (SSI) cluster runtime for Linux, drawing on ChrysaLisp (mailbox/actor IPC, gossiped service directory, downhill task placement) and Plan 9 (per-process namespaces, hardware-as-files).

The headline feature: users declare hardware requirements per process; the cluster places the process on a node that has matching hardware available.

## Design choices already made (v1)

Do not relitigate without explicit human approval.

| Choice              | Decision                                                                       |
|---------------------|--------------------------------------------------------------------------------|
| Runtime             | Rust user-space daemon on Linux                                                |
| Workload target     | AI/ML — multi-node, GPU-heavy                                                  |
| Migration           | Placement-at-fork only. No CRIU, no live migration.                            |
| Deploy              | One daemon (`classicd`) per Linux host. Static peer config initially.          |
| Trust               | Fully trusted cluster. No node auth, no multi-tenancy enforcement in v1.       |
| Predicate language  | Hand-rolled Rust expression DSL (not CEL/Rhai). See `plans/03-*.md`.           |
| Transport           | TCP + length-prefixed frames, single multiplexed connection per peer pair.    |

## Out of scope for v1

The first five each have a roadmap-level design doc under `plans/future/` (status: `future`). They do not have epics/tasks filed yet — those are generated only when a future plan is promoted to v2 work.

- Runtime process migration (CRIU, DMTCP) — see `plans/future/F1-runtime-migration.md`
- Distributed POSIX file-system semantics / transparent network FS — see `plans/future/F2-transparent-network-fs.md`
- Custom kernel modules — see `plans/future/F3-kernel-modules.md`
- Authenticated node-to-node communication / security against malicious nodes — see `plans/future/F4-node-security.md`
- Multi-tenant resource quotas / namespace isolation — see `plans/future/F5-multitenancy-quotas.md`
- Cross-architecture mixed clusters (assume homogeneous x86_64 first; aarch64 should compile but is not a v1 test target)

## Repository layout

Cargo workspace, single virtual `Cargo.toml` at the repo root.

```
Cargo.toml                       # workspace root
crates/
  classic-proto/                 # wire frames, codec, frame mux
  classic-ad/                    # node ad schema, hardware discovery, gossip
  classic-place/                 # placement predicate DSL, matcher, ranker
  classic-mbox/                  # mailbox runtime, service directory
  classic-fs/                    # 9P2000.L server, namespace primitives
  classic-cap/                   # device capability tokens, cgroup enforcement
  classic-spawn/                 # spawn orchestrator
  classic-node/                  # `classicd` binary — wires everything together
  classic-cli/                   # `classic` user-facing CLI binary
plans/                           # design docs (this directory)
.beads/                          # issue tracker
```

Crate dependency direction (acyclic, top → bottom; arrows = "depends on"):

```
classic-cli ─────────────────────► classic-node
classic-node ────────► classic-spawn, classic-fs, classic-mbox, classic-place
classic-spawn ──────────────────► classic-cap, classic-place, classic-mbox
classic-fs   ─────────────────────► classic-mbox
classic-place ──────────────────────► classic-ad
classic-mbox ──────────────────────► classic-proto
classic-ad   ──────────────────────► classic-proto
classic-cap  ──────────────────────► (stdlib + cgroup + nvml)
```

## Identity types

Defined in `classic-proto`. Every other crate imports from there.

```rust
/// 128-bit non-recurring per-node identity. Generated on daemon first start;
/// persisted at `$CLASSIC_STATE_DIR/node_id` (default
/// `/var/lib/classicd/node_id`, fallback `$XDG_DATA_HOME/classicd/node_id`).
#[derive(Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct NodeId(pub [u8; 16]);

/// Per-task mailbox id. Allocated by the local node; non-recurring within the
/// node's lifetime. Reset on daemon restart.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct MboxId(pub u64);

/// Cluster-wide address. ChrysaLisp's `net_id`.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct NetId {
    pub node: NodeId,
    pub mbox: MboxId,
}
```

`NodeId` is generated with `getrandom` once at first start and persisted. `MboxId` is allocated by a per-daemon atomic counter starting at 1; mbox 0 is reserved for the kernel/control mailbox of each node.

## Wire transport (baseline)

One TCP connection per ordered peer pair. Long-lived; reconnected on failure with exponential backoff (250 ms → 30 s, capped). QUIC may replace this later — keep all transport-specific code inside `classic-proto`. Higher-level crates see only `Frame` and a `Connection` trait.

Frame format (length-prefixed, little-endian):

```
+----------+----------+----------------------+
|  length  |   kind   |       payload        |
|  u32 LE  |  u16 LE  |   length - 2 bytes   |
+----------+----------+----------------------+
```

`length` is the byte count of `kind + payload` (so the on-wire size is `4 + length`).
Max frame size: 16 MiB. Larger payloads are chunked above the frame layer (e.g. multi-frame 9P responses).

### Frame-kind allocation

A single `FrameKind: u16` enum lives in `classic-proto`. Each subsystem owns a contiguous range. **Plans MUST allocate from their assigned range and not poach others.**

| Range            | Owner            | Examples                                |
|------------------|------------------|-----------------------------------------|
| `0x0000–0x00FF`  | classic-proto    | `Hello`, `Heartbeat`, `Bye`, `Error`    |
| `0x0100–0x01FF`  | classic-ad       | `NodeAd`, `AdRequest`, `AdGossip`       |
| `0x0200–0x02FF`  | classic-mbox     | `MailSend`, `MailDeliver`, `ServiceAd`, `ServiceLookup` |
| `0x0300–0x03FF`  | classic-spawn    | `SpawnRequest`, `SpawnAck`, `SpawnDeny`, `ChildStdio`, `ChildExit` |
| `0x0400–0x04FF`  | classic-fs       | 9P2000.L frames (raw — see RFC 9P)      |
| `0x0500–0x05FF`  | classic-place    | `PlacementRequest`, `PlacementResponse` |
| `0x0600–0x06FF`  | (reserved)       | Future: auth, telemetry                 |

Encoding: serde + `bincode` v2 (fixed-int, little-endian) for control frames. 9P frames are wrapped raw (the 9P size prefix is stripped — outer frame length is authoritative).

### Versioning

`classic-proto` exposes:

```rust
pub const PROTO_VERSION: u32 = 1;
```

`Hello` frame carries this version. Mismatched peers log + refuse the connection. v1 makes no backward-compatibility guarantees — bump and break freely.

## Linux runtime assumptions

- Kernel ≥ 6.1 (cgroup v2 unified hierarchy required)
- systemd is **not** required; the daemon runs as a plain process
- `/sys/bus/pci`, `/proc`, `/dev/nvidia*` available where applicable
- Daemon runs as root for cgroup + device-cgroup + NVML access (or with `CAP_SYS_ADMIN` + `CAP_SYS_RESOURCE` + appropriate `/dev` ACLs)
- NVML (`libnvidia-ml.so.1`) available on hosts with NVIDIA GPUs
- Target host distro: Ubuntu 24.04 LTS for v1 dev/test (other modern distros should work)

## Glossary

- **Node** — a Linux host running `classicd`.
- **Task** — a process spawned via `classic spawn`. Has a `NetId`.
- **Mailbox** — endpoint owned by a task; receives messages addressed to a `NetId`.
- **Service** — named (string) endpoint registered in the Service Directory; resolves to one or more `NetId`s.
- **Ad** — a node's published advertisement of its identity, hardware, and current load.
- **Predicate** — boolean expression over an ad's fields. Used to match a job's hardware requirements.
- **Placement group** — bundle of related tasks placed together with a strategy (`PACK` = same node, `SPREAD` = different nodes).
- **DeviceCap** — capability token granting exclusive or shared access to a hardware device.

## Plan index

| #  | Slug                              | Owner crate(s)                       | Depends on plans     |
|----|-----------------------------------|--------------------------------------|----------------------|
| 01 | `01-skeleton-transport.md`        | proto, node                          | (foundation)         |
| 02 | `02-node-ad-hw-discovery.md`      | ad                                   | 01                   |
| 03 | `03-placement-predicates.md`      | place                                | 02                   |
| 04 | `04-spawn-pipeline.md`            | spawn, cap, cli                      | 01, 03               |
| 05 | `05-mailbox-service-directory.md` | mbox                                 | 01                   |
| 06 | `06-9p-namespace-server.md`       | fs                                   | 01, 05               |
| 07 | `07-placement-groups.md`          | place, spawn                         | 03, 04               |
| 08 | `08-multinode-demo.md`            | (integration only)                   | 01-07                |

### Future plans (not v1 — no epics/tasks filed)

| #  | Slug                                       | Topic                                              |
|----|--------------------------------------------|----------------------------------------------------|
| F1 | `future/F1-runtime-migration.md`           | Opt-in CRIU / CRIUgpu live migration               |
| F2 | `future/F2-transparent-network-fs.md`      | Distributed POSIX-ish FS over the 9P namespace     |
| F3 | `future/F3-kernel-modules.md`              | Conservative carve-out (mostly: don't)             |
| F4 | `future/F4-node-security.md`               | mTLS, signed gossip, capability tokens, revocation |
| F5 | `future/F5-multitenancy-quotas.md`         | Tenants, quotas, fair-share, preemption (needs F4) |
