# Feature: Node Security — Authentication, Integrity, Capability Hardening

> **Status:** future
> **Epic bead:** (not filed; this is a future-work plan)
> **Owner:** TBD
> **Last updated:** 2026-05-07

This plan describes the path from v1's "fully trusted cluster" assumption
(see [`ARCHITECTURE.md`](../ARCHITECTURE.md) § Design choices already made)
to a cluster that survives a compromised peer. It directly hardens
[`plans/01-skeleton-transport.md`](../01-skeleton-transport.md) (Hello
handshake and frame transport) and
[`plans/04-spawn-pipeline.md`](../04-spawn-pipeline.md) (in-memory
`DeviceCap` and `SpawnRequest`). Authentication, integrity, capability
binding, and revocation are mutually load-bearing — they ship together.

## Scope

### In scope

- **Mutual authentication** via long-term Ed25519 keys; `NodeId` bound
  to the public key.
- **Encrypted, integrity-protected transport** replacing v1's plain TCP.
  Primary: rustls + RFC 7250 raw public keys. Documented runner-up:
  Noise IK.
- **Hello-handshake extension** (plan 01) that proves possession of the
  private key via signed challenge — not just possession of a socket.
- **Signed gossip** — every `NodeAd`, `ServiceAd`, `ServiceForget` carries
  a signature. Receivers verify before applying; forwarders preserve the
  original signature unmodified.
- **Cryptographically-backed capability tokens** replacing the in-memory
  `DeviceCap` (plan 04). Concrete choice: biscuit (third-party verifiable,
  attenuatable, short-TTL). Tokens bind to issuing node + holder mailbox.
- **Spawn authorization** — `SpawnRequest` carries an operator-trusted
  token verified before exec.
- **Replay / freshness** — per-NodeId monotonic sequence numbers signed
  into every gossip frame, plus a roster-managed clock-skew window.
- **Operator-managed Sybil resistance** — trust roster
  (`peers.toml` with `pubkey`) is the membership boundary.
- **Revocation** via short token TTLs plus a CRL gossip channel carried
  over the existing service-directory plumbing.
- **Rate-limiting / quota** against rogue peers (handshake floods,
  oversize frames, gossip storms).
- **Tamper-evident audit log** per daemon (hash-chained, signed).

### Out of scope

- **Hardware-rooted trust** (TPM-backed identity, secure enclave key
  storage). Noted as a v3 add-on; the on-disk Ed25519 key file is the v2
  pragmatic compromise.
- **Multi-tenant policy engine** — quotas, namespaces, per-user
  fairness. Separate plan **F5**, depends on the identity primitives
  here but does not subsume them.
- **Workload sandboxing** — protecting nodes against malicious
  *workloads* is partly addressed by per-process namespaces (plan 06)
  and cgroup-v2 enforcement (plan 04). Not this plan.
- **Open-membership Sybil resistance** — no proof-of-work / -stake.
- **Encrypted-at-rest service-directory entries** — listed as Open
  question; may defer indefinitely.
- **Post-quantum migration** — the design is cryptographically agile
  enough to absorb it; no PQ algorithm lands here.
- **Federation / cross-cluster trust.** SPIFFE/SPIRE integration is
  discussed under Open questions; not a deliverable.

## Reasoning

### Why v1 deferred all of this

`ARCHITECTURE.md` is explicit: v1 assumes a fully trusted cluster. That
choice was deliberate. Every line of crypto we *didn't* write in v1 was a
line we didn't have to debug while transport, placement DSL, gossip, and
spawn were also new. v1's target is a homelab/lab cluster behind an
operator-controlled boundary; for that, plan 01's plain TCP is correct.

The honest tradeoff: bolting security on later costs more than designing
it in from day one — every plan that touched a frame without a signature
is a plan we now revisit. We pay that cost willingly because v1 needed
testable end-to-end foundations before we knew which abstractions were
load-bearing. With plans 01–08 in place we can harden them with
confidence about which seams matter.

### Threat model

A compromised peer must not be able to (a) impersonate other nodes,
(b) inject false ads to attract workloads, (c) reuse capability tokens,
(d) replay stale messages, (e) consume unbounded resources, or
(f) exfiltrate data flowing between two healthy peers.

A *network attacker* (no node compromise; can read/inject packets) is in
scope for (a), (d), (f). A *peer-compromise attacker* (full control of
one node, including its private key) is in scope for (a)–(e); (f) is
unavoidable for traffic that node is already a legitimate party to. We
are explicitly **not** defending against an attacker who has compromised
the operator's signing root — that is a "rebuild the cluster" event in
any system.

### Alternatives considered

- **Network-underlay only (WireGuard / Tailscale / Nebula).** Rejected
  as the *sole* answer: solves transport encryption + node auth, but
  not per-message signing (compromised peers still inject false ads),
  capability binding, or revocation. Documented as complementary.
- **Full PKI with X.509 + internal CA.** Rejected as the primary path:
  heavy surface (ASN.1, OCSP, name-constraints) we don't need. We use
  TLS 1.3 *as transport* via raw public keys (RFC 7250). An optional
  X.509 mode is left as a follow-on for orgs already on SPIFFE/SPIRE.
- **Noise IK end-to-end.** Strong contender (smaller library, identity
  hiding). Rejected as default for rustls maturity, FIPS path, and
  operator tooling familiarity. Kept behind a feature flag.
- **Macaroons for capabilities.** Closer prior art. We pick biscuits
  for the Rust-native fit and Datalog caveat layer; macaroons remain
  the documented fallback.
- **TTL-only revocation (no CRL).** Rejected: an hour is too long when
  an operator notices a compromise. CRL-by-gossip is cheap on top of
  plan 05's plumbing.

### Success in plain English

An operator running a 50-node Classic cluster can say: "If a node is
taken over tonight, by tomorrow my cluster is still running, the
attacker's revocation has propagated, no other nodes have been
impersonated, and the audit log lets me reconstruct exactly which
workloads touched the compromised node and when." Homelab operators who
want none of this keep v1's trust model as a config mode — security is
opt-in, but once on, it is on properly.

## Design

### Architecture

Two new crates plus modifications:

```
crates/
  classic-id/    # NEW — Ed25519 identity, key file, NodeId derivation
  classic-sec/   # NEW — secure transport, signed envelopes, biscuit
                 #       verifier, CRL store, audit log
  classic-proto/ # MOD — extended Hello
  classic-ad/    # MOD — sign NodeAd; verify on receipt
  classic-mbox/  # MOD — sign ServiceAd / ServiceForget; CRL channel
  classic-cap/   # MOD — DeviceCap backed by biscuit token
  classic-spawn/ # MOD — verify spawn-auth biscuit before exec
  classic-node/  # MOD — load identity; replace TcpStream w/ secure transport
```

`classic-id` is tiny (just `ed25519-dalek` + `getrandom`). `classic-sec`
owns everything that touches the wire or the audit log; nothing else
links rustls or biscuit directly.

### Identity (classic-id)

```rust
pub struct NodeKey { /* Ed25519 keypair */ }

impl NodeKey {
    pub fn load_or_create(state_dir: &Path) -> Result<Self, IdError>;
    pub fn public(&self) -> ed25519_dalek::VerifyingKey;
    pub fn node_id(&self) -> NodeId; // first 16 bytes of SHA-256(pubkey)
    pub fn sign(&self, msg: &[u8]) -> ed25519_dalek::Signature;
}

pub fn derive_node_id(pk: &VerifyingKey) -> NodeId {
    NodeId(Sha256::digest(pk.as_bytes())[..16].try_into().unwrap())
}
```

Persisted at `state_dir/node_key.priv` (mode `0600`), with the public
key at `node_key.pub` (`0644`). This is a **breaking change** to v1's
random-`NodeId` rule; migration is operator-driven.

Trust roster extends `peers.toml`:

```toml
[security]
mode = "strict"        # "off" (v1) | "strict" | "transitional"
roster_file = "/etc/classicd/peers.toml"

[[peer]]
addr   = "10.0.0.2:7421"
pubkey = "ed25519:MCowBQYDK2VwAyEA..."
```

`mode = "transitional"` accepts both v1 and v2 peers during a rolling
upgrade, with a loud WARN per unauthenticated peer. A CA-style fallback
is supported: roster carries a single `[root]` pubkey, and peers present
a root signature over `(pubkey, addr, valid_until)` at handshake.

### Transport (classic-sec)

Plan 01's `Connection` trait is the seam. We implement
`SecureConnection` that wraps `tokio::net::TcpStream` in **rustls 0.23+**
with **TLS 1.3 + raw public keys (RFC 7250)**. Both sides use their
long-term Ed25519 key as the TLS endpoint identity; client auth is
mandatory. After handshake completes, each side reads the peer's raw
pubkey from the rustls session and computes
`derive_node_id(peer_pubkey)`.

**Why rustls + RFC 7250 over Noise IK.** rustls is widely deployed,
async-clean, FIPS-validatable; RFC 7250 sidesteps X.509 entirely (no
Subject, SANs, or external expiration). Noise IK offers slightly better
forward secrecy and a smaller surface but diverges from operator
tooling. The decision is reversible: `classic-sec` exposes a
`SecureTransport` trait; both implementations live behind it. Default
rustls; Noise gated behind `--features noise`.

### Hello extension (plan 01 hardening)

```rust
#[derive(Serialize, Deserialize)]
pub struct HelloPayload {
    pub proto_version: u32,
    pub node_id: NodeId,
    pub listen_addr: String,
    pub capabilities: u64,
    pub challenge: [u8; 32],         // NEW
    pub challenge_sig: Vec<u8>,      // NEW — Ed25519 sig over peer's challenge
}
```

Sequence (replaces plan 01 § Hello handshake step 2 onward):

1. TLS handshake completes; both sides hold each other's raw pubkey.
2. Initiator sends `Hello{ challenge: rand32, challenge_sig: empty }`.
3. Responder verifies `derive_node_id(peer_pubkey) == initiator.node_id`.
4. Responder replies with its `challenge` plus
   `sign(initiator.challenge)`.
5. Initiator verifies, sends its own signed challenge.
6. Both sides verify, then promote to `Healthy`.

Mismatch at any step → `Error{ ProtocolViolation }`, close, audit
entry. The v1 self-loop and tiebreak rules are preserved verbatim.

### Signed gossip (plans 02 + 05 hardening)

We do **not** sign every wire frame — TLS already provides hop-by-hop
integrity. We sign *content* so a receiver can verify gossip forwarded
by a third party.

```rust
pub struct SignedEnvelope<T> {
    pub origin: NodeId,
    pub origin_seq: u64,        // monotonic per-origin
    pub issued_at: u64,         // unix-ms; advisory
    pub payload_hash: [u8; 32], // SHA-256 of bincode(payload)
    pub signature: Vec<u8>,     // Ed25519 over (origin || seq
                                //                   || issued_at
                                //                   || payload_hash)
    pub payload: T,
}
```

Forwarders MUST NOT modify the envelope. A receiver verifies:

1. `origin` is in the roster.
2. `signature` checks against the origin's pubkey.
3. `origin_seq > last_seen_seq[origin]` (replay defence).
4. `|now - issued_at| < clock_skew_window` (default 5 min).

Per-NodeId monotonic seq is the ground truth for replay defence; Lamport
clocks (already in plan 05) compose with it — Lamport orders events
across the cluster, per-origin seq says "not stale from origin X".

### Capability tokens (plan 04 hardening)

Plan 04's in-memory `DeviceCap` is replaced by a **biscuit token**
issued by the executor when a cap is acquired. Caveats: `holder ==
MboxId(N)`, `node == NodeId(...)`, `device == GpuMinor(K)`,
`exclusive == true|false`, `not_before`, `not_after`. Default TTL
1 h, max 24 h. Tokens are **attenuatable**: a holder can derive a
more-restricted child token (shorter TTL or device subset) without
contacting the executor.

```rust
pub struct DeviceCap {
    pub kind: DeviceKind,
    pub exclusive: bool,
    pub holder: MboxId,
    pub token: BiscuitToken,    // NEW — replaces in-process accounting
    _release: BrokerHandle,
}

impl CapBroker {
    pub fn acquire(/* unchanged */) -> Result<Vec<DeviceCap>, AcquireError>;
    pub fn verify_token(&self, t: &BiscuitToken, expect: &TokenExpectations)
        -> Result<(), TokenError>;
}
```

`SpawnRequest` (plan 04) gains a `spawn_auth` field carrying a biscuit
issued by an operator-trusted root attesting that the requester is
permitted to spawn at all (and what device classes they may request).
Executor verifies before fork/exec. This is the seam through which **F5
(multi-tenancy)** will eventually attach per-tenant policy.

### Replay / freshness

- Per-NodeId monotonic `origin_seq` (above) is primary.
- A sliding-window cache of recent `(origin, seq)` pairs (1024 entries,
  evict by seq) catches in-window replays.
- `issued_at` skew window catches replays older than the window.
- After daemon restart, the highest-emitted `origin_seq` is persisted
  to `state_dir/origin_seq` (atomic write); on startup we add a safety
  bump (+1024) before emitting again. Monotonicity survives crashes.

### Sybil resistance — what we promise and don't

We **do not** attempt open-membership Sybil resistance. The bound is:

> An attacker can join iff the operator adds their pubkey to the
> roster, OR they steal an existing peer's private key file.

That's it — no proof-of-work, no proof-of-stake, no federated
discovery. Documented loudly in the operator guide. Organizations
needing open membership should deploy SPIFFE/SPIRE and integrate via
the X.509 fallback path.

### Revocation

Two layers:

1. **Short TTLs.** Default 1 h for spawn/device caps; 24 h for
   membership-equivalent assertions. A revoked peer's *new* credentials
   become useless within the TTL with no further action.
2. **CRL gossip channel.** A new `RevokeEntry` frame in the mbox range
   (`0x02xx`):

   ```rust
   pub struct RevokeEntry {
       pub revoked_kind: RevokeKind,    // NodeId | TokenId
       pub revoked_id: Vec<u8>,
       pub reason: RevokeReason,        // Compromised | Retired | Operator
       pub effective_at: u64,           // unix-ms
       pub issued_by: NodeId,           // must be roster-trusted
       pub signature: Vec<u8>,
   }
   ```

   Receivers add the entry to a local CRL set; any inbound frame from
   a revoked NodeId after `effective_at` is dropped, and any biscuit
   whose issuer or token-id matches a CRL entry fails verification.
   Persisted to `state_dir/crl.jsonl`.

Operator UX:

```bash
classicctl revoke node  <NodeId>  --reason compromised
classicctl revoke token <TokenId> --reason rotated
```

Acceptance bar (AC-5): a revoked node's spawn requests are rejected
within 60 s on a healthy 3-node cluster.

### Rate limits and quotas

Per source IP: 16 concurrent unauthenticated handshakes; 100 handshake
attempts/min. Per peer post-handshake: gossip frames token-bucketed at
100/s (burst 500); oversize frames (> 16 MiB, already enforced by plan
01) drop the connection. Audit-log entry on every limiter trip.

### Audit log

```
state_dir/audit.log     # binary; one bincode AuditRecord per entry

struct AuditRecord {
    seq: u64,
    prev_hash: [u8; 32],   // SHA-256 of previous record's signed bytes
    timestamp_ns: u64,
    event: AuditEvent,     // PeerHandshake | AdSigned | TokenIssued
                           // | TokenVerified | RevokeReceived | SpawnExec
                           // | CapAcquired | CapReleased | LimiterTripped
    signature: Vec<u8>,    // Ed25519 over (seq || prev_hash || ts || event)
}
```

`classicctl audit verify` walks the chain and checks every signature.
Logs are deliberately *not* centralized — given the threat model, that
is a feature.

### File / crate layout

```
crates/
  classic-id/                          # NEW
    src/lib.rs                         # NodeKey, derive_node_id, key file IO
    src/roster.rs                      # peers.toml with pubkey field
  classic-sec/                         # NEW
    src/lib.rs
    src/transport/mod.rs               # SecureTransport trait
    src/transport/rustls_rpk.rs        # RFC 7250 over rustls
    src/transport/noise_ik.rs          # alternative; feature-gated
    src/envelope.rs                    # SignedEnvelope<T>
    src/seq.rs                         # per-origin seq cache + persistence
    src/cap_token.rs                   # biscuit issue/verify/attenuate
    src/crl.rs                         # CRL store + gossip helpers
    src/audit.rs                       # hash-chained signed log
    src/limit.rs                       # rate-limiter primitives
  classic-proto/src/proto.rs           # MOD — extended HelloPayload
  classic-ad/src/lib.rs                # MOD — wrap NodeAd in SignedEnvelope
  classic-mbox/src/directory.rs        # MOD — sign + RevokeEntry frame
  classic-cap/src/broker.rs            # MOD — DeviceCap carries BiscuitToken
  classic-spawn/src/executor.rs        # MOD — verify spawn_auth biscuit
  classic-node/src/main.rs             # MOD — load NodeKey, swap transport
  classic-cli/src/cmd/revoke.rs        # NEW
  classic-cli/src/cmd/audit.rs         # NEW
```

## Requirements

### Threat model (functional)

- [ ] TM-1: Passive network attacker cannot read frame payloads.
- [ ] TM-2: Active network attacker cannot modify a frame undetected.
- [ ] TM-3: Attacker with stolen TCP socket but no private key cannot
      complete the Hello handshake.
- [ ] TM-4: Compromised peer cannot impersonate another peer in
      gossip — receivers reject signatures from a peer's `NodeId` not
      signed by that peer's pubkey.
- [ ] TM-5: Attacker cannot replay a captured `NodeAd` / `ServiceAd`
      after the origin has emitted a higher-seq update.
- [ ] TM-6: Attacker cannot reuse a biscuit token after revocation
      (within propagation latency).
- [ ] TM-7: Attacker cannot derive an unattenuated child token from a
      restricted parent (biscuit's monotonic-attenuation property).
- [ ] TM-8: Malformed or oversize frame from a peer does not allow
      memory exhaustion or panics.
- [ ] TM-9: Handshake or gossip flood from one peer does not starve
      service to other peers (rate limiter holds).

### Non-functional

- **Cryptographic agility.** All crypto is behind `classic-sec` traits;
  algorithm IDs (`"ed25519"`, `"x25519"`) appear in serialized form so a
  PQ algorithm can be added later without breaking on-disk formats.
- **Performance.** TLS overhead < 10% of v1 loopback throughput in the
  heartbeat-rate regime; < 15% under sustained 1 MiB/s mailbox traffic.
  Signed-envelope verify on the gossip path < 1 ms per ad on x86_64.
- **Operator UX.** Rotating a node keypair is one command:
  `classicctl rotate-key` generates a new keypair, publishes a
  `RevokeEntry` for the old `NodeId`, writes new key files. Roster file
  is reloadable without daemon restart (file watch + signed reload).
- **Compatibility.** Linux 6.1+, x86_64 (aarch64 must compile).
- **Hardware.** None required. (TPM-backed identity is out of scope.)

## Testing plan

### Unit

- `classic-id::tests` — keypair gen, key file round-trip with `0600`,
  `derive_node_id` matches a fixed test vector.
- `classic-sec::envelope::tests` — sign/verify round-trip; tampered
  payload fails; wrong-pubkey fails; replay (lower seq) fails.
- `classic-sec::seq::tests` — sliding-window eviction; restart recovery
  with safety bump.
- `classic-sec::cap_token::tests` — issue/verify/attenuate
  (TTL-shorten, device-restrict); reject expired; reject revoked.
- `classic-sec::crl::tests` — CRL persistence; merge of inbound
  revocations; refusal to accept revocation from non-operator-root.
- `classic-sec::audit::tests` — chain verification; tampered record
  detected; signatures verify across rotation.
- `classic-sec::limit::tests` — token-bucket math; per-IP isolation.

### Integration — mock-evil-node harness

Lives at `crates/classic-node/tests/evil_node.rs`. A `MaliciousPeer`
driver is configured to violate one invariant per test:

- `wrong_pubkey_in_hello` — signed challenge does not match TLS
  pubkey → close with `ProtocolViolation`.
- `replay_node_ad` — capture + replay a higher-seq ad → drop + log.
- `forged_origin` — `NodeAd` with `origin = some-other-NodeId` →
  drop (signature mismatch).
- `expired_token` — present an expired biscuit on spawn → `SpawnDeny`.
- `revoked_within_60s` — operator revokes; peer attempts spawn 60 s
  later → executor refuses.
- `oversize_frame` — 17 MiB frame → close cleanly without OOM.
- `handshake_flood` — 1000 TCP connects without completing → daemon
  rate-limits at the configured threshold.

Output is `Pass | Fail | Hang` per invariant plus captured `tracing`
events for the audit-log suite.

### Fuzzing

`cargo fuzz` targets, run nightly in CI; any new finding is a release
blocker:

- `decode_hello` — random bytes into the extended `HelloPayload`.
- `decode_signed_envelope` — random bytes into
  `SignedEnvelope<NodeAd>` *before* signature verification (largest
  pre-crypto attack surface).
- `biscuit_caveats` — random Datalog caveats fed to the verifier.

### End-to-end

3-node cluster (real or in-process via `LocalSet`):

```bash
for n in n1 n2 n3; do classicd --config /etc/classicd/$n.toml & done
classicctl status   # all three Healthy with mode=strict

# Forged-NodeId attempt is rejected.
classic-evil --target n2 --impersonate n1
# expect: n2 logs "handshake reject: NodeId/pubkey mismatch"; cluster healthy.

# Revocation propagation latency.
t0=$(date +%s)
classicctl revoke node $(classicctl id n3) --reason compromised
classic spawn --requires "true" --target $(classicctl id n3) -- /bin/true
# expect: SpawnDeny PeerRevoked; (date +%s) - t0 < 60.
```

### Hardware-dependent

None. (Future TPM integration would introduce one.)

## Acceptance criteria

- [ ] AC-1: `cargo build --workspace` and `cargo test --workspace` pass
      with `--features security` on Linux x86_64.
- [ ] AC-2: With `mode = "strict"`, two daemons whose pubkeys are not
      in each other's roster cannot complete the handshake; both audit
      logs record the rejection.
- [ ] AC-3: An attacker controlling a node's network (full traffic
      capture + injection) cannot impersonate another roster member,
      proven by `evil_node::wrong_pubkey_in_hello`.
- [ ] AC-4: A captured-and-replayed `NodeAd` is dropped with a
      `replay_dropped` audit entry; cluster steady-state unaffected.
- [ ] AC-5: A revoked node's spawn requests are rejected within 60 s
      of `classicctl revoke` on a healthy 3-node cluster.
- [ ] AC-6: A biscuit attenuated to TTL=10 s is rejected after 10 s
      even if the parent token is still valid.
- [ ] AC-7: Daemon restart preserves audit-chain continuity:
      `classicctl audit verify` walks the chain across the restart
      with no gap.
- [ ] AC-8: TLS overhead on loopback is < 10% of v1 throughput in the
      heartbeat-rate regime (measured in `bench/transport.rs`).
- [ ] AC-9: `mode = "transitional"` accepts both v1 and v2 peers and
      logs every unauthenticated peer at WARN.
- [ ] AC-10: `classicctl rotate-key` generates a new keypair,
      publishes a revocation for the old `NodeId`, and the new
      identity is Healthy on every roster peer within 30 s without
      daemon restart.
- [ ] AC-11: All `evil_node` tests pass within their 60 s per-invariant
      budget.
- [ ] AC-12: `cargo fuzz run decode_signed_envelope -- -max_total_time=600`
      finds no crashes in a clean tree.

## Open questions

- **Hardware-rooted identity (TPM, secure enclave).** Storing the
  long-term key in a TPM (via `tpm2-tss`) or in an SEV/TDX enclave
  would close the "operator-root steals key file" gap. Cost: integration
  complexity, hardware variance. Proposal: file-on-disk for v2; add
  `key_source = "tpm2"` as a v3 follow-up (plan F4a) that swaps the
  `NodeKey` constructor without touching other crates.
- **Post-quantum migration.** Ed25519 is fine today; NIST PQ
  signature standards (ML-DSA, Falcon) are still settling. The format
  has an algorithm tag; migration path is parallel keys, dual-sign
  during transition, retire Ed25519. Open: when to start. Proposal:
  revisit annually starting 2027.
- **Relationship to F5 (multi-tenancy).** F5 needs per-tenant signing
  roots, per-tenant biscuits with policy caveats, per-tenant audit
  views. F4 supplies all primitives. Open: per-tenant roster files vs.
  unified roster with tenant labels — defer to F5's plan.
- **Encrypted-at-rest service-directory entries.** A compromised
  forwarder sees every `ServiceAd` flowing through it (TLS terminates
  per hop). Fix: end-to-end encrypt to consumer pubkeys. Cost: every
  consumer must declare itself ahead of time. Probably not worth the
  UX cost for v2; revisit on demand.
- **SPIFFE/SPIRE integration.** Orgs already running SPIRE have node
  identity via X.509-SVIDs. We can support that via the CA-style root
  mode (a SPIRE workload-API client signs an Ed25519 cert as a SPIFFE
  identity). Proposal: document for v2; build if a real user shows up.
- **Operator-root thresholding.** v2 assumes a single operator root;
  multi-operator m-of-n signing is deferred. Acceptable for
  homelab/lab/single-org clusters; revisit for shared infra.
- **Compromise of `state_dir`.** If an attacker reads
  `state_dir/node_key.priv` they have full impersonation until
  revoke. Mitigations: TPM, tighter perms (already `0600`), Linux
  Lockdown LSM. Proposal: ship `0600` and document the LSM recipe.

## References

### Direct hardening targets

- [`plans/01-skeleton-transport.md`](../01-skeleton-transport.md) — Hello
  handshake, frame mux, transport. The `Connection` trait seam is what
  makes the secure-transport swap surgical.
- [`plans/04-spawn-pipeline.md`](../04-spawn-pipeline.md) — `DeviceCap`
  in-memory accounting and unsigned `SpawnRequest`, replaced here with
  biscuit-backed tokens.
- [`plans/02-node-ad-hw-discovery.md`](../02-node-ad-hw-discovery.md)
  and [`plans/05-mailbox-service-directory.md`](../05-mailbox-service-directory.md) — gossip frames that gain
  `SignedEnvelope` wrappers.
- [`plans/ARCHITECTURE.md`](../ARCHITECTURE.md) — the "fully trusted
  cluster" v1 decision this plan supersedes for the v2/security mode.

### Specifications and prior art

- Trevor Perrin, **Noise Protocol Framework** —
  <https://noiseprotocol.org/noise.html>. Documented runner-up
  transport (Noise IK).
- **RFC 7250** — *Using Raw Public Keys in TLS and DTLS*. The route to
  cert-free TLS used here.
- **RFC 8446** — TLS 1.3.
- **biscuit-auth** — <https://www.biscuitsec.org/>; Rust impl
  <https://github.com/biscuit-auth/biscuit-rust>.
- Birgisson et al., *Macaroons: Cookies with Contextual Caveats for
  Decentralized Authorization in the Cloud*, NDSS 2014. Documented
  fallback if biscuit proves immature.
- **SPIFFE/SPIRE** — <https://spiffe.io>. Integration target for orgs
  already running workload identity at scale.
- Lamport, *Time, Clocks, and the Ordering of Events in a Distributed
  System*, CACM 21(7), 1978. Composes with per-origin seq for replay
  defence.

### Adjacent design influence

- **AgentOS** — <https://github.com/jordanhubbard/agentos>. The
  capability-typed-IPC pattern (every IPC carries a typed, unforgeable
  capability rather than a raw handle) directly informs the
  biscuit-backed `DeviceCap` here. Adopt the discipline: capabilities
  are the only authority, bound to identity, scope, and lifetime.
- **Tailscale's WireGuard control plane** — node identity as Curve25519
  pubkey, central roster, short-TTL credentials. Useful mental model
  for the operator-root-managed roster pattern.
- **systemd-credentials** — TPM-backed credentials in systemd. Prior
  art for the v3 TPM follow-up.
