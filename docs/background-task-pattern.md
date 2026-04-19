# Background-task notification pattern (#796)

## Background

When an agent fires off a long-running task with `Bash run_in_background` (or
similar long-poll mechanism) and later receives a completion signal mid-turn,
the agent typically wants to surface the result in Discord. Doing this through
the normal command-bot path collides with two existing safeties:

1. The **race handler** in `src/services/discord/router/message_handler.rs`
   protects against parallel turns by deleting the new turn's placeholder
   when another turn is already in flight. For background-task results that
   placeholder is the only user-visible record of the event — deleting it
   silently destroys information.
2. The command-bot path (`announce`/`claude`/`codex`) wakes receiving agents
   in the same channel as if the message were a directive, which can trigger
   cascading work the user did not request.

This document describes the standardized delivery pattern and the safe-action
classification rules agents must follow when reacting to background-task
completions.

## Delivery: notify bot is the canonical sink

- Background-task completions and other info-only updates are delivered via
  the **notify bot**, never via the command bot.
- Agents call the existing `/api/send` endpoint with `bot: "notify"`:

  ```bash
  curl -sS -XPOST http://127.0.0.1:$ADK_PORT/api/send \
       -H 'Content-Type: application/json' \
       -d '{
             "target":  "channel:1234567890",
             "source":  "<role-id>",
             "bot":     "notify",
             "content": "🟢 main CI 통과! 85588837 success."
           }'
  ```

- The notify bot is configured separately from the command bot and is **not**
  treated as an actionable directive by receiving agents (see
  `is_allowed_turn_sender` in `src/services/discord/mod.rs`).
- The race handler (`#796`) classifies any incoming Discord message authored
  by the notify bot as `TurnKind::BackgroundTrigger` and **preserves** the
  associated placeholder when the turn loses to an in-flight one. Foreground
  (human-typed) turns keep the legacy delete-on-loss behavior.

## Safe-action classification

After a background-task notification fires, the agent must decide whether to
auto-progress or wait for explicit user confirmation. Classify the next
intended action into one of four categories:

| Category | Examples | Rule |
|---|---|---|
| **safe-auto** | additional polling, prepping the next pipeline step (no branch/history mutation), pure analysis follow-ups | Auto-progress; do **not** wait for user confirmation. Each step must still notify via the notify bot. |
| **read-only** | `git status`, `gh run view`, log inspection, additional analysis | Auto-progress; output goes to notify bot. |
| **needs-confirm** | force-push to a non-feature branch, PR merge, auto-queue reset, branch protection edits, **any rebase or other branch/history mutation** | Stop, post a notify message describing the proposed action, wait for explicit user reply. |
| **destructive** | `rm`, `git branch -D`, DB writes, secret rotation, `kill -9` on user processes | Always require user confirmation, even in clearly safe-looking contexts. Default to **stop** if uncertain. |

**Conservative default**: when in doubt, classify down (e.g. `safe-auto` →
`needs-confirm`). The cost of an unwanted confirmation prompt is bounded; the
cost of an unwanted destructive action is not.

## Auto-progression chain (Phase 2 — not yet implemented)

The full state machine for capping auto-progression chain depth (e.g. "stop
after 5 consecutive notify-driven steps and request user confirmation") is
tracked as a follow-up. For now, agents must self-limit chain depth and
include a "다음 자동 진행 액션 N개 예정" hint in the first notify message of a
chain so the user can intervene before steps fire.

## Reference points in code

- `TurnKind::{Foreground, BackgroundTrigger}` and
  `classify_turn_kind_from_author` —
  `src/services/discord/router/message_handler.rs`
- Race-handler delete branch (preserves placeholder for `BackgroundTrigger`) —
  same file, around the `mailbox_try_start_turn` call site.
- Notify bot id resolution — `resolve_notify_bot_user_id` in
  `src/services/discord/mod.rs` (delegates to
  `HealthRegistry::utility_bot_user_id("notify")`).
- `/api/send` handler — `send_handler` in
  `src/server/routes/health_api.rs`, dispatching into `health::handle_send`.
