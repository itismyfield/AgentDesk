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
- [`rollout-plan.md`](rollout-plan.md) — phased delivery plan and gate conditions.
