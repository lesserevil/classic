# Feature: End-to-end multi-node demo + integration tests

> **Status:** draft
> **Epic bead:** `bd-XXX` (filed after this doc lands)
> **Owner:** shedwards
> **Last updated:** 2026-05-07

## Scope

### In scope

- `examples/multinode/` tree: README, Makefile, three node configs, two demo
  programs, five scenario scripts, and a Docker-Compose-based CI smoke
  harness with NVML stubbed out.
- Documented 3-node topology (dev workstation + GPU host + CPU-only worker).
- A new `cargo xtask demo-smoke` subcommand that runs the no-real-hardware
  scenarios in CI.
- A human sign-off checklist that closes the loop on real hardware.

### Out of scope (explicitly)

- **Any new feature work.** No new crates, no new wire frames, no new CLI
  flags. If a scenario surfaces a missing feature, it becomes a follow-up
  bead and the scenario is marked `xfail` — it is **not** retrofitted here.
- **Performance benchmarking.** Throughput / latency / scale belong to a
  later epic. Smoke target measures correctness only.
- **Long-running soak / chaos tests.** Future reliability epic owns these.
- **Authentication or multi-tenant isolation.** Trusted-cluster v1
  assumption from `ARCHITECTURE.md` carries through.
- **Cross-architecture testing.** x86_64 only.

## Reasoning

Plans 01–07 each carry their own unit and (where applicable) two-node
integration tests. Those prove the parts work in isolation; they do not
prove a user with three real Linux boxes can follow the docs and watch a
GPU job placed by a predicate land on the right node and produce real
output. Plan 08 closes that gap, and doubles as the project's first
regression contract: every future feature must keep these scenarios green.

### Alternatives considered

1. **Per-plan e2e tests, no central demo.** Rejected: leaves no single
   "does the system work?" answer, and forces newcomers to assemble a
   story from seven disjoint test suites.
2. **Kubernetes-based test harness.** Rejected: pulls in operational
   complexity the project explicitly avoids (`ARCHITECTURE.md`: "Static
   peer config initially"). Compose with `classicd` as PID 1 inside each
   container is enough for CI; bare-metal `classicd` invocations are
   enough for the manual demo.
3. **Web UI driving scenarios.** Rejected — the CLI is the contract
   surface, and a UI is a feature, not a test.

### Success in plain English

A developer with a workstation and two cloud VMs (one with GPU passthrough)
clones the repo, builds release binaries, copies three config files into
place, starts `classicd` on each host, runs
`make -C examples/multinode all-real-hardware`, and watches five scenarios
print `OK`. CI runs three of those scenarios on every PR with no real GPU.

## Design

### Architecture

Plan 08 is a thin shell over the binaries delivered by plans 01–07. There
is no Rust library code here apart from the xtask helper; everything else
is shell, Compose YAML, two tiny C / CUDA programs, and Markdown.

```
examples/multinode/
├── README.md
├── Makefile                       setup, scenario-1..5, all-real-hardware, clean
├── config/{node-a,node-b,node-c}.toml
├── programs/
│   ├── cuda_hello/{Makefile,cuda_hello.cu}
│   └── pci_lspci_loop/{Makefile,pci_lspci_loop.c}
├── scenarios/{01-hw-affinity,02-no-match,03-service-lookup,
│              04-namespace-bind,05-pack-group}.sh
├── lib/assert.sh
└── ci/{compose.yaml,Dockerfile.node,nvml-stub.c,smoke.sh}

xtask/src/
├── main.rs                        # MODIFIED — registers `demo-smoke`
├── demo_smoke.rs                  # NEW — orchestrates compose + smoke.sh
└── nvml_stub.rs                   # NEW — builds nvml-stub.so for CI
```

No `crates/` are touched. No new workspace members.

### Topology

```
Subnet 10.42.0.0/24 (Ethernet)

  node-a 10.42.0.10  dev box, no GPU,           classicd + classic CLI
  node-b 10.42.0.11  1× NVIDIA ≥ 16 GiB VRAM,   classicd, NVML present
  node-c 10.42.0.12  CPU-only worker,            classicd
```

All three peers are wired statically into each other's `[[peer]]` blocks
per `ARCHITECTURE.md` v1. Default daemon port assumed `7421` (confirmed by
implementer against plan 01).

### Data shapes

No new wire types. Plan 08 consumes existing frames from plans 01–07
(`NodeAd`, `SpawnRequest`/`Ack`/`Deny`, `MailSend`, `ServiceAd`,
`ServiceLookup`, raw 9P, `PlacementRequest`/`Response`). **No new
`FrameKind` allocations.**

### CLI surface (consumed, not added)

```
classic spawn --requires "<predicate>" -- <argv...>
classic spawn --pack <N> --requires "<predicate>" -- <argv...>
classic spawn --bind-remote node=<id|hostname> at=<path> -- <argv...>
classic spawn --service <name> --node <hostname> -- <argv...>
classic ps [--all] [--json] [--hardware <node>]
classic mail --service <name> -- <payload>
classic kill <task-id>
```

Spellings inherit from plans 04/07; if upstream renames, scenario scripts
update in the same PR.

### Exact `config.toml` per node

`config/node-a.toml` (dev workstation, no GPU):

```toml
node_name = "node-a"
listen    = "10.42.0.10:7421"
state_dir = "/var/lib/classicd"

[hardware]
gpu      = false
discover = ["pci", "cpu", "mem"]

[[peer]]
addr = "10.42.0.11:7421"
hint = "node-b"

[[peer]]
addr = "10.42.0.12:7421"
hint = "node-c"

[log]
level = "info"
```

`config/node-b.toml` (GPU host):

```toml
node_name = "node-b"
listen    = "10.42.0.11:7421"
state_dir = "/var/lib/classicd"

[hardware]
gpu      = true
discover = ["pci", "cpu", "mem", "nvml"]

[[peer]]
addr = "10.42.0.10:7421"
hint = "node-a"

[[peer]]
addr = "10.42.0.12:7421"
hint = "node-c"

[log]
level = "info"
```

`config/node-c.toml` (CPU worker):

```toml
node_name = "node-c"
listen    = "10.42.0.12:7421"
state_dir = "/var/lib/classicd"

[hardware]
gpu      = false
discover = ["pci", "cpu", "mem"]

[[peer]]
addr = "10.42.0.10:7421"
hint = "node-a"

[[peer]]
addr = "10.42.0.11:7421"
hint = "node-b"

[log]
level = "info"
```

### Demo programs

- `cuda_hello.cu` — single CUDA kernel that prints
  `hello from gpu <ord> on node <hostname>` from device 0 and exits 0.
  CI replaces it with a host-only stub printing the same line when
  `CLASSIC_NVML_STUB=1` is set.
- `pci_lspci_loop.c` — opens the namespace path passed on argv (e.g.
  `/cluster/B/dev/pci/`), reads each entry, prints
  `pci: <bdf> <vendor>:<device>`, sleeps 100 ms, repeats 10×. Pure libc.

### CI Compose layout

`examples/multinode/ci/compose.yaml`:

```yaml
version: "3.9"

x-base: &node-base
  image: classic-test-node:latest
  build:
    context: ../../..
    dockerfile: examples/multinode/ci/Dockerfile.node
  cap_add: [SYS_ADMIN, SYS_RESOURCE]
  security_opt: [seccomp:unconfined]
  networks: [cluster]

services:
  node-a:
    <<: *node-base
    hostname: node-a
    networks: { cluster: { ipv4_address: 10.42.0.10 } }
    command: ["/usr/local/bin/classicd", "--config", "/etc/classic/node-a.toml"]
    volumes: ["../config/node-a.toml:/etc/classic/node-a.toml:ro"]
  node-b:
    <<: *node-base
    hostname: node-b
    networks: { cluster: { ipv4_address: 10.42.0.11 } }
    environment:
      LD_PRELOAD: /usr/local/lib/nvml-stub.so
      CLASSIC_NVML_STUB: "1"
    command: ["/usr/local/bin/classicd", "--config", "/etc/classic/node-b.toml"]
    volumes: ["../config/node-b.toml:/etc/classic/node-b.toml:ro"]
  node-c:
    <<: *node-base
    hostname: node-c
    networks: { cluster: { ipv4_address: 10.42.0.12 } }
    command: ["/usr/local/bin/classicd", "--config", "/etc/classic/node-c.toml"]
    volumes: ["../config/node-c.toml:/etc/classic/node-c.toml:ro"]
  driver:
    <<: *node-base
    depends_on: [node-a, node-b, node-c]
    entrypoint: ["/workspace/examples/multinode/ci/smoke.sh"]
    environment:
      CLASSIC_PEER: "10.42.0.10:7421"

networks:
  cluster:
    driver: bridge
    ipam:
      config: [{ subnet: 10.42.0.0/24 }]
```

`Dockerfile.node` is a thin Ubuntu 24.04 image with `classicd`, `classic`,
the demo binaries, and `nvml-stub.so` copied into place. The stub exposes
the NVML symbols plan 02 calls (`nvmlInit_v2`, `nvmlDeviceGetCount_v2`,
`nvmlDeviceGetHandleByIndex_v2`, `nvmlDeviceGetName`,
`nvmlDeviceGetMemoryInfo_v2`, `nvmlDeviceGetUUID`, `nvmlShutdown`) and
reports a single fake 24 GiB device.

`cargo xtask demo-smoke` chain: build release binaries → build
`nvml-stub.so` and host-only `cuda_hello` → `docker compose up --build -d`
→ `compose exec driver smoke.sh` → `compose down -v` → exit with the
inner code.

## Requirements

### Functional

- [ ] FR-1: `make -C examples/multinode setup` checks each peer is
      reachable, each `classicd` is up, and NVML is visible on node-b;
      exits non-zero with a remediation hint on any failure.
- [ ] FR-2: Scenario 1 places `cuda_hello` on node-b, prints
      `hello from gpu 0 on node-b`, returns 0.
- [ ] FR-3: Scenario 2 refuses to spawn, stderr contains
      `no node matches`, CLI exits non-zero, no child process runs on any
      node.
- [ ] FR-4: Scenario 3 coordinator on node-b registers `coord`; worker on
      node-c mails `hello`; coordinator's stdout contains
      `coord: got hello from <NetId>`.
- [ ] FR-5: Scenario 4 spawned task on node-a lists `/cluster/B/dev/gpu/`
      and matches `classic ps --hardware node-b`.
- [ ] FR-6: Scenario 5 PACK-2 group places both members on node-b with
      ≥ 500 ms concurrent overlap; both exit 0.
- [ ] FR-7: `cargo xtask demo-smoke` runs scenarios 1, 2, 3 against the
      Compose stack and exits 0 on success.
- [ ] FR-8: `make clean` removes built binaries, generated logs, and
      Compose volumes / networks.

### Non-functional

- **Reproducibility:** `make setup` + `make scenario-1` succeed on a fresh
  Ubuntu 24.04 install with documented packages, with no undocumented
  manual steps.
- **Performance:** smoke target completes in ≤ 90 s wall time on a 4-core
  CI runner. Each real-hardware scenario completes in ≤ 30 s.
- **Compatibility:** Linux 6.1+ on x86_64 (Ubuntu 24.04 tested).
- **Hardware:** node-b requires one NVIDIA GPU with ≥ 16 GiB VRAM and
  `libnvidia-ml.so.1` on the default linker path.
- **Security:** none added; trusted-cluster v1.

## Testing plan

### Unit

None. Plan 08 owns no library code beyond xtask glue, exercised end-to-end
by `cargo xtask demo-smoke`.

### Integration — five scenarios

Each script lives in `examples/multinode/scenarios/` and follows the
shape: source `lib/assert.sh`, check prereqs, run the command, capture
stdout / stderr / exit, assert, print `OK`.

#### Scenario 1 — HW-affinity placement

- **Prerequisites:** all three daemons up; node-b advertises a GPU with
  ≥ 8 GiB VRAM in its `NodeAd`.
- **Command (on node-a):**
  ```
  classic spawn \
    --requires 'any(gpu, gpu.vendor == 0x10de && gpu.vram_mb >= 8000)' \
    -- ./cuda_hello
  ```
- **Expected stdout:** `hello from gpu 0 on node-b`.
- **Expected stderr:** empty (modulo `[classic]`-tagged log lines, filtered).
- **Expected exit code:** `0`.
- **Success means:** predicate matched node-b only; spawn pipeline staged
  the binary, applied the GPU device-cap, ran it on node-b, streamed
  stdout back, and reported the exit cleanly.

#### Scenario 2 — No-match failure

- **Prerequisites:** all three daemons up. No node has ≥ 1 TiB VRAM.
- **Command (on node-a):**
  ```
  classic spawn \
    --requires 'any(gpu, gpu.vram_mb >= 1000000)' \
    -- ./cuda_hello
  ```
- **Expected stderr:** contains literal `no node matches`.
- **Expected stdout:** empty.
- **Expected exit code:** non-zero.
- **Success means:** placement layer refused before any `SpawnRequest`
  was sent. Verified by post-hoc `classic ps --all` showing no record on
  any node.

#### Scenario 3 — Service registration + cross-node lookup

- **Prerequisites:** all three daemons up; service name `coord` unused.
- **Commands:** terminal 1 (foreground) on node-a:
  ```
  classic spawn --service coord --node node-b -- coord-server
  ```
  where `coord-server` is a generated bash one-liner installed by
  `make setup`:
  `while read line; do echo "coord: got $line from $CLASSIC_PEER_NETID"; done`.
  Terminal 2 on node-a:
  ```
  classic spawn --node node-c -- \
    sh -c 'classic mail --service coord -- "hello"'
  ```
- **Expected stdout (terminal 1):** matches
  `^coord: got hello from [0-9a-f]+:[0-9]+$`.
- **Expected exit code:** terminal 2 exits 0; terminal 1 is torn down by
  the scenario script with `classic kill`.
- **Success means:** plan 05 service-directory gossip propagated `coord`;
  node-c's worker resolved it; mail delivered through plan 05's mailbox.

#### Scenario 4 — 9P namespace bind (real hardware only)

- **Prerequisites:** all daemons up; node-b's plan-06 9P server exposing
  its hardware tree.
- **Command (on node-a):**
  ```
  classic spawn --bind-remote node=node-b at=/cluster/B/ \
    -- ./pci_lspci_loop /cluster/B/dev/pci/
  ```
- **Expected stdout:** at least one line per PCI device on node-b matching
  `^pci: [0-9a-f]{4}:[0-9a-f]{2}:[0-9a-f]{2}\.[0-9] [0-9a-f]{4}:[0-9a-f]{4}$`,
  repeated 10×.
- **Expected exit code:** `0`.
- **Success means:** node-a's mount of node-b's `/dev/pci` over 9P is
  live; the BDF set matches `lspci -n` on node-b.

#### Scenario 5 — Placement group PACK (real hardware only)

- **Prerequisites:** all daemons up; node-b GPU device-cap idle.
- **Command (on node-a):**
  ```
  classic spawn --pack 2 \
    --requires 'any(gpu, gpu.vendor == 0x10de && gpu.vram_mb >= 8000)' \
    -- ./cuda_hello
  ```
- **Expected stdout:** two `hello from gpu 0 on node-b` lines (any order).
- **Expected exit code:** `0` for the group; both members 0.
- **Success means:** plan 07 PACK strategy placed both on node-b;
  `classic ps --json` shows both `placed_node = node-b` with ≥ 500 ms
  start/end overlap.

### End-to-end smoke (CI)

`cargo xtask demo-smoke` runs scenarios 1, 2, 3 inside the Compose stack
described above. Scenarios 4 and 5 require real device files and a real
PACK-able GPU, so they are listed under `make all-real-hardware` and not
under `make smoke`. The README states this explicitly.

### Hardware-dependent

Scenarios 4 and 5 — gated to manual runs only.

## Acceptance criteria

- [ ] AC-1: `examples/multinode/` exists with the file layout above.
- [ ] AC-2: Each `config/*.toml` validates (`classicd --check-config`
      exits 0).
- [ ] AC-3: `make setup` exits 0 on a healthy cluster, non-0 with a clear
      remediation hint when any precondition is missing (peer down,
      daemon down, NVML missing).
- [ ] AC-4: `make scenario-1` … `make scenario-5` each run their script
      with the right `CLASSIC_PEER` env var and print `OK` on success.
- [ ] AC-5: All five scenarios pass on real hardware on the documented
      topology (signed off via the human checklist below).
- [ ] AC-6: `cargo xtask demo-smoke` passes scenarios 1, 2, 3 in CI in
      ≤ 90 s wall time.
- [ ] AC-7: `examples/multinode/README.md` walks a fresh developer from
      `git clone` to a green `make scenario-1` with no undocumented steps
      (validated by a colleague new to the project).
- [ ] AC-8: README's troubleshooting section names at least: peer
      unreachable, NVML missing, GPU passthrough not configured,
      libnvidia-ml ABI mismatch, port 7421 firewalled, hostname not
      resolvable from peers.
- [ ] AC-9: No new crates added (`cargo metadata` workspace-member diff
      is empty).
- [ ] AC-10: No new `FrameKind` values allocated.

### Definition of multi-node demo success — human sign-off checklist

A reviewer signs the demo off only after running, on real hardware, all
of the following and recording the output in the PR description:

- [ ] `make -C examples/multinode setup` returned 0 and listed all three
      peers as reachable.
- [ ] `make scenario-1` printed `OK`; captured stdout contained
      `hello from gpu 0 on node-b`.
- [ ] `make scenario-2` printed `OK`; CLI exit non-zero;
      `classic ps --all` on every node shows no record of the rejected
      task.
- [ ] `make scenario-3` printed `OK`; coordinator received exactly one
      `hello` from a NetId whose node component matches node-c's
      `NodeId`.
- [ ] `make scenario-4` printed `OK`; the `pci_lspci_loop` BDF set
      matches `lspci -n` on node-b.
- [ ] `make scenario-5` printed `OK`; `classic ps --json` shows both
      members `placed_node = node-b` with ≥ 500 ms time overlap.
- [ ] `cargo xtask demo-smoke` passes locally as well.
- [ ] `make clean` returns the tree to no leftover Compose volumes or
      built binaries.

## Open questions

- **CLI flag spelling.** Plans 04 and 07 may settle on different names
  than `--requires`, `--pack`, `--bind-remote`, `--service`, `--node`.
  Resolution: scripts use whatever plan 04/07 ship; this doc updates in
  the same PR. Shape (one flag per concept) does not change.
- **Default daemon port.** Plan 01 has not committed `7421` in writing.
  Implementer confirms and updates configs + Compose YAML in same PR.
- **NVML stub completeness.** Stub list above covers plan 02's draft.
  Implementer extends the stub if plan 02 adds calls.
- **`--node` on `classic spawn`.** Used by scenarios 3 and 4. Plan 04
  should already deliver this; if missing, file a follow-up bead and pin
  those scenarios on the feature beads (do **not** retrofit).
- **Scenario 3's `coord-server` helper.** Decided no for now — lives
  inline in `make setup` as a generated script. Revisit if a third
  scenario needs the same shape.

## References

- `plans/ARCHITECTURE.md` (v1, 2026-05-07) — identity types, frame ranges,
  Linux runtime assumptions, repo layout.
- `plans/TEMPLATE.md` — section structure.
- `AGENTS.md` § "Feature Development Workflow" — quality bar.
- Plan 01 — skeleton + transport (provides `classicd`, peer config, port).
- Plan 02 — node ad + hardware discovery (NVML on node-b).
- Plan 03 — placement predicates (the `--requires` DSL).
- Plan 04 — spawn pipeline + CLI (`classic spawn`, stdio streaming, exit).
- Plan 05 — mailbox + service directory (scenario 3).
- Plan 06 — 9P namespace server (scenario 4).
- Plan 07 — placement groups (`--pack`, scenario 5).
- 9P2000.L spec — Plan 9 from Bell Labs.
- NVML reference — NVIDIA Management Library docs.
