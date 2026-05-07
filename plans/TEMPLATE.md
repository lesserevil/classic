# Feature: <name>

> **Status:** draft | review | approved | in-progress | done
> **Epic bead:** `bd-XXX` (filed after this doc lands)
> **Owner:** <name>
> **Last updated:** YYYY-MM-DD

## Scope

What is in scope for this feature.

What is explicitly **out of scope** for this feature (call this out — it is the most-skipped, most-load-bearing section).

## Reasoning

Why are we building this? What problem does it solve, for whom?

What alternatives were considered and rejected, and why? (At least one alternative — if there is no alternative, the design is probably under-thought.)

What does success look like in plain English?

## Design

### Architecture

Where does this fit in the system? Which crates / modules / processes are involved?

Include a sequence diagram or component diagram if the data flow is non-trivial. ASCII is fine.

### Data shapes

Schemas, struct definitions, wire formats. Be precise — this is what task beads will reference.

```rust
// Example
pub struct NodeAd {
    pub node_id: NodeId,
    pub gpus: Vec<GpuAd>,
    pub pci: Vec<PciAd>,
    // ...
}
```

### Interfaces

Public APIs, RPC methods, CLI surface. Function signatures with doc comments.

### File / crate layout

Which files will be created or modified? Use this as the spine for breaking the work into tasks.

```
crates/
  classic-foo/
    src/
      lib.rs        # NEW
      bar.rs        # NEW
  classic-baz/
    src/
      lib.rs        # MODIFIED — adds X
```

## Requirements

### Functional

- [ ] FR-1: <thing the feature must do>
- [ ] FR-2: ...

### Non-functional

- **Performance:** <e.g. spawn placement decision <50 ms for 100-node cluster>
- **Compatibility:** <e.g. Linux 6.x, x86_64 + aarch64>
- **Security:** <e.g. assumes trusted cluster; node-to-node unauthenticated>
- **Hardware:** <e.g. requires NVIDIA GPU with NVML; tested with H100>

## Testing plan

### Unit

What will be tested in isolation. Include the crate / module each test set lives in.

### Integration

Multi-component tests. How they will be set up (fixtures, mocks, test harnesses).

### End-to-end

Multi-node tests, manual smoke tests. Include the exact command sequence to reproduce.

### Hardware-dependent

Any test that requires real hardware (GPU, specific PCI device). Mark explicitly so they can be gated/skipped in CI.

## Acceptance criteria

The checklist a human reads to decide "done". Each item must be testable.

- [ ] AC-1: <observable behavior>
- [ ] AC-2: ...
- [ ] AC-N: All tests in the testing plan pass on Linux x86_64

## Open questions

Anything still undecided. Resolve before filing the epic, or note explicitly that a sub-decision is deferred and where the deferral is recorded.

## References

- Related design docs
- Upstream specs (e.g. 9P2000.L, NVML)
- Prior art
