# Feature: Placement Predicate Language + Matcher + Ranker

> **Status:** draft
> **Epic bead:** `bd-XXX` (filed after this doc lands)
> **Owner:** classic-place
> **Last updated:** 2026-05-07

## Scope

**In scope.** This plan defines `crates/classic-place`: a small, self-contained
expression DSL for describing the hardware/load constraints of a task, plus
the evaluator that decides whether a given `NodeAd` satisfies those
constraints, plus the ranker that orders matching candidates.

Concretely:

- A hand-rolled expression grammar (no external interpreter, no `cel-rust`,
  no `rhai`). Specified below in PEG form.
- A recursive-descent parser producing a typed AST.
- A pure evaluator over the `NodeAd` schema (defined in plan 02; field list
  reproduced here for self-containment).
- Two top-level expression kinds:
  - `Requirement` — boolean predicate. Filters candidate nodes.
  - `Rank` — numeric expression. Orders surviving candidates; higher is
    better.
- A public `place(req, rank, ads)` function that returns ranked candidates.
- Wire frames `PlacementRequest` / `PlacementResponse` allocated in the
  `0x0500–0x05FF` range owned by `classic-place`. v1 evaluates locally inside
  each daemon's ad cache; the frames are reserved so a future v2 placement
  service can be slotted in without re-allocating frame kinds.

**Out of scope.** Explicitly *not* designed in this doc:

- **Placement groups** (PACK / SPREAD across multiple nodes for related
  tasks). Plan 07.
- **Spawn pipeline.** How a chosen `NodeId` becomes a running process is
  plan 04. This crate hands a `Vec<(NodeId, f64)>` upward; spawn consumes it.
- **Hardware discovery.** What populates a `NodeAd` (NVML, sysfs, `/proc`)
  is plan 02.
- **Cross-node placement RPC behavior.** The frames are defined here. The
  remote-placement-service implementation, including consistency / staleness
  policy for remotely held ad caches, is deferred to v2.
- **Predicate authoring UI.** Users write predicates as strings in spawn
  requests. No GUI, no fluent builder.
- **Live re-placement.** Predicates are evaluated once at fork time. Drift
  after placement (a GPU getting busy, a node going offline) is the
  scheduler's problem in some future plan, not this one.

## Reasoning

### Problem

The headline feature of Classic SSI is *"users declare hardware
requirements per process; the cluster places the process on a node that has
matching hardware available."* That requires (a) a way to write the
requirements, (b) a way to evaluate them against a node's published ad, and
(c) a way to break ties when several nodes match.

The DSL is the user-visible contract. A user submitting an AI training job
writes something like

```
any(gpu, gpu.vram_mb >= 80000 && gpu.vendor == 0x10de && !gpu.in_use)
```

and expects the cluster to do the right thing. That string is the API.

### Alternatives considered

**Alt A — CEL (Common Expression Language).** Mature, well-specified, has a
Rust implementation (`cel-interpreter`). Rejected for two reasons:

1. CEL is a moving target maintained by Google. Pinning to a specific minor
   version is a maintenance liability for a project this size, and CEL's
   surface (string functions, timestamps, durations, macros) is much larger
   than we need or want to support.
2. CEL's `has()` macro and proto-message-shaped field semantics don't map
   cleanly to our `NodeAd` (which is a Rust struct with `Vec<GpuAd>`, not a
   protobuf). We'd be writing a bespoke field resolver anyway.

**Alt B — Rhai (embedded scripting language).** Turing-complete,
syntactically friendly. Rejected: Turing-completeness is a *liability* here.
Predicates evaluate against many ads on every spawn; we want a fixed,
known eval cost (linear in AST size) and a guarantee that no predicate can
loop, allocate unboundedly, or block. Rhai also has a much wider attack
surface than we want for an expression that comes in over the wire.

**Alt C — Fixed key/value matcher (Kubernetes-style label selectors).**
Simple, but doesn't compose. "≥80GB VRAM" is a range query. "any GPU not in
use" is an existential over a collection. Label selectors can't express
either without per-key special casing. We'd reinvent half a DSL on top.

**Alt D — Hand-rolled expression DSL (chosen).** Small, finite, fully
under our control. We design exactly what we need: comparisons,
booleans, dotted field access, three aggregations (`any`/`all`/`count`),
and set membership. No closures, no user-defined functions, no
strings-as-code, no I/O, no recursion. Eval is a depth-first walk over a
typed AST; cost is `O(nodes × ad_size × ast_size)`. Predictable.

### Success in plain English

A user says "I want a node with at least one idle H100." They write that
predicate, hand it to `classic spawn`, and the spawn lands on a node that
actually has an idle H100. If no node matches, they get a clear error
explaining which constraint failed across the cluster. If multiple match,
the rank picks the one with the most headroom by default. The DSL itself is
small enough to fit in the user's head after one example.

## Design

### Architecture

`classic-place` is a leaf crate in the dependency graph: it depends only on
`classic-ad` (for the `NodeAd`/`NodeId` types) and `classic-proto` (for
frame definitions). Nothing depends on `classic-place` except `classic-spawn`
and, at the binary level, `classic-node`.

Data flow at spawn time (v1, all-local evaluation):

```
classic-cli                    classicd (local node)
   │                                │
   │  SpawnRequest{ req, rank, … }  │
   ├───────────────────────────────►│
   │                                │  classic-place::place(
   │                                │      &req, &rank,
   │                                │      ad_cache.snapshot())
   │                                │       │
   │                                │       ▼
   │                                │  Vec<(NodeId, f64)>
   │                                │       │
   │                                │       ▼
   │                                │  classic-spawn picks head, dispatches
   │                                │  SpawnRequest to that NodeId.
```

The cache (`ad_cache.snapshot()`) is the locally-replicated set of `NodeAd`s
maintained by `classic-ad`'s gossip layer (plan 02). Every daemon has the
full set, so placement is a local computation.

Future flow (v2, remote placement service): the same `PlacementRequest`
frame is sent to a designated placement node, which runs the same matcher
on its own cache and returns a `PlacementResponse` with the ranked
candidate list. The matcher code is the same — only the carrier changes.

### Data shapes

#### `NodeAd` (referenced from plan 02; reproduced for self-containment)

The matcher must work over this shape. Fields the DSL can address:

```rust
pub struct NodeAd {
    pub node_id:  NodeId,
    pub hostname: String,
    pub os:       OsAd,        // .kernel: String, .distro: String
    pub cpu:      CpuAd,       // .cores: u32, .threads: u32, .model: String,
                               //   .arch: String ("x86_64" | "aarch64")
    pub mem:      MemAd,       // .total_mb: u64, .free_mb: u64
    pub load:     LoadAd,      // .cpu_pct: f64 (0..100), .mem_pct: f64,
                               //   .load_1m: f64
    pub gpu:      Vec<GpuAd>,
    pub pci:      Vec<PciAd>,
    pub labels:   BTreeMap<String, String>,
    pub gen:      u64,         // monotonically increasing per node
}

pub struct GpuAd {
    pub index:     u32,        // NVML index on the node
    pub vendor:    u32,        // PCI vendor id, e.g. 0x10de
    pub device:    u32,        // PCI device id
    pub model:     String,     // "NVIDIA H100 80GB HBM3"
    pub vram_mb:   u64,
    pub vram_free_mb: u64,
    pub sm_count:  u32,
    pub in_use:    bool,       // any process holds it (NVML query)
    pub mig:       bool,       // MIG mode active
}

pub struct PciAd {
    pub bdf:    String,        // "0000:01:00.0"
    pub vendor: u32,
    pub device: u32,
    pub class:  u32,           // PCI class code
}
```

**Type rules in the DSL:**

| Field path                   | DSL type        |
|------------------------------|-----------------|
| `cpu.cores`, `cpu.threads`   | int             |
| `cpu.arch`, `cpu.model`      | string          |
| `mem.total_mb`, `mem.free_mb`| int             |
| `load.cpu_pct`, etc.         | float           |
| `gen`                        | int             |
| `gpu`                        | collection (`[GpuAd]`) |
| `gpu.vram_mb`                | int (per element, only inside `any/all/count`) |
| `gpu.in_use`                 | bool (per element)     |
| `gpu.vendor`, `gpu.device`   | int             |
| `pci.vendor`, `pci.device`   | int (per element)      |
| `labels[\"key\"]`            | string (deferred — see Open Questions) |

Integers fit in `i64`. Floats are `f64`. Numeric comparisons coerce int → float
when one side is float.

#### Grammar

PEG-style, with implicit whitespace skipping between tokens. (PEG is a
description format here; the parser is hand-written recursive descent — see
"Parser strategy" below.)

```
Expr        := OrExpr
OrExpr      := AndExpr ( "||" AndExpr )*
AndExpr     := NotExpr ( "&&" NotExpr )*
NotExpr     := "!" NotExpr | CmpExpr
CmpExpr     := AddExpr ( CmpOp AddExpr )?
             | AddExpr "in" ListLit
CmpOp       := "==" | "!=" | "<=" | ">=" | "<" | ">"
AddExpr     := MulExpr ( ("+" | "-") MulExpr )*
MulExpr     := UnaryExpr ( ("*" | "/") UnaryExpr )*
UnaryExpr   := "-" UnaryExpr | Primary
Primary     := Aggregate
             | "(" Expr ")"
             | ListLit
             | Literal
             | FieldPath
Aggregate   := AggOp "(" Ident ( "," Expr )? ")"
AggOp       := "any" | "all" | "count"
ListLit     := "[" ( Expr ( "," Expr )* )? "]"
FieldPath   := Ident ( "." Ident )*
Ident       := [A-Za-z_][A-Za-z0-9_]*
Literal     := Float | HexInt | Int | String | Bool
Float       := [0-9]+ "." [0-9]+ ( ("e" | "E") ("+" | "-")? [0-9]+ )?
HexInt      := "0x" [0-9A-Fa-f]+
Int         := [0-9]+
String      := '"' ( [^"\\] | "\\" ["\\nrt] )* '"'
Bool        := "true" | "false"
```

**Operator precedence (low → high), right-binding noted:**

1. `||`
2. `&&`
3. `!` (prefix; right-associative as a unary op)
4. comparisons (`==`, `!=`, `<`, `<=`, `>`, `>=`, `in`) — non-associative;
   `a < b < c` is a parse error
5. `+`, `-` (binary)
6. `*`, `/`
7. `-` (unary; right-associative)
8. primaries

`+ - * /` are included in the grammar because the **rank** expression needs
arithmetic. They are *legal* in requirements too (e.g. `mem.free_mb / 1024
> 64`) but not required.

**Reserved keywords:** `any`, `all`, `count`, `in`, `true`, `false`. These
cannot appear as field names.

**Aggregates** are the only construct that introduces a binding. The
identifier after `(` (the *iteration variable*) is bound to one element of
the collection at a time and shadows any outer field path of the same
name. Examples:

```
any(gpu, gpu.vram_mb >= 80000)        # bind gpu = each GpuAd
all(gpu, gpu.in_use == false)         # all GPUs idle
count(gpu)                            # arity-1: just count the collection
count(gpu, gpu.in_use)                # arity-2: count elements where pred
count(pci, pci.class == 0x030000)     # count display-class PCI devices
```

`count` returns int. `any` and `all` return bool. The two-argument form's
predicate must evaluate to bool; otherwise it's a type error at parse time.
The collection name must be a known root collection (currently `gpu` or
`pci`).

**`in` operator.** `x in [a, b, c]` is sugar for `x == a || x == b || x ==
c`. The list members must all be the same type as the LHS (checked at
parse time).

#### AST

```rust
#[derive(Debug, Clone)]
pub enum Expr {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Expr>),                   // only legal as RHS of `in`
    Field(Vec<String>),                // ["gpu", "vram_mb"]
    BinOp(BinOp, Box<Expr>, Box<Expr>),
    UnaryOp(UnaryOp, Box<Expr>),
    In(Box<Expr>, Vec<Expr>),          // already lowered list-membership
    Agg(AggOp, String /*var*/, Option<Box<Expr>>),
}

#[derive(Debug, Clone, Copy)] pub enum BinOp {
    Or, And,
    Eq, Ne, Lt, Le, Gt, Ge,
    Add, Sub, Mul, Div,
}
#[derive(Debug, Clone, Copy)] pub enum UnaryOp { Not, Neg }
#[derive(Debug, Clone, Copy)] pub enum AggOp   { Any, All, Count }

pub struct Requirement(pub Expr);  // type-checked: returns bool
pub struct Rank(pub Expr);         // type-checked: returns numeric (int|float)
```

#### Values during eval

```rust
enum Val {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}
```

Numeric ops widen to `Float` if either operand is `Float`. Comparisons
follow the same rule. There is no implicit string ↔ number coercion.

#### Wire frames (range `0x0500–0x05FF`)

Allocated kinds:

```rust
// classic-proto::FrameKind
Place_Request   = 0x0501,
Place_Response  = 0x0502,
Place_Error     = 0x0503,
// 0x0500 reserved (no zero-value frame inside a range, by convention)
// 0x0504-0x05FF reserved for future placement RPCs (v2 streaming, scope-
//   limited queries, capability checks, etc.)
```

Frame payloads (bincode v2, fixed-int LE):

```rust
pub struct PlacementRequest {
    pub req_id:  u64,         // caller-allocated correlation id
    pub req_src: String,      // raw predicate text
    pub rank_src: Option<String>, // raw rank text; None => default rank
    pub max_results: u16,     // 0 == unlimited
}

pub struct PlacementResponse {
    pub req_id: u64,
    pub candidates: Vec<PlacedCandidate>, // sorted, best first
}
pub struct PlacedCandidate { pub node: NodeId, pub score: f64 }

pub struct PlacementError {
    pub req_id: u64,
    pub kind:   PlaceErrKind, // ParseError | TypeError | NoCandidates
    pub message: String,
    pub line: Option<u32>,
    pub col:  Option<u32>,
}
```

v1 wires these frames into the protocol but only consumes them for
in-process plumbing (so we can drive the local matcher through the same
path the v2 RPC will use, which makes future migration boring).

### Interfaces

Public API of `classic-place`:

```rust
/// A parsed boolean predicate.
pub struct Requirement(/* private: Expr + provenance */);

/// A parsed numeric rank expression.
pub struct Rank(/* private: Expr + provenance */);

/// Parse a requirement predicate. Returns a structured ParseError on
/// syntax errors and a TypeError on type-rule violations (e.g. comparing
/// a string to an int, or a non-bool top-level expression).
pub fn parse_req(src: &str) -> Result<Requirement, ParseError>;

/// Parse a rank expression. Top-level type must be int or float.
pub fn parse_rank(src: &str) -> Result<Rank, ParseError>;

/// True iff the ad satisfies the requirement.
/// Never panics. Field paths that don't exist on a given ad evaluate to
/// `false` for boolean leaves and the eval as a whole short-circuits to
/// `false` (the ad is rejected). This keeps version skew between
/// `NodeAd` schemas non-fatal.
pub fn matches(req: &Requirement, ad: &NodeAd) -> bool;

/// Compute the rank score for an ad. Higher is better.
/// Same field-skew handling as `matches`: a missing field yields the
/// numeric "neutral" element for its surrounding op (0 for sum, 1 for
/// product) and the eval continues. Returns `f64::NEG_INFINITY` only if
/// the rank can't be evaluated at all (the ad is then ranked last).
pub fn score(rank: &Rank, ad: &NodeAd) -> f64;

/// Filter `ads` by `req`, score the survivors with `rank`, return them
/// sorted high-to-low by score, ties broken by `NodeId` byte order
/// (stable, deterministic).
pub fn place(req: &Requirement, rank: &Rank, ads: &[NodeAd])
    -> Vec<(NodeId, f64)>;

/// Convenience: parse + place. The most common spawn-side entry point.
pub fn place_str(req_src: &str, rank_src: Option<&str>, ads: &[NodeAd])
    -> Result<Vec<(NodeId, f64)>, ParseError>;

/// The default rank used when the user supplies no rank expression.
/// Source text: `-load.cpu_pct - 1000.0 * (count(gpu) - count(gpu, gpu.in_use))`
/// Rationale: rewards low CPU load; rewards nodes with more *idle* GPUs by
/// a heavy multiplier (a single idle GPU dominates ~1000 CPU-pct points).
pub fn default_rank() -> Rank;

/// Errors from parsing or type-checking.
#[derive(Debug, Clone)]
pub struct ParseError {
    pub msg: String,
    pub line: u32,
    pub col: u32,
    pub expected: Vec<&'static str>,   // empty for type errors
    pub kind: ParseErrorKind,
}
#[derive(Debug, Clone, Copy)]
pub enum ParseErrorKind { Lex, Syntax, Type }

impl std::error::Error for ParseError {}
impl std::fmt::Display for ParseError { /* "line N col M: ... (expected: ...)" */ }
```

#### Parser strategy

**Hand-written recursive descent.** Justification:

- The grammar is small (~12 productions) and the operator precedence is
  already encoded in the OrExpr → AndExpr → … chain. A hand-written parser
  is ~400 lines of straight-line Rust that any maintainer can step
  through in a debugger.
- We get *exact* control over error messages. PEG/`pest` macros tend to
  produce errors of the form "expected one of: …" with the full FIRST set
  at the failure site, which is noisy. We can instead emit "expected `,`
  or `)` after aggregate body" with hand-tuned wording.
- No runtime dependency on `pest` / `pest_derive` (proc-macro pulls in
  ~30 crates). For a leaf crate this matters.
- Position tracking (line, column) is straightforward: the lexer carries
  a `Pos { line: u32, col: u32, byte: u32 }` and every token records
  its start position.

The lexer is a one-pass scanner producing `Vec<Token>` with attached
positions. The parser consumes that slice with a single index; no
backtracking is needed because the grammar is LL(1) modulo the `in`
operator (which is disambiguated by lookahead-1: after an `AddExpr`, if
the next token is the `in` keyword, parse a list).

### File / crate layout

```
crates/
  classic-place/
    Cargo.toml                # NEW
    src/
      lib.rs                  # NEW — public API: parse_req, parse_rank,
                              #       matches, score, place, place_str,
                              #       default_rank
      lex.rs                  # NEW — tokens + scanner
      parse.rs                # NEW — recursive-descent parser
      ast.rs                  # NEW — Expr, BinOp, UnaryOp, AggOp
      typeck.rs               # NEW — type-check after parse
      eval.rs                 # NEW — matches + score implementations
      error.rs                # NEW — ParseError, Display impls
      frame.rs                # NEW — PlacementRequest/Response/Error
                              #       (re-exported through classic-proto
                              #        FrameKind range 0x0500-0x05FF)
      tests/
        fixtures.rs           # NEW — synthetic NodeAd builders for tests
    tests/
      grammar.rs              # NEW — table-driven parser tests
      eval.rs                 # NEW — table-driven evaluator tests
      golden_errors.rs        # NEW — error-message snapshot tests
  classic-proto/
    src/
      frame.rs                # MODIFIED — register Place_Request /
                              #             Place_Response / Place_Error
                              #             in FrameKind enum
```

No new top-level binaries. The `classic` CLI gains an indirect dependency
through `classic-spawn` later (plan 04).

### Worked sequence: spawn-time placement

```
1. CLI builds SpawnRequest { req: "any(gpu, gpu.vram_mb>=80000 && !gpu.in_use)",
                             rank: None, ... }
2. Local classicd receives it on its control mailbox.
3. classic-spawn calls classic_place::place_str(req, rank, &ad_cache.snapshot()).
4. classic-place:
   - parse_req → Requirement     (errors → SpawnDeny{ParseError})
   - rank.unwrap_or_else(default_rank)
   - filter by matches()
   - score by score()
   - sort, return Vec<(NodeId, f64)>
5. classic-spawn picks candidates[0].0; dispatches SpawnRequest frame to
   that NodeId.
6. If candidates is empty: SpawnDeny{NoCandidates, message describing
   total ads considered}.
```

## Requirements

### Functional

- [ ] FR-1: `parse_req` accepts every example in the "Examples" section
      below and rejects every example in the "Error examples" section.
- [ ] FR-2: `parse_rank` accepts numeric expressions including arithmetic
      and `count(...)` aggregations; rejects expressions whose top-level
      type is not int or float.
- [ ] FR-3: `matches(req, ad)` returns the predicate's logical truth value
      against `ad`. No I/O. No allocations beyond temporary `Val`s on the
      eval stack.
- [ ] FR-4: `score(rank, ad)` returns `f64`. Integer rank expressions are
      cast to f64 at the boundary.
- [ ] FR-5: `place(req, rank, ads)` filters then sorts. Order is total and
      deterministic: primary key score (descending, NaN sorts last),
      secondary key `NodeId` bytes (ascending).
- [ ] FR-6: `default_rank()` parses to the documented expression.
- [ ] FR-7: Aggregations: `any`, `all`, `count` work over `gpu` and `pci`
      collections. Empty collections: `any(...)`=false, `all(...)`=true,
      `count(...)`=0 (matches conventional vacuous-truth semantics).
- [ ] FR-8: `in [..]` semantics: `x in []` is `false`. Type-mismatched
      list members are a parse-time type error.
- [ ] FR-9: Hex int literals parse identically to decimal ints (same `i64`
      type).
- [ ] FR-10: Field-skew tolerance: a `NodeAd` missing a field referenced
      in a requirement causes that requirement to evaluate to `false` (no
      panic). For ranks, a missing field contributes the surrounding op's
      neutral element.
- [ ] FR-11: Frame kinds `0x0501`, `0x0502`, `0x0503` registered in
      `classic-proto::FrameKind`. Codec round-trips for each.

### Non-functional

- **Performance.** Parser: <1 ms for predicates up to 4 KB on a modern
  x86_64 core. Matcher: linear in AST size; for a 100-node cluster with
  8 GPUs/node, full `place(...)` returns in <5 ms (mid-range desktop).
  Allocations: parsing allocates O(AST nodes); matching allocates zero
  beyond the result `Vec`.
- **Compatibility.** Linux x86_64 primary; aarch64 must compile. Rust
  MSRV: workspace MSRV (currently stable). No platform `cfg` gates in
  this crate.
- **Security.** DSL is safe for untrusted input: no I/O, no shell exec,
  no FS access, no time/RNG access, no recursion in the language itself.
  Eval cost is bounded by `O(ast_size × max_collection_size)`. The parser
  rejects inputs over 64 KiB with a hard error (defense against pathological
  predicates).
- **Hardware.** None required; all tests use synthetic `NodeAd` fixtures.

## Testing plan

### Unit

In `crates/classic-place/src/` and `crates/classic-place/tests/`. All
table-driven where structure permits.

**`lex.rs` tests.**
- Every token kind (each operator, each literal kind, identifiers).
- Position tracking: assert `(line, col)` for tokens spanning newlines.
- Bad escapes in strings (`"\q"`), unterminated strings, malformed hex
  (`0xZZ`), trailing decimal point (`3.`).

**`parse.rs` tests.** Table form: `(input, expected_ast_or_error_kind)`.
- Precedence: `a || b && c` parses as `a || (b && c)`; `!a == b` parses as
  `(!a) == b`; chained comparisons are rejected.
- Aggregates: arity 1 vs 2; binding identifier shadowing.
- `in` lowers to multi-eq AST.
- Hex/decimal/float distinguishable.
- Reserved-keyword usage as field name is rejected.

**`typeck.rs` tests.**
- Numeric coercion: `int < float` ok; `string < int` rejected.
- `Requirement` top-level non-bool rejected.
- `Rank` top-level non-numeric rejected.
- Aggregate body type: `any(gpu, gpu.vram_mb)` (non-bool body) rejected.

**`eval.rs` tests.** Table-driven with the fixtures below.
- Every example in the "Examples" section evaluates to its claimed value
  on the matching fixture.
- Empty-collection vacuous-truth cases.
- Field skew: an ad with `gpu = []` against
  `any(gpu, gpu.vram_mb >= 80000)` returns `false` (not a panic).

**`error.rs` golden tests.** Snapshot-style: a small set of canonical bad
inputs has its rendered error message pinned. Updates to error wording
require updating the snapshot intentionally.

### Integration

In `crates/classic-place/tests/` (cross-module, but still single-crate).

- **End-to-end `place_str`** with a fixture cluster of 5 synthetic ads
  spanning a representative mix (no-GPU node, 1×A100, 2×H100, 8×H100,
  CPU-loaded 8×H100) — assert which node the default rank picks for each
  of the example predicates below.
- **Frame round-trip:** build a `PlacementRequest`, encode through
  `classic-proto`'s codec, decode, assert equality. Same for
  `PlacementResponse` and `PlacementError`.
- **Determinism:** same inputs, same outputs across 1000 runs (catch
  iteration-order bugs in `BTreeMap`/`HashMap` should they sneak in).

### End-to-end

This crate has no e2e (no daemon, no network) at its own level. Its
behavior is exercised end-to-end through plan 04 (spawn pipeline) and
plan 08 (multinode demo). Those plans own:

- "spawn an H100-requiring task on a 2-node cluster, observe it lands on
  the H100-bearing node" — plan 04.
- Multi-node ranking — plan 08.

### Hardware-dependent

None at this layer. All `NodeAd` inputs are synthetic.

### Test fixtures

Defined in `tests/fixtures.rs`, callable from any test:

```rust
pub fn ad_cpu_only(node: u8) -> NodeAd;          // 16 cores, 64 GB RAM, no GPU
pub fn ad_a100x1(node: u8, busy: bool) -> NodeAd;  // 1× A100 40GB
pub fn ad_h100x2(node: u8, busy_count: u32) -> NodeAd; // 2× H100 80GB
pub fn ad_h100x8(node: u8, busy_count: u32, cpu_pct: f64) -> NodeAd;
pub fn ad_amd_mi300(node: u8) -> NodeAd;         // vendor 0x1002
pub fn ad_arm_cpu_only(node: u8) -> NodeAd;      // arch=aarch64
```

`node: u8` becomes the first byte of the `NodeId` so test failures point
to a recognizable node.

### Examples

Six concrete predicates with motivating workloads. Each becomes a row in
the eval table.

1. **"Any NVIDIA GPU with ≥80 GB VRAM that isn't in use."** — large model
   training; user wants H100/H200 specifically.
   ```
   any(gpu, gpu.vendor == 0x10de && gpu.vram_mb >= 80000 && !gpu.in_use)
   ```

2. **"Two or more idle GPUs on the same node."** — small-scale data
   parallel.
   ```
   count(gpu, !gpu.in_use) >= 2
   ```

3. **"Any GPU at all (vendor agnostic), but exclude MIG-partitioned
   ones."** — full-card workloads.
   ```
   any(gpu, !gpu.mig && !gpu.in_use)
   ```

4. **"AMD MI300X specifically."** — a workload built against ROCm.
   ```
   any(gpu, gpu.vendor in [0x1002] && gpu.vram_mb >= 192000)
   ```

5. **"At least 64 GB free RAM and CPU load below 50 %."** — CPU-bound
   inference server.
   ```
   mem.free_mb >= 65536 && load.cpu_pct < 50.0
   ```

6. **"x86_64 node with at least 32 cores and a Mellanox NIC
   (vendor 0x15b3)."** — tightly coupled networking job.
   ```
   cpu.arch == "x86_64"
     && cpu.cores >= 32
     && any(pci, pci.vendor == 0x15b3)
   ```

Plus the **default rank**:
```
-load.cpu_pct - 1000.0 * (count(gpu) - count(gpu, gpu.in_use))
```
which simplifies to "-cpu_pct - 1000*idle_gpus", so a node with 4 idle
GPUs and 10 % CPU scores `-10 - 4000 = -4010`; a node with 0 idle GPUs
and 5 % CPU scores `-5 - 0 = -5`. Higher (less negative) is better, so
the no-GPU node is *worse* than the 4-idle-GPU node — exactly what we
want.

### Error-message examples

These pin the user-visible error format. Each is a golden test.

**Unterminated string.**
```
input:  any(gpu, gpu.model == "H100
error:  line 1 col 19: unterminated string literal
        any(gpu, gpu.model == "H100
                              ^
```

**Chained comparison.**
```
input:  cpu.cores < 32 < 64
error:  line 1 col 16: comparison operators are not associative
        cpu.cores < 32 < 64
                       ^
        hint: write `cpu.cores < 32 && 32 < 64` instead
```

**Type mismatch.**
```
input:  cpu.arch >= 5
error:  line 1 col 10: type error: cannot compare string and int
        cpu.arch >= 5
                 ^^
```

**Top-level non-bool requirement.**
```
input:  cpu.cores
error:  line 1 col 1: requirement must be a boolean expression,
        got int
```

**Unknown field.**
```
input:  any(gpu, gpu.warp_count >= 4)
error:  line 1 col 22: unknown field `warp_count` on GpuAd
        (known: index, vendor, device, model, vram_mb, vram_free_mb,
         sm_count, in_use, mig)
```

**Reserved keyword as field.**
```
input:  any.in == 3
error:  line 1 col 5: `in` is a reserved keyword and cannot be used
        as a field name
```

## Acceptance criteria

- [ ] AC-1: `parse_req` accepts all six example predicates in §Examples
      and `parse_rank` accepts the default rank expression.
- [ ] AC-2: For each (predicate, fixture) pair in the eval table, the
      observed `matches`/`score` result equals the table's expected
      value.
- [ ] AC-3: `place(...)` on the 5-fixture cluster picks the documented
      best node for each example predicate under the default rank.
- [ ] AC-4: Every error-message example renders byte-identically to the
      pinned snapshot.
- [ ] AC-5: Frame kinds `0x0501`, `0x0502`, `0x0503` are registered in
      `classic-proto::FrameKind`; bincode round-trip tests pass for each
      payload struct.
- [ ] AC-6: No `unsafe` in `classic-place`. `cargo deny` (or equivalent)
      reports zero new external dependencies introduced beyond `serde`,
      `bincode`, and the workspace-shared crates.
- [ ] AC-7: `cargo test -p classic-place` is green on Linux x86_64.
- [ ] AC-8: `cargo bench` (or a single `#[test] perf_smoke()` gated under
      `--release`) shows `place(...)` under 5 ms for 100 ads × 8 GPUs ×
      a 30-AST-node predicate.
- [ ] AC-9: Documentation: every public item in `classic-place` has a
      doc comment; `cargo doc -p classic-place` builds with no warnings.

## Open questions

1. **Label access syntax.** `labels["key"]` requires bracketed string
   indexing, which the grammar above does not currently include. The
   workaround for v1 is to expose specific known labels as virtual
   fields (e.g. `labels.zone` resolved by the field resolver). Decision
   deferred to a v1.1 follow-up; track in a `discovered-from` bead once
   the epic is filed.
2. **Float precision in error messages.** Should `score` results be
   rounded to a fixed number of decimals when displayed by the CLI?
   (Not this crate's problem; flagged here so plan 04 / CLI can decide.)
3. **Per-node partial info.** If `NodeAd` is from a daemon that doesn't
   know about, say, NICs (no PCI scan yet), `any(pci, …)` is false even
   if the hardware is there. Documented as the "field skew" rule (FR-10),
   but worth re-examining once we have real telemetry.
4. **Stable score format on the wire.** `PlacementResponse::candidates`
   carries `f64`. Endianness is fixed by bincode v2 (LE). NaN handling on
   the wire: do we forbid sending NaN, or accept and sort-last? Currently
   "sort-last" — deferred unless real-world placements produce NaN.
5. **Predicate text size cap.** 64 KiB is generous. Drop to 4 KiB once
   we have real-world predicates in flight? Defer.
6. **Tie-breaking by `NodeId`.** Documented as ascending byte order.
   Alternative: random with a daemon-stable seed (avoids hot-spotting on
   the lowest-id node when many ties exist). Defer to plan 04 once spawn
   load patterns are observable.

## References

- `plans/ARCHITECTURE.md` — frame-kind allocation table, identity types,
  crate layout, transport.
- `plans/02-node-ad-hw-discovery.md` — `NodeAd` schema (this plan's
  evaluator targets that schema; field list reproduced here for
  self-containment in case plan 02 has not yet been written).
- `plans/04-spawn-pipeline.md` — primary consumer of `place(...)`.
- `plans/07-placement-groups.md` — extends placement to multi-task
  bundles; out of scope here.
- ChrysaLisp downhill placement (prior art): each node ranks itself
  against a job and the job hops downhill until it finds a local minimum.
  Classic's variant is centralized-per-spawn (one daemon ranks all known
  ads at once) but the ranking philosophy — small numeric expressions
  combining load and capacity — is borrowed.
- HTCondor ClassAds (prior art): the ancestor of "predicate over a node
  ad". HTCondor's ClassAd language is much larger (string functions,
  user-defined functions, attribute references with default values);
  Classic deliberately picks the small subset that covers the placement
  use case.
- PEG primer: Bryan Ford, *Parsing Expression Grammars: A
  Recognition-Based Syntactic Foundation* (POPL 2004) — for readers
  unfamiliar with the grammar notation used above.
