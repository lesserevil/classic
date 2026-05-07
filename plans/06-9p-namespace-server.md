# Feature: 9P2000.L Namespace Server (`classic-fs`)

> **Status:** draft
> **Epic bead:** `bd-XXX` (filed after this doc lands)
> **Owner:** unassigned
> **Last updated:** 2026-05-07

## Scope

**In scope (v1):**

- New crate `classic-fs` hosting a 9P2000.L server, a per-spawn namespace
  assembler, and a FUSE bridge that mounts assembled namespaces into Linux.
- Server-side 9P2000.L protocol carried as raw 9P bytes wrapped inside
  Classic frames in `0x0400–0x04FF`. Outer Classic length is authoritative;
  the 9P `size[4]` prefix is stripped on the wire.
- Read-only synthetic file hierarchy: `/dev/pci/...`, `/dev/gpu/...`,
  `/proc/<MboxId>/`, `/svc/<name>`, `/node`.
- Per-spawn namespace assembly: local root + zero or more remote bind-mounts
  driven by `--bind-remote node=<NodeId> at=<path>` from plan 04.
- FUSE bridge (`fuser`/fuse3 ABI) per spawned task.
- Dynamic content for `/dev/gpu/<index>/in_use` — read-time call into the
  `classic-cap` capability broker.

**Out of scope (deferred):**

- **Writes** anywhere. v1 is read-only; `Twrite` -> `EROFS`.
- **Distributed POSIX semantics:** no cross-node `flock`, coherent caches,
  or rename across mounts. Each remote 9P server is a separate root.
- **Caching** of remote 9P responses — every read goes to the wire.
- **9P authentication** (per ARCHITECTURE.md "fully trusted cluster").
  `Tauth` -> `EOPNOTSUPP`.
- **Hardware enumeration** — `classic-fs` consumes `NodeAd`s from
  `classic-ad` (plan 02); it never probes `/sys` itself.
- **Kernel `v9fs` mount.** v1 uses FUSE; `v9fs` is a possible v2 (needs
  `CAP_SYS_ADMIN`).
- **Extended attributes** and **POSIX locking** ops -> `EOPNOTSUPP`.
- Per-user namespace isolation between cluster users — single-tenant in v1.

## Reasoning

Classic borrows two Plan 9 ideas: **hardware-as-files** and **per-process
namespaces**. Together they let a placement decision express "give this
task GPU 0 from node B, mounted as the local `/dev/gpu/0`" without the
task knowing the device is on a different host. The file-system *is* the
cluster-wide naming layer; without it every consumer (spawn pipeline, CLI,
tooling) would invent its own scheme for addressing remote+local hardware.

**Why 9P2000.L:** designed for synthetic hierarchies and namespace ops;
`.L` has the right error model (`Rlerror{ecode}` with Linux errnos) and
the ops Linux clients actually use (`Tgetattr`, `Treaddir`). Wire format
is small (~14 essential ops for v1) and well-specified. We reuse the
plan-01 transport by wrapping 9P inside Classic frames.

**Alternatives rejected:**
1. **Custom RPC over Classic frames** — reinvents 9P, no off-the-shelf
   clients (`9pfuse`, kernel `v9fs`, `diod`), still need FUSE.
2. **NFSv4 / Ganesha** — heavyweight, kernel-mount oriented, drags in a
   UID/GID auth model we don't want.
3. **Kernel `v9fs` directly (no FUSE)** — needs `CAP_SYS_ADMIN` in the
   task's user ns; more setup risk for v1. Listed as a possible v2.

**Success:** a task on node A spawned with `--bind-remote node=B at=/cluster/B`
runs `cat /cluster/B/dev/gpu/0/vram_free_mb` and gets a fresh number from B;
`cat /node` returns A's NodeId in hex; `readlink /svc/placer` returns
`NetId`s. The spawned program needs no changes.

## Design

### Architecture

```
spawned process --syscalls--> FUSE kernel --/dev/fuse--> classic-fs::fuse_bridge
                                                                |
                                                                v
                                              classic-fs::namespace (per-task mounts;
                                                                     longest-prefix dispatch)
                                              |                              |
                                              v                              v
                                  classic-fs::server               classic-fs::client
                                  (in-process loopback;            (outbound 9P over plan-01
                                   reads NodeAd cache,              transport; kinds 0x0400/0x0401)
                                   classic-mbox tables,                      |
                                   classic-cap broker)                       v
                                                                  remote classicd's server
```

One daemon per host. All 9P multiplexes over the existing per-peer TCP
connections from plan 01. The FUSE mount talks 9P to an in-process
loopback (no TCP); remote prefixes go out as `0x04xx` Classic frames.

### 9P op coverage (v1)

Implemented (well-formed `Rlerror` on any failure path; never closes the
connection short of an unparseable frame):

| Op           | Code | Notes                                                   |
|--------------|------|---------------------------------------------------------|
| `Tversion`   | 100  | Negotiates `9P2000.L`; `msize` clamped to 64 KiB.        |
| `Tattach`    | 104  | `afid == NOFID`; `aname == ""` returns root.            |
| `Tflush`     | 108  | No-op + Rflush.                                         |
| `Twalk`      | 110  | Up to 16 names per call (spec max).                     |
| `Tlopen`     | 12   | Read modes only; write flags -> `EROFS`.                |
| `Treadlink`  | 22   | `/svc/<name>` is a symlink whose target is `NetId`s.    |
| `Tgetattr`   | 24   | Synthetic stat (mode, size, mtime; uid=gid=0).          |
| `Treaddir`   | 40   | Variable-length entries, lexicographic order.           |
| `Tfsync`     | 50   | No-op + Rfsync.                                         |
| `Tread`      | 116  | Synthetic content; always uncached.                     |
| `Tclunk`     | 120  | Releases a fid.                                         |

Rejected with `Rlerror{EROFS}` (read-only tree): `Tlcreate`(14),
`Tsymlink`(16), `Tmknod`(18), `Trename`(20), `Tsetattr`(26), `Tlink`(70),
`Tmkdir`(72), `Trenameat`(74), `Tunlinkat`(76), `Twrite`(118),
`Tremove`(122).

Rejected with `Rlerror{EOPNOTSUPP}`: `Tauth`(102), `Txattrwalk`(30),
`Txattrcreate`(32), `Tlock`(52), `Tgetlock`(54). Plus the legacy
non-`.L` ops `Tstat`(124) / `Twstat`(126) — we negotiated `.L`.

### File hierarchy (with example output)

```
/                           dr-xr-xr-x
├── node                    -r--r--r--   local NodeId hex (32 chars + \n)
├── dev/
│   ├── pci/
│   │   └── <vendor>:<device>/
│   │       └── <bus:device.function>/
│   │           ├── numa_node, iommu_group, class, vendor, device
│   └── gpu/
│       └── <index>/
│           ├── vendor, device
│           ├── vram_total_mb           (from NodeAd)
│           ├── vram_free_mb            (read-time, from NodeAd ad cache)
│           ├── nvlink_peers            (comma-separated local indices)
│           └── in_use                  (read-time, from classic-cap broker)
├── proc/<MboxId>/
│   ├── cmdline                          argv joined by \0
│   ├── status                           "running\n" | "exited code=N\n"
│   └── caps                             human-readable capability list
└── svc/<name>                           symlink target = "node=<hex> mbox=<u64>" (multi-line for N instances)
```

Example session via `9pfuse` mounted at `/mnt/classic`:

```
$ ls /mnt/classic                              # dev  node  proc  svc
$ cat /mnt/classic/node                        # 9d0f4b1e7f6b4a4f8c1d2e3f4a5b6c7d
$ ls /mnt/classic/dev/gpu                      # 0 1 2 3 4 5 6 7
$ cat /mnt/classic/dev/gpu/0/vram_free_mb      # 78231
$ cat /mnt/classic/dev/gpu/0/in_use            # 0
$ readlink /mnt/classic/svc/placer             # node=9d0f...c7d mbox=3
```

Modes: directories `0o555`, regular files `0o444`, symlinks `0o777`.
UID/GID always `0/0`. `mtime` is daemon start for static files, `now()` for
dynamic ones. **`/svc/<name>` is a symlink** so Linux callers get a
one-syscall `readlink(2)` (matches the Plan 9 idiom).

### Wire format

No new payload types. Two frame kinds: `NineReq = 0x0400` (raw 9P
T-message bytes minus the leading `size[4]`) and `NineRsp = 0x0401` (the
R-message equivalent). Outer Classic length is authoritative. 9P `msize`
clamps to 64 KiB so one 9P message always fits in one Classic frame — no
fragmentation in v1. `0x0402–0x04FF` reserved for future fs-layer frames;
unused in v1.

### Data shapes

```rust
// crates/classic-fs/src/lib.rs
use classic_proto::{NetId, NodeId, MboxId};

pub enum Session {
    Local(LocalServer),
    Remote(RemoteClient),
}

pub struct Mount {
    pub at: PathBuf,            // "/" or "/cluster/<label>"
    pub session: Session,
    pub remote_root: String,    // 9P aname; "" for our root
}

pub struct Namespace {
    pub task: MboxId,
    pub mounts: Vec<Mount>,     // sorted longest-prefix-first
    pub fuse_mountpoint: PathBuf, // /run/classic/ns/<MboxId>
}

/// Snapshot read by the local server on every Tread/Treaddir. Filled by
/// other crates; classic-fs is a consumer.
pub struct NodeView<'a> {
    pub node_id: NodeId,
    pub ad: &'a classic_ad::NodeAd,
    pub services: &'a classic_mbox::ServiceTable,
    pub procs: &'a classic_mbox::ProcTable,
    pub caps: &'a classic_cap::Broker, // for /dev/gpu/*/in_use
}
```

### Interfaces

```rust
// classic-fs::server
impl LocalServer {
    pub fn new(view_provider: Arc<dyn Fn() -> NodeView<'static>>) -> Self;
    /// Drive one inbound 9P frame; produce zero or one outbound frame.
    pub fn handle(&self, kind: FrameKind, body: &[u8]) -> Option<(FrameKind, Vec<u8>)>;
}

// classic-fs::client (one method per implemented op)
impl RemoteClient {
    pub async fn attach(&self, aname: &str) -> io::Result<Fid>;
    pub async fn walk(&self, fid: Fid, names: &[&str]) -> io::Result<Fid>;
    pub async fn open(&self, fid: Fid, flags: u32) -> io::Result<()>;
    pub async fn read(&self, fid: Fid, off: u64, count: u32) -> io::Result<Vec<u8>>;
    pub async fn readdir(&self, fid: Fid, off: u64, count: u32) -> io::Result<Vec<DirEntry>>;
    pub async fn getattr(&self, fid: Fid) -> io::Result<Stat>;
    pub async fn readlink(&self, fid: Fid) -> io::Result<String>;
    pub async fn clunk(&self, fid: Fid) -> io::Result<()>;
}

// classic-fs::namespace
impl Namespace {
    pub fn build_for_spawn(spec: &SpawnSpec, local: &LocalServer, peers: &PeerSet) -> Self;
    pub fn mount_fuse(&self) -> io::Result<FuseHandle>;
    pub fn unmount(handle: FuseHandle);
}
```

### Per-spawn namespace assembly (algorithm)

Inputs: `SpawnSpec` (plan 04) with `bind_remote: Vec<(NodeId, PathBuf)>`,
the freshly allocated `MboxId`, a `PeerSet` for resolving NodeIds.

```text
fn assemble(spec, mbox, peers, local) -> Namespace:
    mounts = [Mount{ at: "/", session: Local(local.clone_handle()), remote_root: "" }]

    for (node_id, at_path) in spec.bind_remote:
        if node_id == local.node_id:           return Err("self bind")
        if !at_path.is_absolute() or at_path == "/": return Err("'at' must be absolute non-root")
        if mounts.any(|m| m.at == at_path):    return Err("duplicate mount point")
        client = peers.fs_client_for(node_id)? # opens transport on demand
        client.attach(aname="").await?         # validate the remote is alive
        mounts.push(Mount{ at: at_path, session: Remote(client), remote_root: "" })

    mounts.sort_by_key(|m| -(m.at.components().count() as i32))   # longest prefix first

    fuse_mp = format!("/run/classic/ns/{}", mbox.0)
    fs::create_dir_all(&fuse_mp)?
    return Namespace { task: mbox, mounts, fuse_mountpoint: fuse_mp }
```

Dispatch for path `p`: scan `mounts` in order, find the first whose `at`
is a prefix of `p`, strip the prefix, forward the residual to that mount.

### FUSE mount lifecycle

Mount point: `/run/classic/ns/<MboxId>` (tmpfs on systemd hosts; falls
back to `/var/run/classic/ns/<MboxId>`).

```text
on spawn (from plan-04 pipeline):
    ns = Namespace::build_for_spawn(spec, mbox, peers)
    handle = ns.mount_fuse()?       # forks fuser worker; returns once mounted
    # pipeline then sets child's CWD/chroot relative to ns.fuse_mountpoint

on child exit (plan-04 receives ChildExit):
    Namespace::unmount(handle)      # umount2(MNT_DETACH) + rmdir
```

The FUSE worker is a tokio task in `classicd`. On daemon startup, `classicd`
walks `/run/classic/ns/` and force-unmounts leftovers from a previous crash
(`umount2(MNT_FORCE | MNT_DETACH)`). Safe because v1 has no live tasks
across daemon restarts (placement-at-fork, no migration). Mount options:
`default_permissions=no, allow_other=no, ro=yes, fsname=classic,
subtype=classic` — only the daemon and the spawning task (same UID) read it.

### Dynamic content: `/dev/gpu/<i>/in_use`

The only path whose freshness matters per-read. Every `Tread` triggers a
synchronous broker call:

```rust
fn read_in_use(view: &NodeView, gpu_index: u32) -> Vec<u8> {
    match view.caps.lookup_gpu(gpu_index) {
        None         => b"0\n".to_vec(),
        Some(holder) => format!("1 mbox={}\n", holder.mbox.0).into_bytes(),
    }
}
```

`Broker::lookup_gpu(u32) -> Option<CapHolder { mbox: MboxId, since: Instant }>`
is the contract from `classic-cap` (plan 04 fills the broker; plan 06
consumes the read-only side). Other dynamic files (`vram_free_mb`, `status`)
read from the cached `NodeAd` rather than calling NVML directly.

### File / crate layout

```
crates/
  classic-fs/                     # NEW crate
                                  # deps: classic-proto, classic-mbox,
                                  #       classic-ad (read-only), classic-cap (read-only),
                                  #       fuser, bytes, thiserror, tokio
    src/lib.rs                    # NEW
    src/proto/{mod,codec,types}.rs # NEW — 9P2000.L messages, codec, Fid/Tag/Qid/DirEntry/Stat
    src/server/{mod,view,tree,ops}.rs # NEW — LocalServer, NodeView, synthetic tree, op handlers
    src/client/{mod,fid_pool}.rs  # NEW — RemoteClient over classicd transport, fid allocator
    src/namespace/mod.rs          # NEW — Mount, Namespace::build_for_spawn, dispatch
    src/fuse/{mod,translate}.rs   # NEW — fuser Filesystem impl + FUSE -> 9P translation
    src/errno.rs                  # NEW — Rlerror code constants
  classic-node/src/{lib,transport}.rs  # MODIFIED — register kinds 0x0400/0x0401, route 0x04xx
  classic-spawn/src/lib.rs        # MODIFIED — wire SpawnSpec.bind_remote -> mount_fuse
  classic-cli/src/args.rs         # MODIFIED — add --bind-remote node=<hex> at=<path>
plans/06-9p-namespace-server.md   # this doc
```

## Requirements

### Functional

- [ ] FR-1: 9P2000.L server implements `Tversion`, `Tattach`, `Twalk`,
  `Tlopen`, `Tread`, `Treaddir`, `Tgetattr`, `Treadlink`, `Tflush`,
  `Tclunk`, `Tfsync` per spec, with `9P2000.L` negotiated.
- [ ] FR-2: All v1-rejected ops return a well-formed `Rlerror` with the
  codes from the op coverage table.
- [ ] FR-3: Synthetic hierarchy contains `/node`, `/dev/pci/...`,
  `/dev/gpu/...`, `/proc/...`, `/svc/...` populated from `NodeView`. Static
  fields are stable; dynamic ones (`vram_free_mb`, `in_use`, `status`)
  re-evaluate per read.
- [ ] FR-4: `classic spawn --bind-remote node=<hex> at=/cluster/<label>`
  causes the spawned task to see that node's hierarchy at the prefix.
  Multiple `--bind-remote` flags are allowed.
- [ ] FR-5: Namespace delivered through a FUSE mount at
  `/run/classic/ns/<MboxId>`; mount is removed on task exit; stale mounts
  reaped on next daemon start.
- [ ] FR-6: Reading `/dev/gpu/<i>/in_use` consults `classic-cap::Broker`;
  output reflects broker state at read time.
- [ ] FR-7: Frame kinds emitted only from `0x0400`/`0x0401`; never poaches
  other ranges.

### Non-functional

- **Performance:** local `Tread` of 4 KiB ≤ 200 µs median; remote `Tread`
  over loopback TCP ≤ 1 ms median; FUSE adds ≤ 50 µs per syscall. Budget
  targets, not blocking for v1 sign-off.
- **Compatibility:** Linux 6.1+, x86_64 primary, aarch64 must compile but
  not a v1 test target. fuse3 module loaded; `/dev/fuse` accessible.
- **Security:** unauthenticated; v1 trusts every Classic peer. FUSE mount
  is `allow_other=no`. GPU-field accuracy depends on NVML via `classic-ad`.

## Testing plan

### Unit (`crates/classic-fs`)

- `proto::codec`: round-trip every implemented 9P message type
  (`decode(encode(msg)) == msg` property test).
- `server::tree`: golden test against a frozen `NodeView` fixture; verify
  `Treaddir` order and `qid.type` bits.
- `server::ops`: per-op tests via an in-process driver. Cover Tversion
  msize fallback, Twalk with 5+ names, Tread offset past EOF, Tclunk
  releasing a fid.
- `errno`: every rejected op returns the documented code.
- `namespace::dispatch`: longest-prefix matching with three mounts
  (`/`, `/cluster/B`, `/cluster/B/dev`); reject relative paths and root
  mounts.

### Integration

- **`9pfuse` interop** (`tests/interop_9pfuse.rs`): pipes 9P bytes between
  `9pfuse` and the in-process server over a Unix socket; asserts `ls`,
  `cat`, `readlink` outputs. Skipped if `which 9pfuse` fails.
- **Rust-native 9P client** (`tests/native_client.rs`): drives every
  implemented op via `classic-fs::client` against the local server. The
  authoritative regression test — no system packages.
- **FUSE smoke** (`tests/fuse_smoke.rs`): mounts the in-process server
  through FUSE at a tempdir; runs `ls`, `cat`, `stat`, `readlink`. Gated
  by `CLASSIC_FUSE_TESTS=1`.
- **Two-node bind-remote** (`crates/classic-node/tests/fs_bind_remote.rs`):
  two `classicd` instances in one process (in-memory transport, distinct
  NodeIds); spawn a fake task on A with `--bind-remote node=<B> at=/cluster/B`;
  assert the FUSE mount shows B's `/dev/gpu/0/vram_free_mb`.

### End-to-end (manual smoke)

```
# on host A and B
classicd --config conf-{a,b}.toml &
# from A:
classic spawn --bind-remote node=<B-hex> at=/cluster/B -- \
    /bin/sh -c 'ls /cluster/B/dev/gpu && cat /cluster/B/dev/gpu/0/vram_free_mb'
```

Expected: shell lists B's GPU directory and prints a numeric `vram_free_mb`.

### Hardware-dependent

None for the FS layer itself. Gated and skipped (not failed) when absent:
`fuse_smoke.rs` needs `/dev/fuse`; `interop_9pfuse.rs` needs `9pfuse` on `PATH`.

## Acceptance criteria

- [ ] AC-1: `classic-fs` compiles; unit tests pass; `cargo clippy
  --all-targets` clean.
- [ ] AC-2: `9pfuse` mounts the local server and `ls /mnt/classic` shows
  `dev`, `node`, `proc`, `svc`.
- [ ] AC-3: Native-client test passes every implemented 9P op and every
  rejected op returns the documented errno.
- [ ] AC-4: FUSE smoke test passes with `CLASSIC_FUSE_TESTS=1` on Linux
  6.1+ x86_64.
- [ ] AC-5: `classic spawn --bind-remote node=<hex> at=/cluster/B -- ls
  /cluster/B/dev/gpu` succeeds end-to-end on a two-node deployment.
- [ ] AC-6: `cat /dev/gpu/0/in_use` reflects a capability acquired via
  `classic-cap` within 1 read of the acquisition (no stale-cache window).
- [ ] AC-7: Frame-range audit (`grep -n 0x04` across the workspace) shows
  only `classic-fs` emits `0x04xx`, and only `0x0400`/`0x0401` are used.
- [ ] AC-8: On daemon shutdown, all per-task FUSE mounts unmount cleanly;
  on next start, stale mounts from a crashed prior daemon are reaped.

## Open questions

1. **Symlink vs regular file for `/svc/<name>`.** v1 picks symlink for
   `readlink(2)` ergonomics. Revisit after the plan-08 demo if needed.
2. **Multi-instance services.** v1 uses a multi-line symlink target. Could
   split into `/svc/foo/0`, `/svc/foo/1`, ... if CLI ergonomics complain.
3. **`msize` ceiling.** 64 KiB suffices for synthetic content; future
   features (log streaming) may bump it.
4. **Kernel `v9fs` as alternate client.** Deferred to post-v1; would
   remove the FUSE hop at the cost of `CAP_SYS_ADMIN`.
5. **Remote node disappears mid-task.** v1: reads return `EIO` once the
   transport notices the peer is gone; no auto-remount. Document in CLI.
6. **Per-task `/proc` filtering.** v1 exposes every local task; future
   work may restrict per reader (v1 has no inter-task isolation anyway).

## References

- 9P2000.L spec (diod): https://github.com/chaos/diod/blob/master/protocol.md
- Linux kernel 9p: `Documentation/filesystems/9p.rst`
- `plans/ARCHITECTURE.md` — frame ranges, identity types, transport
- plan 01 — `Frame`, `Connection`, frame-kind dispatch
- plan 02 — `NodeAd` GPU/PCI fields
- plan 04 — `SpawnSpec`, capability broker contract
- plan 05 — `ServiceTable`, mbox-keyed proc table
- `fuser` crate: https://github.com/cberner/fuser
- `9pfuse` (diod): https://github.com/chaos/diod
- Pike et al. 1995, "Plan 9 from Bell Labs"
