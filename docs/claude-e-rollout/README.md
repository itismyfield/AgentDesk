# claude-e Runtime Rollout (Historical)

> Historical directory: #5706 c1 retires the `claude-e` E2E cell, its dedicated
> E-29 gap row, and the example E2E worker. The production adapter and live
> opt-in config remain until the later ops/c2 retirement steps. These rollout
> documents record the original rollout; their config instructions are not
> current guidance. Use [Source Of Truth](../source-of-truth.md) for canonical
> config paths. The linked documents, including shared TUI requeue guidance,
> remain here pending c4 documentation cleanup.

This directory records the work to introduce `claude-e` (https://github.com/lidge-jun/claude-e)
as a third Claude runtime option alongside the existing tmux wrapper (pipe mode)
and Claude TUI hosting.

## Historical goal

The original rollout let operators flip between three Claude runtimes per-channel and globally:

| Mode | Selector value | What it runs |
|---|---|---|
| `pipe` | `tui_hosting: false` or `runtime: pipe` | Legacy tmux wrapper around `claude -p` (current "LegacyPrompt" driver) |
| `tui` | `tui_hosting: true` or `runtime: tui` | Long-lived interactive Claude in tmux with keystroke relay (current "TuiHosting" driver) |
| `claude-e` | `runtime: claude-e` | Per-turn `claude-e run` spawn (PTY-backed `claude -p`-shape wrapper) |

The original rollout required all three modes to remain reachable via config,
with no mode deleted. That was the rollout constraint, not a requirement for
the later #5706 retirement.

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
