# Feature: Transparent Network Filesystem (`/cluster/fs`)

> **Status:** future
> **Epic bead:** (none — future-work doc; no beads filed)
> **Owner:** unassigned
> **Last updated:** 2026-05-07

This is the v2 evolution of `plans/06-9p-namespace-server.md`. v1 is a
read-only synthetic 9P tree (hardware-as-files, service directory,
per-task `/proc`). v2 is a real shared filesystem that user data can
live on, mounted cluster-wide at a stable path.

## Scope

**In scope:**

- A cluster-wide unified mount point — proposal: `/cluster/fs` —
  appearing with the same contents on every node running `classicd`.
- Per-node local backing store (NVMe / SSD); capacity is the union,
  modulo replication factor.
- Read/write replication of regular files and directories with a
  defined consistency model (close-to-open + writer leases — see
  Design).
- Basic POSIX semantics applications actually depend on:
  `open`/`creat`, `read`, `write`, `pread`/`pwrite`,
  `fsync`/`fdatasync`, `unlink`, `rename` within a volume,
  `mkdir`/`rmdir`, `stat`/`mtime`/`ctime`, advisory locks (`flock`,
  `fcntl(F_SETLK)`), `O_APPEND` (with caveats), `O_TRUNC`.
- Failure handling: a single-node outage degrades availability only
  on shards the lost node owned; surviving shards stay read-write.
- Coexistence with v1's 9P namespace: `/cluster/fs` is one more
  `Mount` in the per-task `Namespace` from plan 06; the synthetic
  tree (`/dev/...`, `/svc/...`, `/proc/...`) is unchanged.
- Migration story for open file handles if process migration lands
  (see `plans/future/F1-process-migration.md`).

**Out of scope:**

- **Full POSIX.** Deviations documented: `O_APPEND` not atomic during
  partition; `mmap`-based concurrent writes not coherent; `rename`
  across volumes not atomic.
- **ACID transactions across files** — no two-phase commit at the FS
  layer. Apps that need this stay on a real database.
- **Archival / cold tiers, S3 backing, cross-rack erasure coding.**
- **Encrypted-at-rest** in the FS layer. Defer to LUKS under the
  backing store, or revisit in v3.
- **Multi-tenant quotas / ACLs** beyond unix mode bits.
  ARCHITECTURE.md pins us to a fully trusted cluster.
- **Clients outside the cluster.** Only `classicd` peers and tasks
  they spawn see `/cluster/fs`.
- **NFS / SMB re-export gateway.** Possible v3.

## Reasoning

v1 deliberately shipped a read-only synthetic FS because the hard part
of a distributed filesystem is not the wire format — it is **cache
coherence, locking, write replication, and behavior under partition**.
Getting those wrong corrupts user data.

We want `/cluster/fs` because Classic's defining feature is "a process
declares hardware needs, the cluster finds a node, the process runs
there." If the user's input/output data isn't reachable from the chosen
node, we have not built an SSI; we have built a smarter `ssh`. Plan 06
gave us a unified namespace; v2 makes it writable.

Lessons we are drawing on:

- **NFSv4 close-to-open consistency.** Loose but workable. We adopt
  CTO as *baseline* and offer stronger semantics only via opt-in
  (e.g. `O_DIRECT`-mapped flag, or explicit `fcntl(F_SETLK)`).
- **AFS volumes + tokens / callbacks.** Surgical invalidation when
  another writer commits. We borrow the callback idea and the
  per-file write-lease idea.
- **CephFS dynamic subtree partitioning.** Adaptive metadata sharding.
  Not needed at small scale, but the architecture must not preclude it.
- **GlusterFS translator stack.** Replication / distribution / caching
  as composable modules. Worth borrowing internally.
- **Coda optimistic replication.** Excellent for disconnected
  operation, costs application-visible conflicts. We do not expect
  disconnected operation in v2 (same-DC peers), so default to
  pessimistic (writer-lease) replication.
- **JuiceFS / SeaweedFS.** Split metadata (transactional KV) from
  data (object store). Tempting for strong metadata consistency.

**Why not just adopt CephFS or JuiceFS verbatim?** Probably we can. But:

1. We already operate a 9P namespace per task. The cluster mount
   should plug into that namespace as one more `Mount`, not replace
   it. A thin layer over 9P keeps the integration trivial.
2. The service directory and the FS share a naming substrate. If
   `/cluster/fs/...` is reachable through the same `Tattach` /
   `Twalk` primitives as `/dev/...` and `/svc/...`, we get one mount
   rather than two.
3. Re-exporting an external FS through 9P to a FUSE bridge is two
   extra hops in the data path. Latency cost.

Approach A below mounts an existing distributed FS and re-exports —
reliability for free, paying the hop. Approach B builds a native
backend and pays engineering cost for integration leverage. Pick after
a prototype bake-off.

**Success in plain English:** a developer runs
`classic spawn -- python train.py --data /cluster/fs/datasets/imagenet`,
the placement engine picks node C (with the right GPU), and
`train.py` reads its data exactly the way it would on a laptop.
Crash a node mid-training; the job sees a stall on its shards but
does not corrupt or fail.

## Design

Two approaches; both must solve the cross-cutting concerns at the end.

### Approach A: thin layer over an existing distributed backend

Each node mounts a backend (CephFS, JuiceFS, MooseFS, GlusterFS, or
BeeGFS) at a hidden path, e.g. `/var/lib/classicd/fs/`. `classic-fs`
adds a `Session` variant alongside `Local` and `Remote`:

```rust
pub enum Session {
    Local(LocalServer),     // synthetic tree (v1)
    Remote(RemoteClient),   // remote 9P peer (v1)
    Backend(BackendMount),  // NEW v2: passthrough to mounted FS
}
```

`BackendMount` implements the op surface (`attach`, `walk`, `open`,
`read`, `write`, `getattr`, `readdir`, `lock`, `fsync`, `setattr`,
`create`, `remove`, `rename`) by translating 9P ops to syscalls
against the mounted backend.

```
spawn'd task --syscalls--> FUSE --> classic-fs::namespace
                                          |
                                          v  (Session::Backend)
                                  syscalls into
                                  /var/lib/classicd/fs/...
                                          v
                                  backend client (ceph/juicefs/...)
                                          v
                                  remote storage nodes
```

**Pros:** reliability/durability from a battle-tested codebase (no
Raft of our own); ship in months not years; backend is swappable.

**Cons:** two FS layers in the data path; cache coherence inherited
from backend (cannot tighten); locking semantics inherited; we
re-export but cannot blend with service directory or per-task
namespace.

### Approach B: native classic-fs distributed backend

Each node owns shards. Rendezvous-hash the parent directory's inode
to a primary, replicate to N-1 secondaries (default N=3). Reads from
any replica via quorum; writes through the primary, which appends to a
per-shard log replicated by Raft (or a simpler primary-replica
protocol with leases, à la Chain Replication).

**Per-file write affinity.** First writer to open a file gets a
**write lease** (AFS-style token). The lease lets the writer cache
writes locally and skip per-op Raft round-trips; revoked when another
node opens for writing or when it expires (default 30 s, refreshed on
activity). Readers without a lease do CTO: cache pages while open,
drop on close.

**Storage per shard.** Local-disk chunk store of 4 MiB
content-addressed chunks under
`/var/lib/classicd/fs/chunks/<shard>/<hash[0..2]>/<hash>`; a metadata
KV (sled or RocksDB) per shard for inode → chunk list, parent-inode →
directory entries, locks, leases; a Raft-replicated shard log for
metadata mutations and chunk-pointer commits. Chunks are durably
written *before* the log entry referencing them — so log replay never
points at a missing chunk.

**Pros:** single FS layer in the data path; we own the consistency
model end-to-end; tight service-directory integration; shard
placement can be GPU-affinity-aware.

**Cons:** massive scope (Raft, compaction, snapshots, repair, scrub,
rebalance, lease recovery, fencing, anti-entropy); we will discover
bugs in production that Ceph/Gluster fixed in 2014; we own all of ops
(backup, restore, capacity planning).

### Recommendation

Prototype both. **Approach A is the realistic shipping target for v2.**
**Approach B is v3-ish ambition** — once we have operational experience
and know what semantics the workload really demands. The interfaces
in `classic-fs` are designed so swapping `Session::Backend` for
`Session::Cluster` is a small change.

### Cross-cutting concerns (both approaches must solve)

**Cache coherence.** Default: close-to-open. On `open`, fetch fresh
inode + mtime; on `read`, populate a per-task page cache; on `close`
of a writer, flush and invalidate other open handles' caches.
Stronger semantics gated by an opt-in flag (probably a bit on
`Tlopen`, mapped from `O_DIRECT` / `O_SYNC`).

**Locking.** Advisory POSIX locks (`fcntl(F_SETLK)`, `flock`) handled
by a per-volume lock manager colocated with the metadata primary.
Lock state is persisted (in the metadata KV under approach B; in an
auxiliary etcd-style KV under approach A, since the backend's lock
semantics may not survive classicd restarts usefully). Locks are
per-`MboxId`, not per-OS-pid: a crashed task has its locks released
by its local classicd noticing the FUSE handle close.

**File-handle stability.** v1 9P `Fid`s are valid only for the life of
one transport connection. v2 needs *durable* file identifiers so a
re-established connection (transient peer flap) can reattach without
reopening. Proposal: `HandleCookie = (volume, inode, generation)`;
client `Treopen`s by presenting the cookie. Shard-agnostic — the file
may have migrated to a different shard.

**Migration interaction.** If `plans/future/F1-process-migration.md`
ships, an open handle on `/cluster/fs/...` is just a `HandleCookie`.
Migration serializes the cookie; the new node's classicd reattaches
and FUSE re-establishes the kernel fd. Files outside `/cluster/fs`
(local disk, `/tmp`) are NOT migratable — documented as a constraint
of migration.

**Frame allocation.** 9P frames use `0x0400`/`0x0401`. v2 needs
inter-node FS-control frames (lease grant/revoke, lock manager,
shard repair, replica protocol). Reserve `0x0410–0x041F`, internal to
`classic-fs`'s assigned `0x0400–0x04FF` range.

**Mount point.** `/cluster/fs` is one more `Mount` in the per-task
`Namespace`. Default classicd config injects it into every spawn
unless `--no-cluster-fs` is passed.

### Data shapes (sketch)

```rust
// crates/classic-fs/src/v2/mod.rs

/// Stable handle surviving transport reconnects and (later) migration.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Serialize, Deserialize)]
pub struct HandleCookie { pub volume: u64, pub inode: u64, pub generation: u32 }

pub enum Session {
    Local(LocalServer),
    Remote(RemoteClient),
    Backend(BackendMount),  // approach A
    Cluster(ClusterClient), // approach B (reserved)
}

pub struct LeaseToken {
    pub cookie: HandleCookie, pub kind: LeaseKind,   // Read | Write
    pub expires: Instant,     pub issued_by: NodeId,
}

pub enum FsControlFrame {  // wrapped in 0x0410..=0x041F
    LeaseRequest { cookie: HandleCookie, kind: LeaseKind },
    LeaseGrant   { token: LeaseToken },
    LeaseRevoke  { cookie: HandleCookie, reason: RevokeReason },
    LockAcquire  { cookie: HandleCookie, range: LockRange, kind: LockKind },
    LockRelease  { cookie: HandleCookie, range: LockRange },
    ShardRepair  { /* per-shard anti-entropy */ },
}
```

### File / crate layout

```
crates/classic-fs/src/v2/
  mod.rs           # NEW — Session::Backend | ::Cluster, lease types
  backend.rs       # NEW — BackendMount (approach A)
  cluster/         # NEW — approach B (skeleton in v2; full impl v3)
    {mod,shard,chunk_store,metadata_kv,replica}.rs
  lease.rs         # NEW — lease manager, callbacks
  lock.rs          # NEW — POSIX advisory lock manager
  cache.rs         # NEW — per-task page cache, CTO invalidation
  handle.rs        # NEW — HandleCookie, Treopen
crates/classic-fs/src/proto/v2_ops.rs   # NEW — write-side 9P ops
crates/classic-spawn/src/lib.rs         # MODIFIED — inject /cluster/fs
crates/classic-cli/src/args.rs          # MODIFIED — --no-cluster-fs, --cluster-fs-volume
plans/future/F2-transparent-network-fs.md   # this doc
```

## Requirements

### Functional

- [ ] FR-1: A regular file created at `/cluster/fs/<volume>/<path>` on
  node A is visible to a reader on node B with content matching CTO
  semantics (reader that `open`s after the writer's `close` sees the
  bytes).
- [ ] FR-2: Advisory locks (`flock`, `fcntl(F_SETLK)`) work across
  nodes: a holder of `LOCK_EX` blocks an `LOCK_EX` from another node
  until release or holder death.
- [ ] FR-3: `fsync(2)` returns success only after a write is durably
  committed to the configured replication factor (default 2-of-3).
- [ ] FR-4: A single-node loss does not lose committed data and does
  not block reads or writes against shards owned by surviving nodes.
- [ ] FR-5: `rename(2)` within a single volume is atomic with respect
  to concurrent readers / writers (POSIX-conformant rename).
- [ ] FR-6: `mtime` updated on `write`, `ctime` on metadata change;
  both readable from any node.
- [ ] FR-7: Default-injected mount of `/cluster/fs` is present in
  every spawned task's namespace unless `--no-cluster-fs` was given.

### Non-functional

- **Performance:**
  - Local-cached read latency: < 100 µs (page-cache hit).
  - Uncached read latency: < 10 ms on a same-DC peer over 25 GbE.
  - Small-write throughput: > 100 MiB/s sustained per node from a
    single writer (4 KiB writes + periodic fsync, replication 3).
  - Sequential-write throughput: > 1 GiB/s per node, replication 3,
    on NVMe-class local storage.
  - Metadata op latency: `open`, `stat`, `readdir(small)` < 5 ms.
- **Compatibility:** Linux 6.1+, x86_64, FUSE3 client. Optionally
  kernel `v9fs` (open question).
- **Security:** Inherits trusted-cluster assumption from
  ARCHITECTURE.md. UID/GID stored verbatim in inode metadata; mode
  bits enforced. No ACLs in v2.
- **Hardware:** Storage-providing nodes need durable local storage
  (NVMe / SSD). Diskless compute-only nodes are fine (just don't host
  shards).

## Testing plan

### Unit

- `lease`: grant / revoke / expiry under simulated clock; race where
  two nodes request a write lease for the same file simultaneously.
- `lock`: POSIX semantics under contention; cross-node deadlock
  detection; lock recovery on holder crash.
- `cache`: CTO invalidation correctness — writer closes, all readers
  with stale pages are invalidated before their next read returns.
- `handle`: `HandleCookie` round-trip; cookie survives a generation
  bump only when the file is genuinely the same file.

### Integration

- **POSIX conformance: pjdfstest** against a FUSE-mounted
  `/cluster/fs/test/`. Pass chmod / chown / open / mkdir / mknod /
  rename / rmdir / symlink / truncate / unlink. Document the
  exclusion list (xattrs, sticky bits, ACLs) as a release artifact.
- **Throughput: fio.** Standard profiles (random read 4k, seq write
  1m, mixed; replication 1/2/3) on a three-node rig. Tracked as a
  perf-regression suite.
- **Concurrent writers.** N writers across M nodes appending to one
  file under `flock`; assert exactly N * write_count lines on close,
  no interleaving within a line.
- **Large directories.** 1 M files in one directory; `readdir`
  completes; metadata KV does not OOM.
- **Cross-node `rename`.** Source and destination dirs on different
  shard primaries; concurrent reader sees old-contents-or-ENOENT or
  new-contents, never a partial state.

### End-to-end

- **Chaos: node kill.** Three-node cluster, replication 3, active
  workload (continuous writes from one node, reads from another).
  `kill -9 classicd` on the third; assert: writes continue (bounded
  stall while primaries reshuffle), reads continue, no committed data
  lost after rejoin.
- **Chaos: network partition.** `iptables` split of a five-node
  cluster. Minority side read-only after lease expiry; majority side
  read-write; on heal, defined reconciliation runs.
- **FS-layer fault injection.** LazyFS-style injector under the chunk
  store; inject `EIO`, partial writes, page reorderings; assert
  correctness.
- **Manual smoke.** Write a file via spawn on node A, read it via
  spawn on node B, see the bytes.

### Hardware-dependent

Throughput numbers are NVMe-dependent. CI without NVMe runs a smaller
fio profile and does not enforce the throughput AC, only correctness.

## Acceptance criteria

Roadmap-level. v2 ships when these are demonstrably true on a
three-node test cluster.

- [ ] AC-1: A process on node A writing to `/cluster/fs/foo`,
  followed by a process on node B reading `/cluster/fs/foo` *after*
  the writer's `close(2)`, observes the writer's bytes (CTO).
- [ ] AC-2: `flock(LOCK_EX)` from node A blocks an `LOCK_EX` from
  node B until release or holder death.
- [ ] AC-3: `fsync(2)` returning success implies durability against
  the loss of any single node.
- [ ] AC-4: With one of three nodes hard-killed mid-workload, the
  remaining two continue serving reads and writes for shards they
  collectively own. (Honest limit: shards whose only surviving
  replica was on the killed node are unavailable until repair.)
- [ ] AC-5: Default-injected `/cluster/fs` mount appears in every
  spawned task's namespace; `--no-cluster-fs` removes it.
- [ ] AC-6: pjdfstest baseline (excluding the documented exclusion
  list of xattr / ACL / sticky-bit tests) passes.
- [ ] AC-7: Performance budgets met on the reference three-node x
  25 GbE x NVMe test rig.
- [ ] AC-8: A documented list of POSIX deviations is published with
  the release. Expected items: `O_APPEND` not atomic during
  partition; `mmap` writes not coherent across nodes; `rename`
  across volumes not atomic.

We are explicit about what v2 does *not* guarantee:

- Strict (linearizable) read-after-write across all nodes without an
  intervening close.
- Atomic multi-file operations.
- Durability through total cluster power loss with zero replication
  delay (an in-flight write in the writer's local cache that has not
  reached a remote replica may be lost — apps that care use `fsync`).

## Open questions

1. **Replication factor: per-directory or per-volume?** Want both
   (volume default, overridable per-subtree, like CephFS pools).
   Syntax: special `.classic-fs/replication` file, xattr, or admin
   RPC?
2. **Conflict resolution if we go optimistic.** v2 stays pessimistic
   (writer leases, single primary). For disconnected operation later,
   Coda manual reconcile vs. CRDT-like (only viable for some file
   types)?
3. **FUSE vs. also kernel `v9fs`.** `v9fs` is faster but needs
   `CAP_SYS_ADMIN` and pins us to the 9P op set. v2 leans FUSE-only;
   revisit if FUSE overhead dominates.
4. **Encryption at rest.** LUKS under the chunk store (simple, does
   nothing against node-root attacker) or chunk-level encryption with
   a cluster KMS?
5. **Quotas.** Per-user quotas need a "user" we have not committed to
   (ARCHITECTURE.md says no multi-tenant). Per-volume quotas are
   simpler and probably enough.
6. **Snapshot and clone.** Worth designing for from day one (immutable
   chunk store helps), or punt to v3?
7. **Hot/capacity tiering on one node.** v2 says no tiering — one
   storage class per node.
8. **Backup story.** Second cluster as DR target? Periodic snapshot
   ship-out to S3? Punted, but the API needs to allow it.
9. **Backend selection (approach A).** Bake-off CephFS vs. JuiceFS
   vs. MooseFS vs. BeeGFS on Classic's reference rig. Gates v2
   kickoff.
10. **Volume namespace shape.** Flat `/cluster/fs/...` or
    `/cluster/fs/<volume>/...`? Multiple volumes are useful
    (independent replication, quotas) but complicate the namespace.

## References

- `plans/ARCHITECTURE.md` — frame ranges, identity types, transport
- `plans/06-9p-namespace-server.md` — v1 read-only 9P namespace this
  evolves
- `plans/future/F1-process-migration.md` — interaction with file-handle
  migration (sibling future-work doc)
- 9P2000.L spec (diod):
  https://github.com/chaos/diod/blob/master/protocol.md
- Pike et al. 1992, "The Use of Name Spaces in Plan 9"
- NFSv4 — RFC 7530 (close-to-open consistency model)
- Howard et al. 1988, "Scale and Performance in a Distributed File
  System" (AFS — volumes, callbacks, tokens)
- Weil et al. 2006, "Ceph: A Scalable, High-Performance Distributed
  File System"; CephFS architecture:
  https://docs.ceph.com/en/latest/cephfs/
- GlusterFS translator architecture:
  https://docs.gluster.org/en/main/Quick-Start-Guide/Architecture/
- Kistler & Satyanarayanan 1992, "Disconnected Operation in the Coda
  File System"
- JuiceFS architecture:
  https://juicefs.com/docs/community/architecture/
- BeeGFS architecture:
  https://doc.beegfs.io/latest/architecture/overview.html
- pjdfstest (POSIX FS conformance):
  https://github.com/pjd/pjdfstest
- LazyFS (FS fault injector):
  https://github.com/dsrhaslab/lazyfs
- fio: https://fio.readthedocs.io/
- Pike et al. 1990 / 1995, "Plan 9 from Bell Labs" — the 9P origin
