# Feature: Skeleton + Transport

> **Status:** draft
> **Epic bead:** `bd-XXX` (filed after this doc lands)
> **Owner:** shedwards
> **Last updated:** 2026-05-07

This is plan 01 — the foundation every other plan depends on. It establishes the
Cargo workspace, the wire transport between `classicd` daemons, and the frame
multiplexer that later subsystems hang their handlers off. Read
[`ARCHITECTURE.md`](./ARCHITECTURE.md) first; that doc owns the type and frame-range
definitions referenced here. This doc does not redefine them.

## Scope

### In scope

- The Cargo workspace at the repo root and **all 9 crates** listed in
  ARCHITECTURE.md (§ Repository layout) as compiling stubs. Stubs other than
  `classic-proto` and `classic-node` may be empty (`pub fn _placeholder() {}` or
  similar) — they exist so dependency edges declared by later plans don't fail
  to resolve.
- `classic-proto`: the wire frame (`Frame`, length-prefixed codec), the
  `FrameKind` enum populated **only** with the `0x0000–0x00FF` range
  (`Hello`, `Heartbeat`, `Bye`, `Error`), the `Connection` trait, and the frame
  multiplexer that routes inbound frames to handlers registered by frame-kind
  range.
- `classic-node`: the `classicd` binary. Owns the peer-mesh state, dials
  configured peers, accepts inbound TCP, drives the Hello handshake and
  Heartbeat loop, and hosts the frame multiplexer.
- Static peer configuration loaded from a TOML file (default
  `/etc/classicd/config.toml`, override via `--config`).
- `NodeId` generation on first start and persistence at `state_dir/node_id`.
- `tracing`-based structured logging configurable through `RUST_LOG`.

### Out of scope (assigned to other plans — do not design here)

- Hardware discovery and node ads — **plan 02** (`classic-ad`)
- Placement predicates and matching — **plan 03** (`classic-place`)
- Spawn pipeline and child stdio — **plan 04** (`classic-spawn`, `classic-cap`,
  `classic-cli`)
- Mailboxes, service directory, gossip — **plan 05** (`classic-mbox`)
- 9P namespace server — **plan 06** (`classic-fs`)
- Placement groups (`PACK`/`SPREAD`) — **plan 07**
- TLS / authenticated peer connections — **post-v1**
- Dynamic peer discovery (mDNS, gossip-based membership) — **post-v1**
- QUIC transport — **post-v1** (the `Connection` trait deliberately admits a
  later swap; no QUIC code lands here)

If you find yourself reaching for any of the above while implementing plan 01,
stop and confirm scope with a human.

## Reasoning

Every feature beyond plan 01 needs two things to exist before it can be built:

1. A workspace that compiles, so that crate-to-crate dependencies declared by
   later plans (e.g. `classic-mbox` depending on `classic-proto`) resolve.
2. A daemon process that holds open connections to its peers and routes frames
   to subsystem handlers — otherwise `NodeAd` (plan 02), `MailDeliver`
   (plan 05), `SpawnRequest` (plan 04) etc. have nowhere to flow.

Plan 01 is the smallest thing that gets us both, and nothing more. Resisting
scope creep here is load-bearing: if we let placement-predicate or hardware-ad
work bleed into the foundation, we end up with a transport that has subsystem
assumptions baked in, and the abstraction we need (subsystems register handlers
by frame range) gets blurred.

### Alternatives considered

- **gRPC instead of length-prefixed framed TCP.** Rejected: ARCHITECTURE.md
  already locks in the framed-TCP decision. gRPC adds HTTP/2 framing,
  protobuf, and a service-method shape that fights the symmetric peer-pair
  model. Subsystem multiplexing is naturally a frame-kind range; modeling it as
  N gRPC services creates ceremony for no benefit at v1 trust level.
- **One TCP connection per subsystem.** Rejected: connection accounting
  multiplies by subsystems, and we lose head-of-line ordering guarantees that
  plan 05 (mailboxes) and plan 02 (gossip) both want with respect to plan 01
  control frames (e.g. Bye must arrive after the last app frame). One
  multiplexed connection per ordered pair is simpler, and the frame-mux
  fan-out covers the demultiplexing we actually need.
- **Bidirectional mesh with two TCP sockets per pair (one per direction).**
  Rejected: doubles the connection count for no gain — TCP is already
  full-duplex. The "lower NodeId initiates" tiebreak gives us exactly one
  connection per ordered pair without races.
- **Dynamic peer discovery now (mDNS, etcd-style membership).** Rejected:
  static config is enough to drive plan 02 onwards and the plans 08+ multinode
  demo. Dynamic discovery is post-v1.
- **JSON / MessagePack control-frame encoding.** Rejected: ARCHITECTURE.md
  fixes `bincode` v2 with serde for control frames. JSON is too slow for
  heartbeat-rate traffic on a busy cluster; MessagePack is fine but offers no
  win over bincode for an internal binary protocol.

### What success looks like

Two `classicd` processes on different hosts (or two on the same host, on
different ports) read each other's configs, dial each other, complete the Hello
handshake, exchange Heartbeats every 5 s, and log a clear unhealthy-peer event
within 15 s of one of them being killed (3 missed heartbeats). The frame
multiplexer rejects an unknown-range frame with an `Error` frame and keeps the
connection open. The daemon survives a peer disappearing and reconnects with
exponential backoff once the peer comes back.

## Design

### Architecture

`classicd` is one process per Linux host. It hosts:

- A **listener** on `node.listen_addr` accepting inbound TCP.
- A **dialer task per configured peer**, driving the reconnect loop.
- One `PeerLink` actor per established connection that owns the codec, runs
  the Hello handshake, drives Heartbeat, and forwards inbound frames to the
  multiplexer.
- A **frame multiplexer** that holds a map from frame-range to handler. Plan 01
  registers only the proto-range handler (Hello / Heartbeat / Bye / Error);
  later plans register their own ranges at startup.

```
                       classicd (this host)
   +-------------------------------------------------------------+
   |                                                             |
   |  TomlConfig --+                                             |
   |               |                                             |
   |               v                                             |
   |        +------+--------+                                    |
   |        |  PeerMesh     |  spawns one task per peer          |
   |        |  (supervisor) |                                    |
   |        +---+---------+-+                                    |
   |            |         |                                      |
   |  dial loop |         |  inbound TCP listener                |
   |  (per peer)|         |                                      |
   |            v         v                                      |
   |        +-----------------+                                  |
   |        |    PeerLink     |  one per established conn        |
   |        |  - Hello        |                                  |
   |        |  - Heartbeat    |                                  |
   |        |  - codec rx/tx  |                                  |
   |        +--------+--------+                                  |
   |                 |                                           |
   |                 v                                           |
   |        +-----------------+        registered by:            |
   |        |   FrameMux      |<---- classic-ad   (0x01..)       |
   |        |  range -> hndlr |<---- classic-mbox (0x02..)       |
   |        +-----------------+<---- classic-spawn (0x03..)      |
   |                 |             <---- classic-fs    (0x04..)  |
   |   proto-range   v                <---- classic-place(0x05..)|
   |        +-----------------+                                  |
   |        |  ProtoHandler   |  Hello, Heartbeat, Bye, Error    |
   |        +-----------------+                                  |
   |                                                             |
   +-------------------------------------------------------------+
```

### Connection lifecycle state machine

Each `PeerLink` runs this state machine. States are exhaustive — no implicit
states. Inbound and outbound (dialed) connections share the state machine; the
`role` field distinguishes them.

```
   [Init]
      |  TCP connected (dialed or accepted)
      v
   [HelloPending]
      |  send Hello, await peer Hello with timeout 5s
      |
      |--peer Hello version mismatch -----> [Closed: HandshakeRejected]
      |--peer Hello peer_node_id == self_node_id -> [Closed: SelfLoop]
      |--peer Hello duplicates an existing PeerLink for that NodeId
      |     and we are the higher NodeId in the ordered pair -----> [Closed: LosingTiebreak]
      |--timeout (no Hello in 5s) -------------------------------> [Closed: HelloTimeout]
      |
      v  Hello OK
   [Healthy]
      |  every 5s: send Heartbeat
      |  on rx Heartbeat: reset miss counter
      |  on rx any frame: forward to FrameMux
      |  on rx Bye: send Bye, [Closed: PeerBye]
      |  on rx Error(fatal=true): [Closed: PeerError]
      |  on 3 missed heartbeats (15s): [Unhealthy]
      |  on TCP rx EOF or err: [Closed: TransportLost]
      v
   [Unhealthy]
      |  same as Healthy but logged + reflected in PeerMesh status snapshot
      |  recovery: any inbound frame transitions back to [Healthy]
      v
   [Closed]
      |  PeerLink task exits; PeerMesh's reconnect loop (if outbound) re-arms
      v  with exponential backoff 250 ms -> 30 s, capped, jitter ±20%
```

State transitions and reason codes are emitted as `tracing` events at INFO so
that the multinode-demo plan can grep them.

### Hello handshake (exact sequence)

1. TCP connection established (dialer or listener side).
2. Local side serializes `HelloPayload { proto_version: PROTO_VERSION,
   node_id: <our NodeId>, listen_addr: <our advertised addr>, capabilities: 0 }`,
   wraps in `Frame { kind: Hello, payload }`, writes it.
3. Local side reads exactly one frame with a 5 s timeout. If the frame is not
   `Hello`, close with `Error { code: ProtocolViolation }`.
4. Validate `proto_version == PROTO_VERSION`. On mismatch, send
   `Error { code: ProtoVersionMismatch }` and close.
5. Validate `peer_node_id != self_node_id`. On match, log "self-loop detected"
   and close (this happens when a config accidentally lists the local node).
6. Tiebreak duplicate connections: if a `PeerLink` already exists for that
   `NodeId` and we are the higher of the two `NodeId`s on this connection,
   close this connection (we lose the tiebreak; the lower-NodeId-initiated
   connection wins). The lower-`NodeId` side keeps the connection.
7. Promote to `Healthy`. Record `peer_node_id` -> `PeerLink` in the PeerMesh
   table. Start the heartbeat ticker.

The `Hello` payload is bincode-v2 encoded:

```rust
#[derive(Serialize, Deserialize)]
pub struct HelloPayload {
    pub proto_version: u32,
    pub node_id: NodeId,
    pub listen_addr: String, // "host:port" the peer can dial back on
    pub capabilities: u64,   // reserved for v2; must be 0 in v1
}
```

### Heartbeat

- A `tokio::time::interval` of 5 s ticks per `PeerLink`. On each tick, send
  `Frame { kind: Heartbeat, payload: bincode(HeartbeatPayload) }`.
- A counter `missed: u8` increments on every tick if no inbound frame of any
  kind arrived since the previous tick. (Any inbound frame, not just
  Heartbeat, resets the counter — application traffic from plan 02+ implies
  liveness.)
- `missed >= 3` (so 15 s of silence) flips state to `Unhealthy` and logs a
  warn event. The connection stays open; we do not close on unhealthy. Closure
  is reserved for transport errors and explicit `Bye`.

```rust
#[derive(Serialize, Deserialize)]
pub struct HeartbeatPayload {
    pub seq: u64,         // monotonic per PeerLink
    pub send_time_ns: u64, // monotonic clock; consumers may ignore
}
```

### Frame format

Defined in ARCHITECTURE.md § "Wire transport (baseline)". Recap for clarity:
length-prefixed, little-endian, `length` covers `kind + payload`, max frame
size 16 MiB. Plan 01 enforces the 16 MiB cap on both encode and decode and
returns a `CodecError::FrameTooLarge` on violation.

### Frame multiplexer

The mux holds a `[Option<Arc<dyn FrameHandler>>; 8]` indexed by the high byte
of `FrameKind` (so frame `0x0123` indexes slot 1, `0x0234` indexes slot 2,
etc.). 8 slots covers `0x0000`–`0x07FF`, the entire allocated range plus one
reserved slot.

```rust
pub trait FrameHandler: Send + Sync + 'static {
    /// Called once per inbound frame whose kind falls in this handler's range.
    /// Implementors must not block; offload to their own task if needed.
    fn on_frame(&self, peer: NodeId, frame: Frame);
}

pub struct FrameMux {
    handlers: [ArcSwapOption<dyn FrameHandler>; 8],
}

impl FrameMux {
    pub fn new() -> Self { /* ... */ }
    pub fn register(&self, range_high_byte: u8, h: Arc<dyn FrameHandler>);
    pub fn dispatch(&self, peer: NodeId, frame: Frame);
}
```

A frame whose high byte has no registered handler triggers a single warn-level
log and is dropped. The connection stays open. We do not send `Error` for
unknown frame kinds — that path is reserved for handshake-time protocol
violations only, since later versions may add new kinds that older peers
should ignore.

### TOML config schema

Config file path resolution, in order:

1. `--config <path>` CLI flag (highest priority).
2. `/etc/classicd/config.toml`.
3. `$XDG_CONFIG_HOME/classicd/config.toml` (fallback for non-root dev runs).

```toml
[node]
listen_addr = "0.0.0.0:7421"        # required
state_dir   = "/var/lib/classicd"   # required; created if missing, mode 0700
peers       = ["10.0.0.2:7421",     # may be empty for a single-node test
               "10.0.0.3:7421"]

[log]
level = "info"                       # optional; overridden by RUST_LOG if set
```

```rust
#[derive(Deserialize)]
pub struct Config {
    pub node: NodeConfig,
    #[serde(default)]
    pub log: LogConfig,
}

#[derive(Deserialize)]
pub struct NodeConfig {
    pub listen_addr: String,
    pub state_dir: PathBuf,
    #[serde(default)]
    pub peers: Vec<String>,
}

#[derive(Deserialize, Default)]
pub struct LogConfig {
    #[serde(default)]
    pub level: Option<String>,
}
```

Config is loaded once at startup; SIGHUP-triggered reload is **not** in scope.
A config error is fatal and aborts startup with a clear log message.

### NodeId generation and persistence

- On startup, `classic-node` reads `state_dir/node_id`.
- File present: parse 16 raw bytes, construct `NodeId`. A short or malformed
  file is fatal — bail with a clear error.
- File absent: call `getrandom::getrandom(&mut [u8; 16])`, write the 16 bytes
  to `state_dir/node_id` with mode `0600`, then construct `NodeId`. Use an
  atomic write (`write` + `rename`) to avoid half-written files on crash.
- The `state_dir` itself is created with `0700` if missing.

### Public APIs

```rust
// classic-proto
pub const PROTO_VERSION: u32 = 1;

#[derive(Copy, Clone, Eq, PartialEq, Hash, Debug, Serialize, Deserialize)]
#[repr(u16)]
pub enum FrameKind {
    Hello     = 0x0001,
    Heartbeat = 0x0002,
    Bye       = 0x0003,
    Error     = 0x0004,
    // 0x0005..=0x00FF reserved for future proto-range frames
}

pub struct Frame {
    pub kind: u16,        // raw u16, not FrameKind, so unknown kinds round-trip
    pub payload: Bytes,
}

#[async_trait::async_trait]
pub trait Connection: Send + Sync {
    async fn send(&self, frame: Frame) -> Result<(), CodecError>;
    async fn recv(&mut self) -> Result<Frame, CodecError>;
    fn peer(&self) -> NodeId;
}

pub fn encode_frame<W: AsyncWrite + Unpin>(w: &mut W, f: &Frame) -> impl Future<Output = Result<(), CodecError>>;
pub fn decode_frame<R: AsyncRead + Unpin>(r: &mut R) -> impl Future<Output = Result<Frame, CodecError>>;

#[derive(Debug, thiserror::Error)]
pub enum CodecError {
    #[error("frame too large: {0} bytes")]
    FrameTooLarge(u32),
    #[error("io: {0}")] Io(#[from] std::io::Error),
    #[error("decode: {0}")] Decode(String),
}

#[derive(Serialize, Deserialize, Debug)]
pub struct ErrorPayload {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum ErrorCode {
    ProtoVersionMismatch,
    ProtocolViolation,
    Internal,
}
```

```rust
// classic-node
pub fn run(cfg_path: Option<PathBuf>) -> Result<(), NodeError>;

pub struct PeerMesh {
    self_id: NodeId,
    links: DashMap<NodeId, Arc<PeerLink>>,
    mux: Arc<FrameMux>,
}

impl PeerMesh {
    pub fn new(self_id: NodeId, mux: Arc<FrameMux>) -> Self;
    pub fn spawn_dialer(&self, peer_addr: String);
    pub fn spawn_listener(&self, listen_addr: String);
    pub fn snapshot(&self) -> Vec<PeerStatus>; // for diagnostics in plans 02+
}

pub struct PeerStatus {
    pub node_id: NodeId,
    pub addr: String,
    pub state: PeerState,           // Healthy | Unhealthy | Connecting | Closed
    pub last_rx: Option<Instant>,
    pub last_tx: Option<Instant>,
    pub missed_heartbeats: u8,
}
```

### CLI surface (classicd)

```
classicd [--config PATH]

Environment:
  RUST_LOG    standard tracing-subscriber filter; defaults to "info"
              if unset and no [log].level in config

Exit codes:
  0   clean shutdown via SIGTERM/SIGINT
  1   config error (file missing, malformed, invalid addr)
  2   state-dir error (permission denied, malformed node_id file)
  3   bind error (listen_addr in use)
```

A single SIGTERM/SIGINT handler:

1. Stops the listener and dialer-spawn loops.
2. Sends `Bye` on every healthy `PeerLink`, with a 1 s grace period.
3. Drops connections, exits 0.

### File / crate layout

All paths relative to the repo root.

```
Cargo.toml                                         # NEW — workspace manifest
crates/
  classic-proto/
    Cargo.toml                                     # NEW
    src/
      lib.rs                                       # NEW — re-exports
      ids.rs                                       # NEW — NodeId, MboxId, NetId
      frame.rs                                     # NEW — Frame, FrameKind, codec
      mux.rs                                       # NEW — FrameMux, FrameHandler
      conn.rs                                      # NEW — Connection trait
      proto.rs                                     # NEW — Hello/Heartbeat/Bye/Error payloads
      version.rs                                   # NEW — PROTO_VERSION
  classic-node/
    Cargo.toml                                     # NEW
    src/
      lib.rs                                       # NEW — public lib for tests
      main.rs                                      # NEW — `classicd` binary
      config.rs                                    # NEW — TOML loader
      node_id.rs                                   # NEW — generate + persist
      mesh.rs                                      # NEW — PeerMesh
      link.rs                                      # NEW — PeerLink, lifecycle FSM
      proto_handler.rs                             # NEW — handles 0x00.. frames
      shutdown.rs                                  # NEW — signal handling
  classic-ad/    src/lib.rs                        # NEW — empty stub
  classic-place/ src/lib.rs                        # NEW — empty stub
  classic-mbox/  src/lib.rs                        # NEW — empty stub
  classic-fs/    src/lib.rs                        # NEW — empty stub
  classic-cap/   src/lib.rs                        # NEW — empty stub
  classic-spawn/ src/lib.rs                        # NEW — empty stub
  classic-cli/   src/main.rs                       # NEW — empty stub binary
```

The 7 stub crates each get a minimal `Cargo.toml` declaring just `name`,
`version`, `edition = "2021"`, and an empty `[dependencies]`. They exist so
that ARCHITECTURE.md's dependency graph compiles.

## Requirements

### Functional

- [ ] FR-1: `classicd` parses a TOML config from `--config` or the default
      paths and fails fast on malformed input.
- [ ] FR-2: On first start, `classicd` generates a fresh `NodeId` and persists
      it to `state_dir/node_id` with mode `0600`.
- [ ] FR-3: On subsequent starts, `classicd` reads the persisted `NodeId`
      verbatim — the same identity must survive a restart.
- [ ] FR-4: `classicd` listens on `node.listen_addr` and accepts inbound TCP.
- [ ] FR-5: `classicd` dials every entry in `node.peers` with exponential
      backoff (250 ms → 30 s, jitter ±20%).
- [ ] FR-6: Two peers exchange `Hello` and refuse the connection if
      `proto_version` differs.
- [ ] FR-7: When two peers race-connect each other, exactly one connection
      survives (the one initiated by the lower `NodeId`).
- [ ] FR-8: Healthy peers exchange Heartbeat every 5 s; a peer is marked
      `Unhealthy` after 3 missed heartbeats (15 s).
- [ ] FR-9: An inbound frame whose kind has no registered handler is logged
      and dropped without closing the connection.
- [ ] FR-10: A `Bye` frame triggers a clean close on both sides.
- [ ] FR-11: SIGTERM / SIGINT triggers a clean shutdown that sends `Bye` to
      every healthy peer.
- [ ] FR-12: `tracing` events at INFO level cover: peer connect, peer
      disconnect, handshake reject, peer unhealthy, peer recovered.
- [ ] FR-13: The `FrameMux` accepts handler registration for any `0x0N..`
      range from `0x00` through `0x07`.
- [ ] FR-14: Frames larger than 16 MiB are rejected on encode and on decode.

### Non-functional

- **Performance:** Heartbeat round-trip on loopback is < 10 ms median. A
  10-peer mesh idle at heartbeat-rate consumes < 1% of one CPU core.
- **Compatibility:** Linux 6.1+, x86_64. Must compile on aarch64 but not
  required to be tested there.
- **Security:** Trusted cluster — no TLS, no peer auth. The `state_dir`
  persists `node_id` and is created with mode `0700`.
- **Hardware:** No hardware dependencies. CI runners with plain TCP suffice.

## Testing plan

### Unit (`crates/classic-proto`)

- `frame::tests` — round-trip encode/decode of `Hello`, `Heartbeat`, `Bye`,
  `Error` frames with random payloads (proptest, seeded).
- `frame::tests::oversize` — encoding a 17 MiB frame returns `FrameTooLarge`;
  decoder rejects a length prefix > 16 MiB without allocating.
- `mux::tests` — registering and dispatching to handlers across several
  ranges; unknown range drops with a counter increment.

### Unit (`crates/classic-node`)

- `config::tests` — round-trip parse of the example TOML; parse failures
  on missing required keys; `--config` overrides defaults.
- `node_id::tests` — generate-and-persist creates the file with mode `0600`;
  re-reading returns the same bytes; malformed file is fatal.
- `link::tests::handshake_ok` — two in-process `PeerLink`s on a `tokio::io::duplex`
  socket pair complete handshake and reach `Healthy`.
- `link::tests::handshake_version_mismatch` — peer with a forged
  `PROTO_VERSION = 2` is rejected with `ProtoVersionMismatch`.
- `link::tests::self_loop` — a peer whose Hello carries our own `NodeId` is
  rejected.
- `link::tests::tiebreak` — two `PeerLink`s for the same `NodeId` resolve so
  exactly one survives, and it is the lower-`NodeId`-initiated one.
- `link::tests::heartbeat_unhealthy` — with a frozen clock, 3 missed ticks
  flip state to `Unhealthy` and a 4th inbound frame restores it.

### Integration (`crates/classic-node/tests/`)

- `tests/two_node.rs` — spin up two `classicd` library instances on
  `127.0.0.1:0` (ephemeral ports) inside one tokio runtime, wired to dial
  each other. Assert both reach `Healthy` within 1 s and exchange ≥ 2
  heartbeats.
- `tests/reconnect.rs` — kill one of the two peers, observe the survivor's
  state moves to `Closed` and its dialer reconnects within 5 s once the peer
  restarts.
- `tests/three_node.rs` — three peers form a triangle; `mesh.snapshot()`
  reports 2 healthy peers per node.

### End-to-end

Manual smoke test (also runnable by CI on a single host with two ports):

```bash
# Terminal 1
mkdir -p /tmp/classic-a && cat > /tmp/classic-a/config.toml <<EOF
[node]
listen_addr = "127.0.0.1:7421"
state_dir = "/tmp/classic-a"
peers = ["127.0.0.1:7422"]
EOF
RUST_LOG=info cargo run --bin classicd -- --config /tmp/classic-a/config.toml

# Terminal 2
mkdir -p /tmp/classic-b && cat > /tmp/classic-b/config.toml <<EOF
[node]
listen_addr = "127.0.0.1:7422"
state_dir = "/tmp/classic-b"
peers = ["127.0.0.1:7421"]
EOF
RUST_LOG=info cargo run --bin classicd -- --config /tmp/classic-b/config.toml

# Expected log lines on both: "peer connected", "handshake ok", "peer healthy"
# Ctrl-C terminal 2; terminal 1 logs "peer disconnected" within 15 s.
# Restart terminal 2; terminal 1 logs "peer connected" again.
```

### Hardware-dependent

None for plan 01.

## Acceptance criteria

- [ ] AC-1: `cargo build --workspace` succeeds; `cargo test --workspace`
      passes on Linux x86_64.
- [ ] AC-2: All 9 crates from ARCHITECTURE.md § Repository layout exist as
      compiling stubs in the workspace.
- [ ] AC-3: Two `classicd` instances dialed at each other reach `Healthy`
      within 1 s on loopback and exchange Heartbeats indefinitely.
- [ ] AC-4: A `classicd` started with a peer pointing at a non-existent
      address keeps retrying with exponential backoff and logs each retry.
- [ ] AC-5: A `classicd` whose peer is killed observes the unhealthy
      transition within 15 s of the last heartbeat.
- [ ] AC-6: A `classicd` whose peer was killed and restarted reconnects
      automatically without operator intervention.
- [ ] AC-7: Two `classicd` instances with mismatched `PROTO_VERSION` log
      `ProtoVersionMismatch` and refuse to peer.
- [ ] AC-8: `state_dir/node_id` is created with mode `0600` and survives a
      daemon restart (same `NodeId` reported in logs).
- [ ] AC-9: `RUST_LOG=debug classicd` emits structured events for every
      state transition in the lifecycle FSM.
- [ ] AC-10: SIGTERM produces a clean shutdown with `Bye` sent to every
      healthy peer; exit code is 0.
- [ ] AC-11: A handler registered against frame range `0x01` (simulated by
      a test handler) receives a frame `kind = 0x0123` injected on a
      `PeerLink`; an unhandled `kind = 0x0723` is logged and dropped
      without closing the connection.
- [ ] AC-12: Frame larger than 16 MiB returns `FrameTooLarge` on encode and
      decode without OOM or panic.

## Open questions

- **Listen-address advertisement.** `HelloPayload` carries `listen_addr` so
  a peer learns where to dial back to. v1 trusts the peer to report its own
  address truthfully. Is that good enough, or do we want the receiver to
  derive the dial-back address from the TCP source IP? Deferred — current
  answer is "trust the config".
- **Heartbeat tunables.** The 5 s / 3-miss values are taken from the brief.
  Is making them config-tunable worth the surface? Default answer: no for v1,
  hardcode and revisit if a multinode demo demands it.
- **Connection backpressure.** The `PeerLink` send path is a `tokio::sync::mpsc`
  channel. What bound? Proposed 1024 frames per peer; oversend triggers a
  link reset. Confirm during implementation.
- **`bincode` v2 config.** Use `bincode::config::standard()` (variable int
  encoding) or `legacy()` (fixed-int LE)? ARCHITECTURE.md says "fixed-int,
  little-endian", so `legacy()` it is — but cross-check the v2 API since the
  function name has shifted across releases.

## References

- [`plans/ARCHITECTURE.md`](./ARCHITECTURE.md) — types, frame ranges, repo
  layout (do not redefine here).
- [`plans/TEMPLATE.md`](./TEMPLATE.md) — section structure.
- [`AGENTS.md`](../AGENTS.md) § Feature Development Workflow — quality bar
  for this doc and the tasks that will hang off its epic.
- ChrysaLisp transport overview — for downstream context on why we picked
  framed TCP with a frame multiplexer.
- `bincode` v2 docs — https://docs.rs/bincode/2 — for the encoding config
  open question.
- `tokio::net::TcpStream`, `tokio::time::interval` — standard library
  primitives used throughout.
