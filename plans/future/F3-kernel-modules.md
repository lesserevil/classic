# Feature: Kernel-Side Acceleration (kmods + eBPF)

> **Status:** future
> **Epic bead:** (none — not yet greenlit)
> **Owner:** unassigned
> **Last updated:** 2026-05-07

## Scope

**In scope (when/if v2+ takes this on):**

- A small, **opt-in** out-of-tree Linux kernel module — working name
  `classic-kmod` — loaded only on nodes that explicitly request it, never
  required for any node to participate in the cluster.
- An **eBPF-only path** for any kernel-side helper that eBPF can deliver
  (cgroup programs, tracepoints/kprobes, `BPF_PROG_TYPE_SOCK_OPS`, ringbuf
  consumers). Strongly preferred over a kmod.
- A small, well-bounded set of kernel-side helpers for things user-space
  genuinely cannot do well: atomic / preemptible state freezing for a
  hypothetical migration path, low-latency cluster IPC bypass, kernel-side
  hardware enumeration with hot-plug events, cgroup extensions that don't
  exist as BPF program types yet.
- DKMS-based packaging, signed-module workflow, CI matrix across the
  current Linux LTS and (LTS − 1).

**Explicitly out of scope — and will stay out of scope forever:**

- A **fully kernel-resident SSI implementation** (the MOSIX / Kerrighed /
  OpenSSI failure mode). Every project that tried this died of kernel-patch
  maintenance debt. We will not go there. See *Reasoning*.
- Patching the mainline kernel. This is strictly out-of-tree code, with the
  long-term goal of *upstreaming* anything that earns its keep — not
  forking the kernel.
- Replacing v1's user-space architecture. Every kernel-side helper is an
  optional accelerator behind a feature flag; the user-space implementation
  remains the reference and the fallback.
- Required kernel features for participation. A node without `classic-kmod`
  must remain a first-class cluster member.
- Multi-OS kernel work. Linux only.

## Reasoning

### Why this is v2+ and bounded

Out-of-tree kernel code carries a maintenance treadmill:

1. **Every kernel update is a potential build break.** Internal kernel
   APIs are not stable; VFS hooks, cgroup subsystem registration,
   scheduler classes, etc., move between releases.
2. **Distros gate which modules can load.** Secure Boot, signed-module
   policies, vendor lockdown LSMs, RHEL-style kABI whitelists.
3. **Out-of-tree modules taint the kernel.** Once tainted, vendor
   support contracts may be void — a real concern for AI/ML hosts.
4. **The MOSIX / Kerrighed / OpenSSI lesson.** openMosix shut down in
   2008 in part because keeping a sprawling kernel patch-set in step
   with mainline overwhelmed volunteer effort. Kerrighed and OpenSSI
   suffered the same fate — patches grew too large to forward-port.
   *Every* SSI project that put significant logic in the kernel
   eventually died of this. Classic's user-space-first design is a
   direct response to that history.

So: a kmod has to clear a high bar. The default answer to "should this
be a kmod?" is **no**. Acceptable yeses: "it's an eBPF program"
(preferred — supported, sandboxed, no taint), or "small, opt-in, ≥10×
win over user-space on a real benchmark, with an upstreaming story."

### Alternatives that almost always win over a kmod

Modern Linux gives user space far more reach than it had in 2008:

- **eBPF** — `BPF_PROG_TYPE_CGROUP_DEVICE`, `CGROUP_SOCK`, `KPROBE`,
  `TRACEPOINT`, `SCHED`, `STRUCT_OPS`, `LSM`, ringbuf, BTF.
- **io_uring** — batched syscalls, `IORING_SETUP_SQPOLL`,
  `IORING_OP_SEND_ZC`, multi-shot ops.
- **perf_event_open** — hardware counters, kernel/user tracepoints.
- **netlink uevents** — `udev` already exposes hot-plug events to user
  space.
- **AF_XDP / RDMA verbs / DPDK** — fast paths at or below the syscall
  layer; user space goes close-to-the-metal without kernel code.
- **cgroup v2** — unified hierarchy, BPF-attachable; plan 04 already
  uses `BPF_CGROUP_DEVICE`.
- **fuse / virtiofs / v9fs client** — kernel mount protocols whose
  servers run in user space.

The v1 plans use these consistently: plan 04 does device gating with a
BPF cgroup program (no kmod); plan 06 uses FUSE and notes mainline
`v9fs` as a possible v2 client (both kernel-already-there); plan 02
consumes hardware via sysfs/NVML.

The honest assessment, candidate-by-candidate below, is that **most
plausible kernel-side helpers don't need a kmod at all.** This document
exists to write that down clearly so future contributors don't keep
relitigating it.

### Success in plain English

- For 80% of "wouldn't a kmod be cool here" requests, this doc says no
  and points at the eBPF / io_uring / cgroup-v2 / perf alternative.
- If a kmod is ever shipped, it is small (single-digit thousand LoC),
  opt-in, DKMS-packaged, builds cleanly against current LTS and LTS−1,
  and demonstrably wins ≥10× over its user-space alternative on a real
  workload.
- Classic remains a fully functional user-space SSI when no kmod is
  loaded anywhere in the cluster.

## Design

The Design section is a candidate-by-candidate review. The first question
on every candidate is: *can eBPF / io_uring / cgroup v2 / perf do this?*
If yes, it's not a kmod candidate. If no, we ask whether the cost is
worth it.

### Candidate 1 — Process-freeze coordination for migration

**Demand.** A future migration plan (F1) would need to atomically
freeze a process, checkpoint it, ship it elsewhere, and resume it.

**Mainline alternatives.**

- **cgroup v2 freezer** — `cgroup.freeze` freezes every task in the
  cgroup at a safe point. The canonical Linux freezer.
- **CRIU** — integrates the cgroup freezer with its own `seize`/`dump`
  machinery via ptrace and parasite code injection. The de facto
  user-space checkpoint/restore solution.

**Conclusion: no kmod.** cgroup v2 freezer + CRIU is sufficient. If F1
needs to evolve beyond CRIU, contribute to CRIU rather than shipping a
kmod. Document this in F1.

### Candidate 2 — Hot-plug event firehose

**Demand.** Plan 02 (`classic-ad`) re-publishes a `NodeAd` when
hardware appears or disappears, and currently polls sysfs.

**Mainline alternatives.**

- **netlink uevents** (`NETLINK_KOBJECT_UEVENT`). Every kernel-emitted
  hot-plug event is already broadcast to user space; `libudev`,
  `udevadm monitor`, or a raw netlink socket consume them directly.
- **`inotify` on `/sys/bus/pci/devices/`** — coarser but trivial.

**Conclusion: no kmod.** Replace polling with a netlink uevent
consumer. A kmod would buy microseconds on an event that fires
minutes-to-hours apart.

### Candidate 3 — Low-latency cluster IPC bypass

**Demand.** Plan 05 (mailbox / service directory) ships messages over
plan 01's TCP frame mux. For collective AI/ML workloads, per-message
latency floor matters.

**Mainline / user-accessible alternatives.**

- **io_uring** — `IORING_SETUP_SQPOLL` lets a user-space thread post
  sends without a syscall on the hot path; `IORING_OP_SEND_ZC` does
  zerocopy TCP.
- **AF_XDP** — kernel-bypass packet I/O from user space.
- **RDMA verbs** — true kernel-bypass on supported NICs.
- **DPDK** — entirely user-space NIC drivers.

**Conclusion: no kmod.** The kernel-bypass ladder is well-developed.
Plan 05 should adopt io_uring SQPOLL first, then RDMA on capable
hardware, then AF_XDP. None require our own kmod.

### Candidate 4 — Cgroup device-controller extensions

**Demand.** Plan 04 already uses `BPF_CGROUP_DEVICE`. Future quota
work (plan 03 predicates → cgroup writes) will lean harder on the
controller surface.

**Mainline alternatives.**

- **BPF cgroup programs** — `BPF_PROG_TYPE_CGROUP_DEVICE` (in v1),
  `CGROUP_SKB`, `CGROUP_SOCK_ADDR`, `CGROUP_SYSCTL`, `CGROUP_GETSOCKOPT
  /SETSOCKOPT`. The program-type set grows each release.
- **Stock cgroup v2 controllers** — `cpu`, `memory`, `pids`, `io`,
  `rdma`, `misc`.

**Conclusion: no kmod, ever.** A custom cgroup subsystem is the exact
shape that killed past SSI projects: subsystem registration is
internal API, churns, forks userland tooling. If something can't be
expressed in BPF, propose a new BPF program type upstream.

### Candidate 5 — Kernel-resident service-directory cache

**Demand.** Imagine a hot kernel-side path — packet rewrite, socket
redirect — that resolves a Classic service name to a `NetId` per
packet. A user-space round-trip would be too slow.

**Alternatives.**

- **eBPF maps populated from user space.** `BPF_MAP_TYPE_HASH` keyed
  by service-name hash, value `NetId`; plan 05 updates from user
  space, BPF programs `bpf_map_lookup_elem` from kernel. Canonical.
- **A kmod exposing `/dev/classic-svc` or a sysfs file.** Possible
  but adds an ABI surface, and any kernel-side consumer is *probably
  itself a BPF program* — putting the cache in the wrong place.

**Conclusion: probably no kmod.** Investigate eBPF maps when (if) a
kernel-side consumer materialises. Don't speculate — wait for a real
user.

### Candidate 6 — Plan 9 v9fs server in-kernel

**Demand.** Plan 06 mounts per-task namespaces through FUSE; each
syscall does user→kernel→user→FUSE. Plan 06's open question 4 asks
about kernel `v9fs` instead.

**Alternatives.**

- **Mainline `v9fs` *client*** mounted against plan 06's `LocalServer`
  over a Unix socket / vsock / tcp. Kernel client → user-space server,
  no FUSE hop on the read path. The natural v2 evolution.
- **In-kernel 9P *server*** — would mean running plan 06's synthetic
  tree code inside the kernel, reading `NodeAd`, `ServiceTable`,
  `ProcTable`, capability state. Disastrous attack surface and ABI.

**Conclusion: no kmod.** Mainline `v9fs` (kernel client) + our
user-space server is the right architecture. Plan 06 already lists
this as a v2 candidate; F3 endorses it explicitly.

### Summary table

| Candidate                              | Mainline alt                       | Need kmod? |
|----------------------------------------|------------------------------------|------------|
| Process-freeze for migration           | cgroup v2 freezer + CRIU           | No         |
| Hot-plug event firehose                | netlink uevents                    | No         |
| Low-latency cluster IPC bypass         | io_uring / RDMA / AF_XDP / DPDK    | No         |
| Cgroup device controller extensions    | BPF cgroup programs                | No         |
| Kernel-resident service-directory cache| eBPF maps populated from user space| Probably no|
| 9P namespace server in-kernel          | mainline v9fs client + UC server   | No         |

The strong default conclusion: **no kmod is currently justified.** This
document's main job is to keep that default explicit so future
contributors who feel the pull of a kmod have to argue against it
specifically, with a workload-grounded benchmark.

### If a kmod ever ships — required shape

Should a future workload force the issue, the kmod must:

1. Be **opt-in** at module-load time; a node without it is a
   first-class cluster member.
2. Live in a separate repo (`classic-kmod`), build with kbuild, ship
   via DKMS, sign cleanly under standard distro signing.
3. Expose its surface through `/dev/classic-<feature>` (cdev) or
   sysfs, *not* new syscalls. Cdev/sysfs is conventional and
   deprecatable; new syscalls are a hard upstream fight.
4. Be feature-flagged in `classicd`, which autodetects the device node
   and falls back to user-space otherwise.
5. Have a credible upstreaming story alongside the code. If we'd
   never propose it to LKML, we shouldn't ship it.

## Requirements

### Functional

- [ ] FR-1: Every kernel-side helper proposed for Classic is reviewed
      against the alternatives table above before any code is written.
- [ ] FR-2: The reference user-space implementation of every feature
      remains in-tree and tested. The kmod, if any, is a code-path
      switch, never a replacement.
- [ ] FR-3: A node without any classic kernel module loaded participates
      fully in the cluster. CI verifies this configuration.
- [ ] FR-4: If shipped, `classic-kmod` is loaded only when explicitly
      requested at the node level (config flag or systemd unit override).
      `classicd` does not auto-load it.
- [ ] FR-5: If shipped, `classic-kmod` exposes its API via cdev or sysfs
      with a documented, versioned ABI. No new syscalls.

### Non-functional

- **Maintainability:** module must build against the current Linux LTS
  *and* (LTS − 1). CI matrix runs every commit against both.
- **Distribution:** DKMS source package; signed binary packages for
  Ubuntu LTS and RHEL-family; respects Secure Boot.
- **Performance:** any kmod-only path must demonstrate ≥10× wall-clock
  win on a representative real benchmark vs. the user-space alternative,
  measured under the same kernel version. Synthetic microbenchmarks do
  not count.
- **Compatibility:** never required for cluster participation; never
  affects the wire protocol.
- **Security:** classic-kmod must pass `sparse`, `smatch`, and `coccinelle`
  checks, and a fuzzing pass on every public ioctl/sysfs entry point.

## Testing plan

### Unit / kernel-side

- **KUnit** tests for pure-logic kernel code (parsers, lookup tables,
  refcount transitions); runs in QEMU.
- **kbuild W=1** clean across the supported kernel matrix.
- **kmemleak** clean across a full unit-test pass.
- **`lockdep`** enabled in CI; any locking violation fails the build.

### Fuzzing

- **syzkaller** harness covering every cdev ioctl, sysfs file, and
  netlink family the module exposes; run continuously.
- **eBPF verifier corpus** — replay curated programs against every
  supported kernel; fail on any verifier rejection regression.

### Integration

- **QEMU harness.** `cargo xtask kmod-test` boots minimal QEMU with the
  built module, runs the user-space integration suite, captures
  `dmesg`. Same matrix as kbuild.
- **Real-hardware soak.** For any kmod feature touching hardware
  (RDMA, GPU minor mapping), ≥1 h soak on a hardware-CI node before
  each release tag.

### Regression matrix

For every supported kernel:
- DKMS install + load + unload, no taints other than `OOT`.
- `classicd` self-test passes with module loaded.
- `classicd` self-test passes with module *not* loaded (parity check).

### Hardware-dependent

Most candidates don't require kernel-side hardware tests because they
don't survive the candidate review. Candidate 3 (low-latency IPC), if
ever shipped, needs an RDMA-capable NIC on a hardware-CI node.

## Acceptance criteria

These are roadmap-level. Anything below is a hurdle to clear *before*
the project ships a kmod, not a checklist for delivering one.

- [ ] AC-1: Each proposed kernel-side helper has a written, dated
      review (in this doc or its successor) explaining why eBPF /
      io_uring / cgroup v2 / perf / netlink is or isn't sufficient.
- [ ] AC-2: At least one production-grade workload demonstrably benefits
      from the kmod by ≥10× (wall-clock or throughput) over the
      user-space alternative on a real benchmark, reproducible by an
      external party. Synthetic microbenchmarks do not satisfy this.
- [ ] AC-3: The module builds cleanly against current LTS and LTS − 1,
      with kbuild `W=1`, in CI, on every commit.
- [ ] AC-4: A node with no Classic kernel modules loaded passes the
      full cluster test suite (parity with the kmod-loaded path).
- [ ] AC-5: DKMS source package installs and loads on stock Ubuntu LTS
      and RHEL-family hosts under default Secure Boot policies.
- [ ] AC-6: A documented upstreaming proposal exists for every kernel-
      side feature shipped. We do not ship out-of-tree code we wouldn't
      eventually propose to LKML.
- [ ] AC-7: syzkaller has been pointed at every public surface for at
      least 24 h with zero crashes and zero KASAN reports before each
      release.

## Open questions

1. **Should we ever ship a kmod?** Honest current answer: no evidence
   yet. Keep it future-status until a concrete workload produces a
   benchmark that user-space-only approaches cannot match.
2. **Indefinite eBPF + io_uring + cgroup v2 sufficiency.** The
   user-space boundary keeps moving in our favour. Bet: the answer
   to (1) stays "no" for the foreseeable future, possibly forever.
3. **Kernel-update break policy.** If a kernel update breaks the
   module: pin to LTS only, skip that kernel, or P0 hotfix? Defer
   until we have a kmod. Provisional: pin to LTS, skip non-LTS
   kernels.
4. **Upstreaming.** Which helper is plausibly mergeable? Realistic
   answer: probably nothing of ours ends up in mainline because we
   shouldn't be shipping kernel code in the first place.
5. **Distros and Secure Boot.** Signing story for locked-down
   kernels? DKMS source + MOK enrollment instructions; accept some
   users won't load it; keep the user-space fallback fast enough.
6. **Out-of-tree taint.** Cost acceptable to our AI/ML user base
   given vendor-driver support contracts? Build with GPL-only symbol
   set where possible; document the taint clearly.

## References

- **openMosix shutdown (2008)** — the canonical "kernel-patch
  maintenance overwhelmed volunteers" SSI failure: LWN coverage and
  the [project shutdown notice](https://en.wikipedia.org/wiki/OpenMosix).
- **Kerrighed retrospective** — distributed-namespace SSI on heavy
  kernel patches; effectively unmaintained after Linux 2.6.x.
- **OpenSSI** — same family, same outcome.
- **eBPF** — kernel `Documentation/bpf/` (notably
  `prog_cgroup_device.rst`, `verifier.rst`) and <https://ebpf.io/>.
- **io_uring** — `Documentation/io_uring/`; Axboe, "Efficient IO with
  io_uring" (2019).
- **cgroup v2** — `Documentation/admin-guide/cgroup-v2.rst`.
- **netlink uevents** — `Documentation/driver-api/uevent.rst`,
  `udev(7)`.
- **v9fs (kernel 9P client)** — `Documentation/filesystems/9p.rst`.
- **DKMS** — <https://github.com/dell/dkms>.
- **CRIU** — <https://criu.org/>; cgroup v2 freezer + ptrace; the
  right home for any future migration logic.
- **AF_XDP** — `Documentation/networking/af_xdp.rst`.
- **RDMA verbs** — `libibverbs`, kernel `drivers/infiniband/`.
- `plans/ARCHITECTURE.md` — user-space-first stance; kernel modules
  already in v1 out-of-scope list.
- `plans/04-spawn-pipeline.md` — `BPF_CGROUP_DEVICE` pattern this doc
  points to as the way forward.
- `plans/06-9p-namespace-server.md` — open question 4 (kernel `v9fs`);
  endorsed here.
- `plans/02-node-ad-hw-discovery.md` — sysfs/NVML hardware discovery;
  netlink-uevent migration belongs there, not here.
