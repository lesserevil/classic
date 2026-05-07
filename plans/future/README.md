# Future plans

Roadmap-level designs for features explicitly out of scope for v1.

These docs exist so that the v1 design space is informed by what comes next — frame ranges are reserved, type shapes are forward-compatible, and architectural decisions are made with v2 in mind. They are **not** ready-to-implement plans.

## Convention

- Status frontmatter: `future`
- No epics or tasks filed in beads
- Each doc references the v1 plans it builds on or evolves
- Promotion to v2: when work is ready to start, copy the doc out of `future/`, set status to `draft`, refine to v1-doc rigor, then file the epic + tasks per the workflow in `AGENTS.md`

## Index

| #  | File                              | Topic                                              |
|----|-----------------------------------|----------------------------------------------------|
| F1 | `F1-runtime-migration.md`         | Opt-in CRIU / CRIUgpu live migration               |
| F2 | `F2-transparent-network-fs.md`    | Distributed POSIX-ish FS over the 9P namespace     |
| F3 | `F3-kernel-modules.md`            | Conservative carve-out (mostly: don't)             |
| F4 | `F4-node-security.md`             | mTLS, signed gossip, capability tokens, revocation |
| F5 | `F5-multitenancy-quotas.md`       | Tenants, quotas, fair-share, preemption (needs F4) |

## Loose dependency hints

- F1 depends on a stable v1 spawn pipeline (plan 04)
- F2 builds on the v1 9P namespace (plan 06) and is informed by F1 (open file handles must follow migrating processes)
- F3 should land last, if at all — most candidates do not need a kernel module
- F4 is a near-term v2 candidate; multi-tenancy is meaningless without it
- F5 depends on F4 (no meaningful tenant quotas without authenticated identity)
