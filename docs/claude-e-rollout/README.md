# claude-e Runtime Rollout

This directory tracks the work to introduce `claude-e` (https://github.com/lidge-jun/claude-e)
as a third Claude runtime option alongside the existing tmux wrapper (pipe mode)
and Claude TUI hosting.

## Goal

Let operators flip between three Claude runtimes per-channel and globally:

| Mode | Selector value | What it runs |
|---|---|---|
| `pipe` | `tui_hosting: false` or `runtime: pipe` | Legacy tmux wrapper around `claude -p` (current "LegacyPrompt" driver) |
| `tui` | `tui_hosting: true` or `runtime: tui` | Long-lived interactive Claude in tmux with keystroke relay (current "TuiHosting" driver) |
| `claude-e` | `runtime: claude-e` | Per-turn `claude-e run` spawn (PTY-backed `claude -p`-shape wrapper) |

All three modes must remain reachable via config — no mode is deleted.

## Documents

- [`decision-log.md`](decision-log.md) — chronological record of architecture
  decisions, alternatives considered, and rationale.
- [`rollout-plan.md`](rollout-plan.md) — phased delivery plan, rollback matrix,
  and counter-review protocol.
- [`phase-1-e2e-plan.md`](phase-1-e2e-plan.md) — Phase 1 Discord e2e deploy /
  flip / smoke / rollback runbook.
- [`phase-2-e2e-plan.md`](phase-2-e2e-plan.md) — Phase 2 three-mode parallel
  e2e stabilisation plan (scenario battery, gate criteria, observability).
- [`operator-guide.md`](operator-guide.md) — operator-facing guide: live
  toggle, per-runtime observability, rollback contract, Phase 1 known
  gaps under `runtime: claude-e`.
