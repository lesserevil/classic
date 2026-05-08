# Feature: QEMU-based final e2e test tier

> **Status:** draft
> **Epic bead:** `bd-XXX` (filed after this doc lands)
> **Owner:** TBD
> **Last updated:** 2026-05-07

## Scope

### In scope

A QEMU-based test harness positioned as the **final** e2e tier — slower and more thorough than the Docker-Compose smoke run, intended for pre-release verification. Specifically:

- A `cargo xtask qemu` runner that boots minimal Linux VMs from prebuilt rootfs images and direct kernel-line `-kernel`/`-initrd` boots.
- A **kernel-version matrix** (Linux 6.1 LTS, 6.6 LTS, 6.12 LTS) covering each test case where kernel behavior matters (BPF cgroup-v2 device controller, v9fs client, cgroup unified hierarchy nuances).
- **Single-VM cases** for kernel-sensitive tests:
  - Plan 04: BPF cgroup-v2 device-controller behavior — the test from plan 04 hardware section ("BPF blocks `/dev/nvidia1` for a task holding only `GpuMinor(0)`") runs against each kernel in the matrix using a fake `/dev/nvidia*` mknod and a synthetic NVML stub.
  - Plan 06: FUSE smoke (`/dev/fuse` available), `9pfuse` interop in a clean image, and **v9fs kernel-client interop** (mount the classic-fs server via the in-kernel v9fs driver instead of FUSE — currently a plan 06 open question).
- **Multi-VM cases** for the headline scenarios:
  - A 3-VM topology mirroring plan 08's three nodes (CPU dev box + GPU host + CPU worker), networked via QEMU socket transport (no host bridge needed).
  - Plan 08 scenarios 1, 2, 3 (HW-affinity placement, no-match failure, cross-node service+mailbox) re-run in QEMU **instead of** the Compose harness when invoked via `cargo xtask qemu --scenarios`.
  - The "GPU host" VM uses an `LD_PRELOAD` NVML stub identical to plan 08's, plus `mknod` of fake `/dev/nvidia*` minors so the BPF device cgroup has something to gate.
- **CI integration: opt-in only.**
  - A separate GitHub Actions workflow `.github/workflows/qemu-e2e.yml` triggered by `workflow_dispatch` (manual button) and by pull-request label `qemu-e2e`.
  - **NOT** triggered by `cargo test`, `cargo xtask test`, `cargo xtask demo-smoke`, push to main, or any default-on path.
  - On tagged release commits, the workflow runs automatically as a release gate (still opt-in in spirit — release tagging is an explicit human action).

### Out of scope

- Replacing or duplicating tiers 1–3 of the testing pyramid (per-crate unit, in-process integration, host-side e2e). Those keep their current responsibilities.
- Real GPU passthrough (vfio-pci of a host GPU into a guest). That belongs in plan 08's hardware-dependent tier on real hardware. We use stubs in QEMU so this tier doesn't require a host GPU.
- Cross-architecture (aarch64) QEMU. v1 is x86_64 only per ARCHITECTURE.md; aarch64 QEMU could be added once aarch64 is a v1 test target.
- Live migration testing (that's `plans/future/F1-runtime-migration.md`).
- Kernel-module testing (that's `plans/future/F3-kernel-modules.md`, which itself proposes a separate QEMU harness; if/when F3 lands, the two harnesses should be unified — noted as an open question in F3).
- Fuzzing inside QEMU. Out for v1; could be added later (syzkaller-style harness on top of this tier).
- Performance benchmarking. QEMU performance numbers are not load-bearing.

## Reasoning

### Why a separate "final" tier

The existing testing pyramid (plans 01–08) catches almost everything fast and cheap. It cannot catch:

- **Kernel-version regressions** in cgroup-v2 device-controller BPF behavior. Docker shares the host kernel; the developer's machine probably runs one specific LTS. A bug introduced by a 6.6 → 6.12 kernel change would silently pass CI and fail in production on hosts with the newer kernel. Plan 04's BPF program is exactly the kind of code where this matters.
- **v9fs client behavior** (plan 06's open question 4). FUSE is well-trodden in CI; v9fs is not. Without testing the kernel client at all, we'd be guessing about its compatibility with our 9P server.
- **Real-NVML code paths** under controlled conditions. Plan 08's Compose smoke uses an `LD_PRELOAD` NVML stub. The stub interposes the API but the real `libnvidia-ml.so.1` code path (initialization, error paths, library version handling) is never exercised in CI. A QEMU VM with a stub `/dev/nvidia*` and the real library installed exercises more of the actual code.
- **Multi-node networking under realistic stack behavior.** Compose uses Docker's network stack with shared kernel; QEMU's user-mode networking stack reflects real socket semantics (port reuse, RST behavior, conntrack, etc.).

### Why "on request" only

QEMU adds 30–60s per test case (boot + run + shutdown), and the kernel matrix multiplies that by 3. A full multi-VM run with the matrix is 5–15 minutes — too slow for `cargo test` and too slow to gate every PR. But it's exactly the right cost to pay before tagging a release or when reviewing changes that touch kernel-sensitive code paths. The label-triggered model lets reviewers opt in per-PR (`/label qemu-e2e`) when the diff warrants it.

### Alternatives considered and rejected

- **Lima / multipass / vagrant.** Heavier per-VM overhead, harder to script kernel selection, more host dependencies. QEMU directly with `-kernel` is the smallest setup that gets us the kernel matrix.
- **Firecracker.** Faster boots, but stripped-down devicetree and limited PCI emulation. Loses too much of what we want to test (cgroup + sysfs + virtio-9p).
- **Bazel / nix-based hermetic VMs.** Would make rootfs reproducibility nicer but adds heavy build-system complexity for v1. Use a pinned Ubuntu cloud image instead and revisit if reproducibility becomes load-bearing.
- **Run tier 3 e2e in QEMU instead of on the host.** Tier 3 e2e is meant to be fast and run on every developer's box. Pushing it through QEMU defeats that. Keep tier 3 on the host; add QEMU as a tier on top.

## Design

### Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ Host (developer or CI runner)                                   │
│                                                                 │
│  cargo xtask qemu --case <name> --kernel <ver>                  │
│      ├─ build classicd + classic CLI for x86_64-unknown-linux   │
│      ├─ acquire rootfs image (cached in target/qemu/images/)    │
│      ├─ acquire kernel + initrd for selected version            │
│      ├─ build COW overlay disk (`qemu-img create -f qcow2`)     │
│      └─ launch qemu-system-x86_64 with:                         │
│           -kernel  vmlinuz-<ver>                                │
│           -initrd  initrd-<ver>                                 │
│           -drive   overlay.qcow2                                │
│           -virtfs  /target/release=hostshare,...  (9p mount)    │
│           -chardev pipe,id=ctrl,path=/run/qemu/ctrl              │
│           -device  virtio-serial,...                            │
│           ...                                                   │
│                                                                 │
│  Host runner ↔ guest test agent over virtio-serial:             │
│     command/result JSONL frames                                 │
└─────────────────────────────────────────────────────────────────┘
                              │ virtio-serial
                              ▼
┌─────────────────────────────────────────────────────────────────┐
│ Guest VM (one per VM in topology)                               │
│                                                                 │
│   /init (statically-linked busybox + classic-test-agent)        │
│       └─ mount /hostshare via 9p, copies classicd into /usr/bin │
│       └─ runs assigned scenario, streams results back           │
│                                                                 │
│   classicd, classic, scenario scripts                           │
└─────────────────────────────────────────────────────────────────┘
```

### Repository layout

```
xtasks/qemu/                          # NEW — owned by this plan
  src/
    main.rs                           # cargo xtask qemu entrypoint
    runner.rs                         # VM lifecycle, virtio-serial proto
    images.rs                         # rootfs + kernel acquisition + cache
    topology.rs                       # 1-VM and 3-VM topologies
    scenarios.rs                      # case selection + dispatch
  agent/                              # in-guest binary (built per-target)
    src/main.rs
  cases/
    bpf-device-cgroup.toml            # plan 04 BPF case
    fuse-smoke.toml                   # plan 06 FUSE
    v9fs-interop.toml                 # plan 06 v9fs (NEW behavior)
    9pfuse-interop.toml               # plan 06 9pfuse
    plan08-scenario1.toml             # HW-affinity placement
    plan08-scenario2.toml             # no-match failure
    plan08-scenario3.toml             # service + mailbox cross-node
  README.md                           # how to run, how to add a case

.github/workflows/
  qemu-e2e.yml                        # NEW — workflow_dispatch + label trigger
```

### Image acquisition

- **Rootfs**: pinned Ubuntu 24.04 cloud image (`focal-server-cloudimg-amd64.img` style, but Noble). Downloaded on first run, SHA-256 verified, cached under `target/qemu/images/`. Not vendored in-repo (size).
- **Kernels**: pinned Ubuntu generic-kernel `.deb`s for each LTS (6.1, 6.6, 6.12 — pick the latest point release available at plan-write time). Extracted to `vmlinuz-<ver>` + `initrd-<ver>`. Same caching + checksum strategy.
- **Test-agent binary**: built once per workspace as `cargo build -p classic-test-agent --release --target x86_64-unknown-linux-musl` so it runs in any guest without libc concerns. Injected into the rootfs via `virtio-9p` host share at boot — not baked in.

### Boot model

- **Direct kernel boot** with `-kernel`/`-initrd`/`-append`. No GRUB. Faster boot (~3 s) and trivial kernel swap.
- **Disk**: read-only base + per-run COW overlay (`qemu-img create -f qcow2 -F qcow2 -b base.qcow2`). Test-clean every invocation.
- **Networking**:
  - Single-VM cases: user-mode SLIRP (`-net user`), no host privileges.
  - Multi-VM cases: per-VM `-netdev socket,...` mesh, plus a small in-host bridge process if needed for >2 VMs. No `tun/tap`, no root.
- **Filesystem sharing**: virtio-9p mounts a single host directory (`hostshare/` containing `classicd`, `classic`, scenario scripts, fixtures) read-only into each guest at `/host`.

### Test agent protocol (virtio-serial)

Newline-delimited JSON over `/dev/virtio-ports/classic-test-agent`:

```json
// host -> guest
{"id":"req-1","cmd":"exec","argv":["/host/scenario-bpf.sh"],"env":{},"timeout_s":30}
{"id":"req-2","cmd":"upload","src":"/host/foo","dst":"/etc/foo","mode":"0644"}
{"id":"req-3","cmd":"shutdown"}

// guest -> host
{"id":"req-1","stream":"stdout","data":"hello\n"}
{"id":"req-1","stream":"stderr","data":""}
{"id":"req-1","done":true,"exit_code":0,"duration_ms":2412}
```

`runner.rs` implements the host side as a `tokio` task; the in-guest agent is a small Rust binary (~300 lines) launched as PID 1 of the initrd or as a systemd unit (depending on rootfs choice; Ubuntu cloud-init path is fine).

### Case definitions

Each `cases/*.toml` is a self-contained recipe:

```toml
name = "bpf-device-cgroup"
plan_ref = "plans/04-spawn-pipeline.md"   # documentation pointer
kernels = ["6.1", "6.6", "6.12"]          # matrix dimension
topology = "single-vm"                     # or "three-vm-classic"
timeout_s = 60

setup = [
  "modprobe fuse 2>/dev/null || true",
  "mknod -m 0666 /dev/nvidia0 c 195 0",
  "mknod -m 0666 /dev/nvidia1 c 195 1",
]

run = [
  "/host/classicd --listen 127.0.0.1:7000 --node-id-file /tmp/n &",
  "sleep 1",
  "/host/classic spawn --requires 'true' --device-cap nvidia:0 --exclusive-device -- /host/test-bpf-blocks-nvidia1.sh",
]

assert = [
  { kind = "exit_code", value = 0 },
  { kind = "stdout_contains", value = "EPERM on /dev/nvidia1: ok" },
]
```

`runner.rs` interprets these declaratively; new cases require only a TOML file + any helper scripts under `hostshare/`.

### What plan 09 owns vs. what it reuses

| Concern                      | Owned by 09 | Reused from              |
|------------------------------|-------------|--------------------------|
| `cargo xtask qemu` runner    | yes         | —                        |
| In-guest test agent          | yes         | —                        |
| Image acquisition + caching  | yes         | —                        |
| Case definitions             | yes         | —                        |
| Multi-VM mesh transport      | yes         | —                        |
| classicd + classic binaries  | no          | plans 01, 04             |
| NVML stub                    | no          | plan 08 (`xtasks/nvml-stub/`) |
| Plan 08 scenario logic       | partial     | plan 08 scenario scripts |
| 9P server, mailbox, etc.     | no          | plans 02–07              |

When plan 08 changes its scenario scripts, plan 09 picks them up automatically (they're invoked from `hostshare/`, not copied). The TOML cases pin which scenarios apply.

## Requirements

### Functional

- **FR-1:** `cargo xtask qemu --list` enumerates all defined cases.
- **FR-2:** `cargo xtask qemu --case <name>` runs that case across the case's declared kernel matrix.
- **FR-3:** `cargo xtask qemu --case <name> --kernel <ver>` runs a single matrix cell.
- **FR-4:** `cargo xtask qemu` (no args) runs every case across every matrix cell.
- **FR-5:** Single-VM cases run without root on the host.
- **FR-6:** Multi-VM cases run without root on the host (no `tun/tap`).
- **FR-7:** First run downloads + caches images; subsequent runs do not re-download. Cache invalidation is explicit (`cargo xtask qemu --refresh-images`).
- **FR-8:** Each case has a hard timeout; on timeout the runner sends a forceful VM shutdown and reports failure with collected logs.
- **FR-9:** A failing case dumps: VM serial log, in-guest agent log, runner log, exit code, last 200 lines of stdout/stderr per command. Saved under `target/qemu/results/<case>/<kernel>/`.
- **FR-10:** No QEMU process is leaked across runs (registered cleanup handlers on SIGINT, panic, normal exit).

### Non-functional

- **Wall-clock budget** (target):
  - Single-case single-kernel: ≤ 90 s on a modern laptop (2024-era 8-core).
  - Full matrix (all cases × 3 kernels): ≤ 20 minutes.
- **Host requirements:** Linux x86_64 with KVM (`/dev/kvm` accessible to the user). macOS and Windows hosts: out of scope for v1; documented in README.
- **Disk:** ~5 GB for cached images.
- **No host network egress** required after first run (offline-capable once cached).
- **Reproducible:** identical inputs produce identical pass/fail for at least 95% of cases. The 5% slack accounts for legitimately-flaky timing-sensitive cases (which we mark and budget for).

## Testing plan

Plan 09 itself has very little internal logic; most of its value is the harness running other plans' scenarios. Tests focus on the harness, not the scenarios.

### Unit (`xtasks/qemu/`)

- `topology::tests` — single-vm and three-vm topology objects produce the right QEMU argv (table-driven).
- `runner::protocol::tests` — virtio-serial JSON frame round-trip; oversize frames rejected; partial reads buffer correctly.
- `cases::tests` — every TOML case parses; references valid kernels; commands are non-empty; assertions are well-formed.
- `images::tests` — cache miss triggers download; cache hit short-circuits; checksum mismatch refuses use.

### Integration (host-only, no QEMU)

- `tests/runner_dryrun.rs` — runner spins up its argv and protocol layer against a fake "VM" (a child process pretending to be the agent over a Unix socket). Validates the runner end-to-end without booting QEMU.

### End-to-end

- `cargo xtask qemu --case bpf-device-cgroup --kernel 6.6` actually boots QEMU and runs the case. CI hook: a single canary cell runs in the `qemu-e2e.yml` workflow's smoke job (~2 minutes); the full matrix runs in the workflow's full job.
- Each case is its own e2e test by construction.

### Hardware-dependent

None at plan 09's level (all hardware is virtualized inside QEMU). The harness host requires KVM, which is "hardware-dependent" in the sense that virt-disabled hosts can't run it — documented as a host requirement, not a test gate.

## Acceptance criteria

- [ ] AC-1: `cargo xtask qemu --list` shows at least the 7 cases enumerated in "Repository layout".
- [ ] AC-2: A fresh checkout on a Linux x86_64 host with KVM can run `cargo xtask qemu --case bpf-device-cgroup --kernel 6.6` and pass within 90 s.
- [ ] AC-3: `cargo test`, `cargo xtask test`, and `cargo xtask demo-smoke` do **not** invoke QEMU. Verified by absence of `qemu-system-x86_64` in their process trees during CI runs.
- [ ] AC-4: The `qemu-e2e` GitHub workflow runs only on `workflow_dispatch` and PR `qemu-e2e` label; pushes to main do not trigger it.
- [ ] AC-5: Plan 08 scenarios 1, 2, 3 each pass in QEMU on Linux 6.6 in three-VM topology.
- [ ] AC-6: Plan 04's BPF device-cgroup test passes on **all three** kernels in the matrix (6.1, 6.6, 6.12).
- [ ] AC-7: Plan 06's v9fs kernel-client interop test passes on at least one kernel; differences across kernels are documented if any.
- [ ] AC-8: A deliberately-failing case (one with `assert.exit_code = 0` against `/bin/false`) produces the documented failure-artifact bundle under `target/qemu/results/`.
- [ ] AC-9: No leaked `qemu-system-x86_64` processes after a successful run, after `Ctrl-C`, or after a panic in the runner.
- [ ] AC-10: README in `xtasks/qemu/` walks a developer through adding a new case (TOML + optional helper script) without modifying Rust source.

## Open questions

1. **Kernel sourcing.** Pinned Ubuntu kernel `.deb`s vs. building kernels from upstream sources (deterministic, slower). Recommend Ubuntu kernels for v1; revisit if kernel config matters for a test.
2. **Rootfs choice.** Ubuntu 24.04 cloud image is convenient but heavy (~600 MB compressed). A custom buildroot or alpine rootfs would shrink to ~50 MB. Defer until image-cache size becomes painful.
3. **NVML library inside the guest.** Do we install the real `libnvidia-ml.so.1` (which won't work without GPU passthrough) and rely on `LD_PRELOAD` stubbing, or do we install only the stub? Recommend: stub-only; document that real-library testing requires real hardware (plan 08 hardware tier).
4. **Multi-VM transport.** Per-VM socket-based mesh works for small N. If we ever want N>4 we need a small host-side broker. Defer until needed.
5. **Coordination with plan F3 (kernel modules).** F3 also proposes a QEMU harness. If F3 lands, the two should be unified — at minimum sharing image acquisition, runner, and test-agent. Decision deferred to F3 promotion time.
6. **Snapshot/checkpoint reuse.** Booting a fresh VM per case is the simplest model but the slowest. A "warm pool" of pre-booted VMs reset via QEMU snapshots could shave 60–80% off repeat runs. Defer; revisit if FR-NF wall-clock target is missed.
7. **macOS / Windows hosts.** macOS dev boxes are common; QEMU there works but with reduced KVM equivalent (HVF on macOS). Worth documenting once a contributor needs it.
8. **PR label name.** "`qemu-e2e`" is concrete but lengthy. Alternatives: `e2e`, `vm-tests`, `final`. Keep as `qemu-e2e` for clarity until a contributor pushes back.

## References

- Plan 04 (`plans/04-spawn-pipeline.md`) — BPF cgroup-v2 device controller, the primary kernel-sensitive code path.
- Plan 06 (`plans/06-9p-namespace-server.md`) — FUSE smoke, 9pfuse interop, v9fs open question 4.
- Plan 08 (`plans/08-multinode-demo.md`) — three-node scenarios that this plan re-runs in QEMU; NVML stub mechanism.
- `plans/future/F3-kernel-modules.md` — the other QEMU-harness proposal; eventual unification target.
- QEMU User Documentation — `https://www.qemu.org/docs/master/system/index.html`
- Linux 9P over virtio — `Documentation/filesystems/9p.rst` upstream.
- Ubuntu kernel package archive — for pinned LTS kernel acquisition.
