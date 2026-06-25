# AgentDesk Test Plan & Queue Hygiene Guidelines

## Queue Hygiene & Merge-Readiness
- **Duplicate Checks:** Before starting work, check open PRs for duplicates. If your generated inventory refresh or PR overlaps with existing open PRs, stop and report a no-change overlap.
- **Strict No-Change Verification:** A "no-change" report MUST have exactly zero changed files. Verify using `gh pr view --json files`. If a PR claims "no change" but modifies files (e.g. migrations, routines), it is unsafe. If an empty no-change PR is unavoidably created, its body must explicitly list the exact overlapping PR numbers and branches.
- **Stale Branch Cleanup:** Treat low-signal or stale broad branches as queue debt. Explicitly close or recommend closing stale broad branches rather than attempting to salvage them in place. A no-change result should NOT become a PR unless it explicitly changes a queue-hygiene artifact.
- **Clean Workspace (Scratch Files):** When using tools that generate scratch files or creating ad-hoc test scripts (e.g., `test_*.rs`, `test.sh`, `plan.md`, `pr-body.md`), always run a final changed-file audit (e.g. `git status`) before committing to ensure stray artifacts are not accidentally included, preventing repository pollution. Do not commit scratch PR body files such as `pr-body.md`; put PR text directly in the GitHub PR body.

## PR Body Requirements
Every PR must include:
- What changed
- Why
- WorkFingerprint (Agent, Boundary, Primary files, Queue hygiene invariant, Related PRs/issues, Non-overlapping reason)
- Duplicate/overlap check
- Verification commands and results
- Skipped checks and reasons
- Risk and rollback notes

## Verification Commands
- **Rust Changes:** `cargo check --all-targets`, `cargo test <narrow-target>`
- **Dashboard Changes:** `./scripts/verify-dashboard.sh`
- **Policy Changes:** `npm run test:policies`
- **Scripts:** `shellcheck`
- **Generated Docs:** `python3 scripts/generate_inventory_docs.py` (only if the PR explicitly owns generated inventory)

## Review Process
- Ensure generated-inventory refresh PRs contain an explicit duplicate-PR guard in the body.
- Changes must be concrete enough for agents or humans to follow.
