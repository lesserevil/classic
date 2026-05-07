# Project Instructions for AI Agents

This file provides instructions and context for AI coding agents working on this project.

<!-- BEGIN BEADS INTEGRATION v:1 profile:minimal hash:7510c1e2 -->
## Beads Issue Tracker

This project uses **bd (beads)** for issue tracking. Run `bd prime` to see full workflow context and commands.

### Quick Reference

```bash
bd ready              # Find available work
bd show <id>          # View issue details
bd update <id> --claim  # Claim work
bd close <id>         # Complete work
```

### Rules

- Use `bd` for ALL task tracking — do NOT use TodoWrite, TaskCreate, or markdown TODO lists
- Run `bd prime` for detailed command reference and session close protocol
- Use `bd remember` for persistent knowledge — do NOT use MEMORY.md files

**Architecture in one line:** issues live in a local Dolt DB; sync uses `refs/dolt/data` on your git remote; `.beads/issues.jsonl` is a passive export. See https://github.com/gastownhall/beads/blob/main/docs/SYNC_CONCEPTS.md for details and anti-patterns.

## Session Completion

**When ending a work session**, you MUST complete ALL steps below. Work is NOT complete until `git push` succeeds.

**MANDATORY WORKFLOW:**

1. **File issues for remaining work** - Create issues for anything that needs follow-up
2. **Run quality gates** (if code changed) - Tests, linters, builds
3. **Update issue status** - Close finished work, update in-progress items
4. **PUSH TO REMOTE** - This is MANDATORY:
   ```bash
   git pull --rebase
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

This project uses a **document-first, deferred-implementation** workflow. The full procedure lives in `AGENTS.md` under "Feature Development Workflow" — read it before planning or implementing anything. Summary:

1. **Write the design doc** at `plans/<slug>.md` covering: Scope, Reasoning, Design, Requirements, Testing plan, Acceptance criteria. Template at `plans/TEMPLATE.md`.
2. **File the epic bead** referencing the doc (with commit SHA) and acceptance criteria.
3. **File implementation task beads** under the epic. Each task must be self-contained — a fresh agent must be able to start cold from `bd show <id>` + the design doc, with no memory of the planning conversation.
4. **Stop.** Wait for the human to explicitly say "start `<bead-id>`". Do NOT begin implementation.

**Mandatory: every epic and every task MUST have its dependencies wired** with `bd dep add` before phase 3 is complete. Verify with `bd ready`, `bd blocked`, and `bd orphans` (must be empty). Without dependencies, beads land out of order and require rework.

**Why beads must be self-contained:** beads filed today may be claimed weeks later by a different agent in a different session. Conversation context is not durable — the bead description and the linked design doc are the only durable record.

## Build & Test

_Add your build and test commands here_

```bash
# Example:
# npm install
# npm test
```

## Architecture Overview

_Add a brief overview of your project architecture_

## Conventions & Patterns

_Add your project-specific conventions here_
