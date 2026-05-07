# Agent Instructions

This project uses **bd** (beads) for issue tracking. Run `bd onboard` to get started.

## Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work atomically
bd close <id>         # Complete work
bd dolt push          # Push beads data to remote
```

## Non-Interactive Shell Commands

**ALWAYS use non-interactive flags** with file operations to avoid hanging on confirmation prompts.

Shell commands like `cp`, `mv`, and `rm` may be aliased to include `-i` (interactive) mode on some systems, causing the agent to hang indefinitely waiting for y/n input.

**Use these forms instead:**
```bash
# Force overwrite without prompting
cp -f source dest           # NOT: cp source dest
mv -f source dest           # NOT: mv source dest
rm -f file                  # NOT: rm file

# For recursive operations
rm -rf directory            # NOT: rm -r directory
cp -rf source dest          # NOT: cp -r source dest
```

**Other commands that may prompt:**
- `scp` - use `-o BatchMode=yes` for non-interactive
- `ssh` - use `-o BatchMode=yes` to fail instead of prompting
- `apt-get` - use `-y` flag
- `brew` - use `HOMEBREW_NO_AUTO_UPDATE=1` env var

<!-- BEGIN BEADS INTEGRATION profile:full hash:d4f96305 -->
## Issue Tracking with bd (beads)

**IMPORTANT**: This project uses **bd (beads)** for ALL issue tracking. Do NOT use markdown TODOs, task lists, or other tracking methods.

### Why bd?

- Dependency-aware: Track blockers and relationships between issues
- Git-friendly: Dolt-powered version control with native sync
- Agent-optimized: JSON output, ready work detection, discovered-from links
- Prevents duplicate tracking systems and confusion

### Quick Start

**Check for ready work:**

```bash
bd ready --json
```

**Create new issues:**

```bash
bd create "Issue title" --description="Detailed context" -t bug|feature|task -p 0-4 --json
bd create "Issue title" --description="What this issue is about" -p 1 --deps discovered-from:bd-123 --json
```

**Claim and update:**

```bash
bd update <id> --claim --json
bd update bd-42 --priority 1 --json
```

**Complete work:**

```bash
bd close bd-42 --reason "Completed" --json
```

### Issue Types

- `bug` - Something broken
- `feature` - New functionality
- `task` - Work item (tests, docs, refactoring)
- `epic` - Large feature with subtasks
- `chore` - Maintenance (dependencies, tooling)

### Priorities

- `0` - Critical (security, data loss, broken builds)
- `1` - High (major features, important bugs)
- `2` - Medium (default, nice-to-have)
- `3` - Low (polish, optimization)
- `4` - Backlog (future ideas)

### Workflow for AI Agents

1. **Check ready work**: `bd ready` shows unblocked issues
2. **Claim your task atomically**: `bd update <id> --claim`
3. **Work on it**: Implement, test, document
4. **Discover new work?** Create linked issue:
   - `bd create "Found bug" --description="Details about what was found" -p 1 --deps discovered-from:<parent-id>`
5. **Complete**: `bd close <id> --reason "Done"`

### Auto-Sync

bd automatically syncs via Dolt:

- Each write auto-commits to Dolt history
- Use `bd dolt push`/`bd dolt pull` for remote sync
- No manual export/import needed!

### Important Rules

- ✅ Use bd for ALL task tracking
- ✅ Always use `--json` flag for programmatic use
- ✅ Link discovered work with `discovered-from` dependencies
- ✅ Check `bd ready` before asking "what should I work on?"
- ❌ Do NOT create markdown TODO lists
- ❌ Do NOT use external issue trackers
- ❌ Do NOT duplicate tracking systems

For more details, see README.md and docs/QUICKSTART.md.

## Landing the Plane (Session Completion)

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
   bd dolt push
   git push
   git status  # MUST show "up to date with origin"
   ```
5. **Clean up** - Clear stashes, prune remote branches
6. **Verify** - All changes committed AND pushed
7. **Hand off** - Provide context for next session

**CRITICAL RULES:**
- Work is NOT complete until `git push` succeeds
- NEVER stop before pushing - that leaves work stranded locally
- NEVER say "ready to push when you are" - YOU must push
- If push fails, resolve and retry until it succeeds

<!-- END BEADS INTEGRATION -->

## Feature Development Workflow

**This is the primary work rhythm of the project.** Read this section in full before doing any planning or implementation work.

### Why this workflow exists

Beads filed in one session may be claimed weeks later by a different agent (or by a human, or by you after compaction). Conversation context, plan-mode plans, scratch notes, and ephemeral memories are not durable — **the design doc and the bead descriptions are the only durable record**. If a fresh agent in a new session, with no memory of the originating conversation, cannot start the work cold from `bd show <id>` plus the linked design doc, the work was not properly planned.

### The four phases (in order)

#### Phase 1 — Write the feature design document

Path: `plans/<short-feature-slug>.md`

Every design doc must contain these sections, in this order:

- **Scope** — what is in, what is explicitly out
- **Reasoning** — why we are doing this, the problem it solves, alternatives considered and rejected
- **Design** — architecture, data shapes, wire formats, key interfaces, file/crate layout, sequence diagrams where useful
- **Requirements** — functional and non-functional (perf targets, compatibility, security, target hardware)
- **Testing plan** — unit / integration / e2e tests that will prove correctness, including how each will be run
- **Acceptance criteria** — explicit, testable checklist for "done"

A starting template lives at `plans/TEMPLATE.md`.

The doc is the source of truth. Beads link to it (with a commit SHA) and excerpt the slice each one needs to be self-contained — they do not duplicate the whole doc.

#### Phase 2 — File the epic bead

```bash
bd create --type=epic --priority=<0-4> \
  --title="Feature: <name>" \
  --description="<elevator pitch + path to plans/<slug>.md @ <commit-sha>>" \
  --design="<key design decisions, copied from the doc>" \
  --acceptance="<acceptance criteria, copied verbatim from the doc>"
```

One epic per feature. The epic is the rallying point; it carries no implementation work itself.

If the epic depends on another epic (e.g. "GPU discovery" depends on "Skeleton + transport"), wire the dependency immediately:

```bash
bd dep add <this-epic-id> <prereq-epic-id>
```

#### Phase 3 — File implementation task beads under the epic

Break the epic into tasks small enough for one agent session. For each task:

```bash
bd create --type=task --priority=<0-4> \
  --title="<concrete deliverable>" \
  --description="<everything needed to start cold — see required content below>" \
  --acceptance="<task-specific acceptance criteria>"
```

Each task description **must** include:

- The slice of the design doc relevant to this task, **excerpted** (not just linked — docs change)
- A pointer to the design doc at a specific commit SHA
- Inputs: file paths to read, existing crates/modules to extend, message formats to honor
- Outputs: file paths to create or modify, function signatures, public APIs, wire-format additions
- Test plan for *this* task (commands to run, fixtures needed)
- Acceptance criteria for *this* task
- Any non-obvious context the task needs (env vars, hardware required to test, fixtures to mock)

#### Mandatory: dependency tracking on every bead

**Every epic and every task MUST have its dependencies wired before phase 3 is considered complete.** Without dependencies, beads land out of order, produce broken intermediate states, and require rework. Wire them with:

```bash
bd dep add <task-id> <epic-id>            # task is part of an epic
bd dep add <task-id> <prereq-task-id>     # task cannot start until prereq closes
bd dep add <epic-id> <prereq-epic-id>     # epic ordering
```

After filing tasks, verify the dependency graph is correct:

```bash
bd ready                # tasks with NO unmet prereqs — should match phase-1 tasks only
bd blocked              # tasks waiting on prereqs — should match downstream tasks
bd show <epic-id>       # check the full graph
bd orphans              # MUST be empty — orphans mean broken deps
```

If `bd ready` shows tasks that should be later, or `bd blocked` is missing tasks you expect, **fix the dependency graph before declaring the planning phase complete**.

#### Phase 4 — Stop. Wait for human approval to start

Do not begin implementation. After phases 1–3 are complete:

1. Summarize the filed work (epic IDs, task counts, top-level dependency order).
2. Stop and wait for the human to explicitly say "start `<bead-id>`" or "work the next ready issue".

### Definition of "ready to implement"

Before reporting that a feature's beads are ready for human approval to start:

- [ ] Design doc exists at `plans/<slug>.md`, committed
- [ ] Epic bead is filed and references the design doc + commit SHA
- [ ] Every implementation task is filed under the epic
- [ ] Every task has its dependency edges wired (`bd dep add` to epic + any prereq tasks)
- [ ] Every task description is self-contained per the phase-3 checklist
- [ ] `bd lint` reports clean
- [ ] `bd doctor --check=conventions` reports clean
- [ ] `bd orphans` is empty
- [ ] `bd ready` shows the expected first-tier tasks (those with no prerequisites)
- [ ] `bd blocked` shows all downstream tasks correctly waiting
