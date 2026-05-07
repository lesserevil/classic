# Feature: Mailboxes + Service Directory

> **Status:** draft
> **Epic bead:** `bd-XXX` (filed after this doc lands)
> **Owner:** unassigned
> **Last updated:** 2026-05-07
> **Plan number:** 05
> **Depends on:** plan 01 (skeleton + transport)
> **Consumed by:** plan 06 (9P namespace), plan 07 (placement groups)

## Scope

### In scope

- A new `classic-mbox` crate providing mailbox primitives (in-process + cross-node), a gossiped Service Directory, the `0x0200–0x02FF` wire frames, and GC of services owned by exiting tasks.
- The public API surface that plan 06 (`/svc/<name>` in 9P) and plan 07 (placement groups using the `coordinator` service pattern) will call into.

### Explicitly out of scope

- **9P namespace integration.** Plan 06 will expose the directory as `/svc/<name>`; the file-tree binding does not belong here.
- **Spawn-pipeline stdio.** Plan 04 does **not** use mailboxes for child stdio in v1; it uses dedicated `0x0300–0x03FF` frames.
- **Backpressure / flow control.** v1 is fire-and-forget. We document the drop-on-send failure mode but do not design a windowing scheme.
- **Persistence.** Mailboxes and service registrations are RAM-only; daemon restart loses them.
- **Authentication / encryption.** v1 cluster is fully trusted per ARCHITECTURE.md.
- **Reliable delivery, retries, ordering across reconnects.** Best-effort, single-attempt; ordered only within one live TCP connection.

## Reasoning

### The problem

Tasks spawned by Classic need (1) **addressed messaging** — sending a byte payload to a `NetId` regardless of node — and (2) **name resolution** — finding a peer by a well-known name (`coordinator`, `metrics-sink`) cluster-wide. These are exactly ChrysaLisp's mailboxes and service directory; Classic inherits the model because it is small, proven, and matches our actor-style runtime.

### Alternatives considered

- **gRPC / Tower services per task.** Heavyweight per-endpoint setup, doesn't fit "millions of cheap mailboxes". Rejected — too much ceremony.
- **NATS or similar message bus sidecar.** Adds an external dependency; doesn't share the per-peer connection plan 01 already maintains; `/svc/<name>` would be a layered fiction. Rejected.
- **Reliable delivery (acks + retries) from the start.** Forces sender-side buffering and idempotency thinking. ChrysaLisp explicitly chose fire-and-forget. Rejected for v1 — applications layer reliability when needed.
- **Centralized service registry (one node owns truth).** SPOF, needs election. Rejected — gossip is simpler and matches plan 02.

### Success in plain English

Task A on node A calls `service_declare("coordinator")`. Within a couple of seconds, task B on node B calls `service_lookup_one("coordinator")`, gets a `NetId` pointing at A, calls `mail_send(net_id, payload)`, and A receives the bytes via `MailboxRecv::recv()`. When A's process is killed, B sees the service disappear without manual cleanup.

## Design

### Architecture

```
classic-node (classicd)
  classic-mbox
    MailboxRegistry  <-->  classic-proto frame mux  <-->  TCP per peer pair
    ServiceDirectory                    ^
    GossipEngine (0x02xx)               |
    TaskGcTracker                       |
       ^         ^                    classic-ad (0x01xx, separate)
    local API  spawn hooks
       ^         ^
   classic-fs  classic-spawn
   (plan 06)   (plan 04)
```

Inside `classic-mbox`:

- `MailboxRegistry` — `MboxId -> tokio::mpsc::Sender<Vec<u8>>`. The receiving half (`MailboxRecv`) holds the corresponding `Receiver`; its `Drop` evicts the entry.
- `ServiceDirectory` — gossiped `HashMap<String, BTreeSet<ServiceEntry>>`.
- `GossipEngine` — tokio task that consumes inbound `0x02xx` frames and forwards outbound deltas. Drives on-connect full sync.
- `TaskGcTracker` — `task_id -> {declared services, owned mboxes}`; on task exit, synthesizes `service_forget` and drops mailbox slots.

#### Sequence — remote send/recv (local case is the same minus the wire)

```
Task X (node A)         conn A<->B              Task Y (node B)
  |                         |                         | new() -> (MboxId=7, recv_y)
  |  service_lookup("coord")=> NetId{B, 7}            |
  |  mail_send(NetId, b"hi")|                         |
  |     -- MailSend frame --|====================>    |
  |                         |   enqueue chan 7        |
  |                         |                         |  recv_y.recv() => b"hi"
```

If conn A↔B is down, the frame is dropped; A may emit a local `MailDeliveryFailure` log line — it does **not** travel as an ack. See "Failure model".

### Mailbox lifecycle

```rust
// alloc — owner gets the receiver; sender side lives only in the registry
let (mbox_id, recv) = Mailbox::new();
let net_id = NetId { node: my_node_id(), mbox: mbox_id };

// publish so others can find us (optional)
let svc = service_declare("coordinator")?;

// send (anyone with our NetId can do this)
mail_send(net_id, b"hello".to_vec()).await; // fire-and-forget

// receive (owner only)
let payload: Vec<u8> = recv.recv().await?;

// drop — registry entry evicted, future sends to this MboxId silently drop
drop(recv);
// service auto-forgotten if `svc` is also dropped (or task GC fires)
drop(svc);
```

- **Allocation:** `MboxId` comes from a per-daemon `AtomicU64` starting at `1`; mbox `0` is reserved (ARCHITECTURE.md). IDs never reuse within a daemon's lifetime; restart resets the counter and stales all prior `NetId`s.
- **Eviction:** `MailboxRecv::drop` removes its registry entry. Subsequent local sends drop silently; remote sends are dropped on arrival with a warn log.
- **Bounded channels:** capacity `MBOX_CAPACITY = 1024`. Full ⇒ message dropped (locally or by the inbound delivery loop). This is the fire-and-forget enforcement point — no sender-side backpressure.

### Service Directory data structure

```rust
type Lamport = u64;

#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct ServiceEntry {
    pub net_id: NetId,
    pub lamport: Lamport,        // monotonic at the publishing node
    pub tombstone: bool,         // true after ServiceForget
    pub last_seen: Instant,      // local-only; for staleness eviction
}

pub struct ServiceDirectory {
    // Sorted set so lookup ordering is deterministic for tests.
    inner: HashMap<String, BTreeSet<ServiceEntry>>,
    // Per-task back-index for GC: which (name, NetId) edges did this task create?
    by_task: HashMap<TaskId, Vec<(String, NetId)>>,
    // Local Lamport counter; advanced on every local declare/forget and on every
    // observed remote (lamport,) per CRDT rules.
    local_clock: AtomicU64,
}
```

Lookup semantics:

- `service_lookup(name)` returns all live (`tombstone == false`) `NetId`s.
- `service_lookup_one(name)` returns one chosen via round-robin per process (a per-name `AtomicUsize` cursor). This is good-enough load-balancing for v1; clients that want stickiness can call `service_lookup` and pick themselves.

#### Lamport ordering rules

Each entry carries the publishing node's Lamport timestamp. The directory is a **last-writer-wins CRDT keyed by `(name, net_id)`**:

1. **Local `service_declare(name)`:** bump `local_clock`; insert/replace `ServiceEntry { net_id, lamport, tombstone: false }`; broadcast `ServiceAd`.
2. **Local `service_forget(name)`:** bump `local_clock`; replace with `tombstone: true` (keep `TOMBSTONE_TTL = 60 s` to suppress late resurrections); broadcast `ServiceForget`.
3. **Inbound `ServiceAd`:** `local_clock = max(local_clock, lamport) + 1`. If existing `lamport >= incoming.lamport`, drop. Else replace.
4. **Inbound `ServiceForget`:** same comparison, result is a tombstone.
5. Tombstones older than `TOMBSTONE_TTL` may be GC'd; a strictly-later ad after GC is accepted.
6. Ties on `lamport`: break by `net_id` (`node` then `mbox`, lexicographic).

This works because the only writes to a given `(name, net_id)` pair come from the node that owns `net_id.node` — there is exactly one writer per key. Two tasks on different nodes registering the same `name` produce two entries that coexist (the directory is a multimap on `name`).

### Gossip

Gossip rides the same per-peer connection plan 01 sets up. `0x0200–0x02FF` frames are demuxed by `classic-proto` and fed into `GossipEngine`.

- **On connect:** the new side sends `ServiceSync` immediately after `Hello`. The peer replies with `ServiceSyncResponse { entries }` containing every non-tombstoned entry it knows. Both sides then switch to delta mode.
- **On local declare/forget:** broadcast a single `ServiceAd` / `ServiceForget` to every connected peer. No batching in v1.
- **On receiving a delta:** apply Lamport rules; do **not** re-broadcast — the originator already flooded all peers. (Assumes full-mesh per ARCHITECTURE.md; non-full-mesh is plan 02's flooding territory.)

Simpler than plan 02 (no anti-entropy timer, no Merkle digest) because the directory is small (hundreds of entries) and reconnect's `ServiceSync` is cheap and repairs any losses.

### Mailbox delivery

- **Local:** `mail_send` checks `net_id.node == self_node_id`. If so, look up `MboxId` and `try_send` on the channel. Full or missing ⇒ drop.
- **Remote:** look up the connection for `net_id.node` (`classic-proto` connection registry). No live connection ⇒ warn-log, drop. Otherwise encode and send a `MailSend` frame.
- **Receiving `MailSend`:** look up `to.mbox` locally. Missing/full ⇒ optionally send `MailDeliveryFailure` back to `from.node` (also best-effort). The sender treats absence of failure as inconclusive — no positive acks.

### Failure model

Fire-and-forget means:

| Scenario                              | Sender sees    | Receiver sees |
|---------------------------------------|----------------|---------------|
| Happy path                            | `Ok(())`       | message       |
| Local channel full                    | drop, log warn | nothing       |
| No live connection to peer            | drop, log warn | nothing       |
| Connection dies mid-send              | drop, log warn | nothing       |
| Receiver mbox closed before delivery  | optional `MailDeliveryFailure` (informational) | nothing |
| Receiver mbox channel full            | optional `MailDeliveryFailure` (informational) | nothing |

Applications that need reliability layer it on top (request/response with timeout + retry by the application) — same as ChrysaLisp.

### Task GC

`classic-spawn` (plan 04) calls into `classic-mbox` on lifecycle events:

```rust
pub fn on_task_start(task_id: TaskId);
pub fn on_task_exit(task_id: TaskId);  // synchronously cleans up
```

`on_task_exit` walks `by_task[task_id]`: emit `service_forget` for each declared service, drop the registry entry for each owned `MboxId`. Tasks may also drop a `ServiceHandle` (RAII) to forget early. Both paths are idempotent.

### Frame definitions (`0x0200–0x02FF`)

All serialized with `bincode` v2, fixed-int, little-endian.

```rust
#[repr(u16)]
pub enum MboxFrameKind {
    MailSend            = 0x0200,
    MailDeliveryFailure = 0x0201,
    ServiceAd           = 0x0210,
    ServiceForget       = 0x0211,
    ServiceSync         = 0x0212,
    ServiceSyncResponse = 0x0213,
    // 0x0214–0x02FF reserved
}

pub struct MailSend { pub from: NetId, pub to: NetId, pub payload: Vec<u8> } // payload <= 8 MiB
pub enum DeliveryFailureReason { UnknownMbox, MboxFull, NodeUnreachable /* local-only */ }
pub struct MailDeliveryFailure { pub to: NetId, pub reason: DeliveryFailureReason }

pub struct ServiceAd     { pub name: String, pub net_id: NetId, pub lamport: u64 } // name <= 256 B UTF-8
pub struct ServiceForget { pub name: String, pub net_id: NetId, pub lamport: u64 }

pub struct ServiceSync;  // empty
pub struct ServiceSyncEntry { pub name: String, pub net_id: NetId, pub lamport: u64, pub tombstone: bool }
pub struct ServiceSyncResponse { pub entries: Vec<ServiceSyncEntry> }
```

### Public API (consumed by plans 04, 06, 07)

```rust
pub struct MailboxRecv { /* Drop evicts registry entry */ }

pub struct Mailbox;
impl Mailbox {
    /// Fresh mailbox; MboxId unique for this daemon's lifetime, never 0.
    pub fn new() -> (MboxId, MailboxRecv);
}

impl MailboxRecv {
    /// Wait for the next message. Cancellation-safe.
    pub async fn recv(&mut self) -> Option<Vec<u8>>;
    pub fn try_recv(&mut self) -> Option<Vec<u8>>;
}

/// Fire-and-forget. `Ok` means handed off (enqueued / written to socket),
/// NOT that the peer received it. Errors only on local validation.
pub async fn mail_send(to: NetId, payload: Vec<u8>) -> Result<(), MailError>;

/// Wait until any mailbox has a pending message; return its index.
pub async fn select_mboxes(mboxes: &mut [&mut MailboxRecv]) -> usize;

pub struct ServiceHandle { /* Drop calls service_forget */ }

/// Register (name, this-task's-NetId); gossip it. Multimap on `name`.
pub fn service_declare(name: &str) -> Result<ServiceHandle, ServiceError>;
pub fn service_forget(name: &str);                  // idempotent
pub fn service_lookup(name: &str) -> Vec<NetId>;    // all live endpoints
pub fn service_lookup_one(name: &str) -> Option<NetId>;  // round-robin
```

### Worked example

Task A on `N_A`, task B on `N_B`, live connection between them.

```rust
// on N_A, task A
let (mbox_a, mut recv_a) = Mailbox::new();
let _svc = service_declare("coordinator")?;       // gossiped to N_B
let msg = recv_a.recv().await.unwrap();           // park

// on N_B, task B (some time later)
let target = service_lookup_one("coordinator").expect("absent");
//   => NetId { node: N_A, mbox: mbox_a }
mail_send(target, b"ping".to_vec()).await?;

// back on N_A: recv_a wakes with b"ping"
```

Wire: `ServiceAd { "coordinator", NetId{N_A, mbox_a}, lamport=1 }` flows N_A→N_B at declare; later `MailSend { from: NetId{N_B, mbox_b}, to: NetId{N_A, mbox_a}, payload: b"ping" }` flows N_B→N_A.

### ChrysaLisp comparison sidebar

| Classic                          | ChrysaLisp                       | Notes                                    |
|----------------------------------|----------------------------------|------------------------------------------|
| `Mailbox::new() -> (MboxId, MailboxRecv)` | `(mail-mbox)` allocate form | ChrysaLisp returns one ID; same idea     |
| `mail_send(net_id, payload)`     | `(mail-send net_id msg)`         | Both fire-and-forget; both drop on unreachable |
| `MailboxRecv::recv()`            | `(mail-read mbox)`               |                                          |
| `select_mboxes(&[..])`           | `(mail-select (list mbox …))`    |                                          |
| `service_declare(name)`          | `(mail-declare 'name :service)`  | Service Manager's `:declare` op          |
| `service_forget(name)`           | `(mail-forget 'name :service)`   | `:forget` op                             |
| `service_lookup(name)`           | `(mail-enquire 'name)` (all)     | `:enquire` op                            |
| `service_lookup_one(name)`       | `(mail-lookup 'name)`            | Single-pick variant                      |
| Lamport-clocked CRDT directory   | Service Manager gossip           | Same intent; we make the clock explicit  |

### File / crate layout

```
crates/classic-mbox/                    # NEW crate
  Cargo.toml
  src/
    lib.rs           # public API re-exports
    mbox.rs          # Mailbox, MailboxRecv, MailboxRegistry
    send.rs          # mail_send, local-vs-remote dispatch
    directory.rs     # ServiceDirectory, Lamport CRDT
    gossip.rs        # GossipEngine; on-connect sync; deltas
    gc.rs            # TaskGcTracker, on_task_exit
    frames.rs        # bincode types for 0x0200–0x02FF
    error.rs         # MailError, ServiceError
  tests/
    local_roundtrip.rs
    directory_crdt.rs
    gc.rs

crates/classic-proto/src/lib.rs         # MOD: register 0x0200–0x02FF FrameKind variants
crates/classic-node/src/main.rs         # MOD: wire MailboxRegistry + GossipEngine into the daemon
```

## Requirements

### Functional

- [ ] FR-1: `Mailbox::new()` returns a fresh `MboxId` (never 0, never reused this daemon-lifetime).
- [ ] FR-2: `mail_send` to a local `NetId` delivers FIFO per sender→receiver pair; to a remote `NetId` writes a `MailSend` frame on the live peer connection (drops if none).
- [ ] FR-3: Dropping `MailboxRecv` evicts the registry entry; subsequent sends drop silently.
- [ ] FR-4: `select_mboxes` wakes on the first ready mailbox and returns its index.
- [ ] FR-5: `service_declare(name)` broadcasts `ServiceAd` to every connected peer; `service_forget` broadcasts `ServiceForget` and lookup stops returning the entry.
- [ ] FR-6: `service_lookup` returns the union of all live `(name, net_id)` entries cluster-wide; `service_lookup_one` round-robins.
- [ ] FR-7: New connections perform a `ServiceSync` / `ServiceSyncResponse` exchange that converges both directories.
- [ ] FR-8: Lamport last-writer-wins on `(name, net_id)` resolves concurrent declares/forgets identically on every node; ties broken by `NetId` ordering.
- [ ] FR-9: On task exit, every service it declared is gossiped as `ServiceForget` and every mailbox it owned is evicted.
- [ ] FR-10: Payloads > `MAX_MAIL_BYTES` (8 MiB) ⇒ `Err(MailError::PayloadTooLarge)`. Names > `MAX_SVC_NAME` (256 B UTF-8) ⇒ `Err(ServiceError::NameTooLong)`.

### Non-functional

- **Performance:** local `mail_send`→`recv` median < 50 µs; loopback remote round-trip median < 500 µs; lookup with ≤ 100 endpoints < 10 µs; `ServiceSync` for 1,000 entries < 100 ms.
- **Compatibility:** Linux 6.x, x86_64 primary; aarch64 must compile.
- **Security:** trusted-cluster assumption (ARCHITECTURE.md) — no auth, no encryption; declare/forget logged at debug.
- **Hardware:** none — Rust + tokio + bincode v2.
- **Resource caps:** per-mbox channel 1024; per-name directory entries uncapped in v1.

## Testing plan

### Unit (within `crates/classic-mbox/`)

- `mbox.rs`: allocator skips 0, monotonic, no reuse; `Drop` evicts; bounded channel drops at capacity (sender still sees `Ok`).
- `directory.rs`: Lamport insert/replace/tombstone; concurrent declare from two simulated nodes resolves by Lamport then `NetId`; `service_lookup` filters tombstones; round-robin fair; tombstone-TTL late-ad acceptance.
- `frames.rs`: bincode round-trip every frame type; oversize `MailSend` rejected.

### Integration (within `crates/classic-mbox/tests/`)

- **`local_roundtrip.rs`:** in-process `MailboxRegistry`; two mailboxes; send/recv; assert ordering and payload integrity.
- **`directory_crdt.rs`:** three simulated "nodes" sharing an in-memory bus; overlapping declares; assert convergence within N gossip rounds.
- **`gc.rs`:** task declares 2 services + owns 3 mailboxes; `on_task_exit`; assert directory purged, registry slots gone, `ServiceForget` frames emitted.

### End-to-end (two `classicd` processes on loopback)

```bash
cargo build -p classic-node -p classic-cli
./target/debug/classicd --listen 127.0.0.1:7001 --peer 127.0.0.1:7002 &
./target/debug/classicd --listen 127.0.0.1:7002 --peer 127.0.0.1:7001 &
# (drivers below use a tiny test-only RPC into each daemon to call the mbox API)
./target/debug/mbox-e2e declare 7001 coordinator
./target/debug/mbox-e2e wait-svc 7002 coordinator
./target/debug/mbox-e2e send 7002 coordinator "hello"
./target/debug/mbox-e2e expect 7001 coordinator "hello"
```

`mbox-e2e` is a thin test binary added under `crates/classic-mbox/tests/bin/`.

### Hardware-dependent

None. All tests run on plain Linux without GPU or special PCI hardware.

## Acceptance criteria

- [ ] AC-1: `cargo build -p classic-mbox` and `cargo test -p classic-mbox` pass on Linux x86_64 stable Rust.
- [ ] AC-2: The two-node end-to-end script runs to completion, with task B's `recv()` returning `b"hello"` originally sent from task A.
- [ ] AC-3: When task A is killed (SIGKILL), node B observes `coordinator` disappear from `service_lookup` within 2 s.
- [ ] AC-4: `MailboxRecv` drop evicts the registry entry — verified by a follow-up send not delivering and a debug counter incrementing.
- [ ] AC-5: 3-node CRDT convergence test: three nodes each declare `coordinator` with different `NetId`s; all three converge to the identical 3-element `service_lookup` result.
- [ ] AC-6: Every frame in `0x0200–0x02FF` round-trips through `classic-proto` bincode (table-driven test).
- [ ] AC-7: `classic-mbox` emits no frame kinds outside `0x0200–0x02FF` (static review + debug assertion in `GossipEngine`).
- [ ] AC-8: rustdoc on every public item in `classic-mbox::lib`; `cargo clippy -p classic-mbox -- -D warnings` clean.

## Open questions

- **OQ-1:** Log level for sends to a non-existent local `MboxId` — trace (proposed) or warn? Defer to first plan-04 integration.
- **OQ-2:** Tombstone TTL is 60 s; verify against plan 02's observed gossip latency before locking.
- **OQ-3:** `select_mboxes(&mut [&mut MailboxRecv])` is awkward at call sites; consider a token-handle API. Defer to plan 06's first concrete user.
- **OQ-4:** Surface `MailDeliveryFailure` to applications via a sender-side stream, or stay log-only? v1 = log-only; revisit if plan 06 needs explicit failure surfacing.
- **OQ-5:** `service_lookup_one` is deterministic round-robin; randomize to avoid herd effects? Defer to a plan-07 benchmark.

## References

- `plans/ARCHITECTURE.md` — `NodeId`, `NetId`, `MboxId`, frame format, kind ranges.
- `plans/01-skeleton-transport.md` — peer connection lifecycle, `Hello` handshake, `Connection` trait.
- `plans/02-node-ad-hw-discovery.md` — gossip pattern this doc piggybacks on.
- ChrysaLisp Service Manager: `class/lisp/sys/lisp.inc`, `apps/services/`.
- Lamport, "Time, Clocks, and the Ordering of Events in a Distributed System", CACM 1978.
- Shapiro et al., "A Comprehensive Study of Convergent and Commutative Replicated Data Types" (last-writer-wins register).
- `bincode` v2 docs.
