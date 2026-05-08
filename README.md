# Classic

> **Cl**ustered **A**gentic **S**ingle-**S**ystem-**I**mage **C**luster (recursive).

A single-system-image cluster runtime for Linux. Users declare hardware
requirements per process; the cluster picks a node that has matching
hardware available and runs the process there with stdio streamed back
as if it were local.

```text
$ classic spawn --requires 'any(gpu, gpu.vram_mb >= 80000 && !gpu.in_use)' \
                --env RANK=0 --env WORLD_SIZE=8 \
                -- python3 train.py
```

The predicate language matches GPU memory, count, vendor, NUMA topology,
arbitrary `/sys/bus/pci` devices, and ordinary node attributes (CPU
arch, RAM, load). The cluster gossips ads, evaluates the predicate
locally, and ranks survivors with a customizable rank expression
(default: lowest CPU + most idle GPUs). See
[`plans/dsl-reference.md`](plans/dsl-reference.md) for the language
reference.

## Status

**v1 in development.** The architecture is settled — see
[`plans/ARCHITECTURE.md`](plans/ARCHITECTURE.md) for the cross-cutting
source of truth — and feature work is broken into eight numbered plans
plus a QEMU end-to-end harness. Each plan is its own design doc and
shipping unit:

| #   | Plan                                                              | Status                  |
|-----|-------------------------------------------------------------------|-------------------------|
| 01  | [Skeleton + transport](plans/01-skeleton-transport.md)            | done                    |
| 02  | [Node ad + hardware discovery](plans/02-node-ad-hw-discovery.md)  | done                    |
| 03  | [Placement predicate DSL](plans/03-placement-predicates.md)       | done                    |
| 04  | [Spawn pipeline](plans/04-spawn-pipeline.md)                      | most tasks done; CLI ↔ daemon bridge in flight |
| 05  | [Mailbox + service directory](plans/05-mailbox-service-directory.md) | upcoming             |
| 06  | [9P namespace server](plans/06-9p-namespace-server.md)            | upcoming                |
| 07  | [Placement groups](plans/07-placement-groups.md)                  | upcoming                |
| 08  | [Multi-node demo + integration tests](plans/08-multinode-demo.md) | upcoming (the v1 bar)   |
| 09  | [QEMU-based final e2e](plans/09-qemu-e2e.md)                      | scaffolded, on-request  |

Headline features that already work end-to-end:

- Two daemons reach a healthy state on a TCP mesh with heartbeat-driven
  Unhealthy/recovery transitions, peer-restart reconnect, and clean
  SIGTERM / SIGINT shutdown that broadcasts a `Bye` to live peers.
- Each daemon discovers its own hardware (CPU / mem / load / PCI / NUMA
  / GPU via NVML) and gossips a `NodeAd` to the cluster; ads converge
  in well under 3 s with last-writer-wins on `(generation, boot_time)`.
- `classic ad list` / `classic ad show <host>` query the daemon over a
  local UDS admin socket and print JSON or a short table.
- The placement-predicate DSL parses, type-checks, and evaluates against
  `NodeAd` shapes, with a `place(ads, requires, rank)` API that filters
  + ranks + tie-breaks deterministically.

Out of scope for v1: process migration, authenticated node-to-node
traffic, multi-tenant quotas, kernel modules, distributed POSIX
filesystem semantics, mixed-arch clusters.

## Building

Standard Rust toolchain. Tested on Ubuntu 24.04 with `rustc` 1.85+.

```bash
cargo build --workspace          # debug
cargo build --workspace --release
cargo test --workspace
```

Two binaries land in `target/{debug,release}/`:

- **`classicd`** — the cluster daemon. One per Linux host. Reads
  `/etc/classicd/config.toml` (or `--config <path>`); persists a
  `NodeId` under its state directory.
- **`classic`** — the user-facing CLI. `classic ad list`,
  `classic spawn ...`, etc.

## Quick taste (single host)

```bash
# Bring up one daemon on localhost; this writes node_id under state-dir.
mkdir -p /tmp/classic-state
target/debug/classicd --config <(cat <<EOF
[node]
listen_addr = "127.0.0.1:7421"
state_dir   = "/tmp/classic-state"
peers       = []
EOF
)

# In another terminal:
target/debug/classic --state-dir /tmp/classic-state ad list
```

For a multi-host walkthrough see [`plans/08-multinode-demo.md`](plans/08-multinode-demo.md)
— `examples/multinode/` lands with that plan.

## Predicate language

The DSL's reference is [`plans/dsl-reference.md`](plans/dsl-reference.md).
A handful of canonical predicates:

```text
any(gpu, gpu.vendor == 0x10de && gpu.vram_mb >= 80000 && !gpu.in_use)
count(gpu, !gpu.in_use) >= 2
mem.free_mb >= 65536 && load.cpu_pct < 50.0
cpu.arch == "x86_64" && cpu.cores >= 32 && any(pci, pci.vendor == 0x15b3)
```

The default rank rewards low CPU load and idle GPUs:

```text
-load.cpu_pct - 1000.0 * (count(gpu) - count(gpu, gpu.in_use))
```

## Repository layout

```
Cargo.toml             # virtual workspace
crates/
  classic-proto/       # wire frames, codec, frame mux, identity types
  classic-ad/          # NodeAd schema, hw discovery, gossip
  classic-place/       # placement predicate DSL + matcher + ranker
  classic-mbox/        # mailbox runtime, service directory (plan 05)
  classic-fs/          # 9P2000.L server (plan 06)
  classic-cap/         # device capability tokens, cgroup enforcement
  classic-spawn/       # spawn orchestrator
  classic-node/        # `classicd` binary — wires everything together
  classic-cli/         # `classic` user-facing CLI binary
plans/                 # design docs (one per feature)
  ARCHITECTURE.md      # cross-cutting source of truth
  dsl-reference.md     # placement-predicate language reference
  TEMPLATE.md          # template for new plan docs
.beads/                # local issue tracker (bd)
```

Crate dependency direction is documented in
[`plans/ARCHITECTURE.md`](plans/ARCHITECTURE.md) §"Repository layout".

## Contributing

This project uses a document-first, deferred-implementation workflow:
write a design doc, file the epic and tasks, then implement. Details
in [`AGENTS.md`](AGENTS.md). For AI agents working on this repo,
[`CLAUDE.md`](CLAUDE.md) has the agent-specific notes.

Issue tracking lives in `.beads/` (local-only Dolt DB; `bd` CLI). Run
`bd ready` for unblocked work; `bd show <id>` for full task spec.

## License

Dual-licensed under MIT or Apache-2.0, at your option. See
`workspace.package.license` in the root `Cargo.toml`.
