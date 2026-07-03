# PostgreSQL Cutover (Historical)

> **Retired:** The PostgreSQL cutover is complete. The one-time SQLite importer
> and its operator commands have been removed from this worktree. See #3034,
> #3035, and #3874. This document remains as a historical record of the cutover
> architecture and safety gates; it is not an operator runbook.

The cutover moved the SQLite runtime state to PostgreSQL. The retired importer
produced a JSON preflight report, could write JSONL archives of historical
tables, and streamed live state into the configured PostgreSQL database unless
the operator intentionally chose archive-only mode.

Older revisions of this document included runnable cutover and dry-run
invocations. Those commands no longer exist. Restore the retired importer from
history only for an explicitly approved emergency re-cutover.

## What got counted

The retired `SqliteCutoverCounts` report counted both historical totals and
live-state pressure:

- `audit_logs`, `session_transcripts` — historical bulk that got archived
  and/or copied into PG.
- `active_dispatches` — `task_dispatches` rows in `pending` or `dispatched`.
- `working_sessions` — `sessions` rows in `working`.
- `open_dispatch_outbox` — replayable `dispatch_outbox` rows that had not
  reached a terminal status. `done` and retry-exhausted `failed` rows are
  terminal; the outbox worker only claimed `pending` rows.
- `pending_message_outbox` — `message_outbox` rows still in `status = 'pending'`.
  These are Discord messages enqueued by the policy engine that the
  message-outbox worker has not yet delivered.

The PG-side counts used the same column names so the report could be compared
before/after import.

## Historical pre-flight gates

`cutover_blocker` enforced three rules:

1. **Archive-only with live state.** If `--skip-pg-import` was passed but
   SQLite still has any in-flight dispatch, working session, open dispatch
   outbox, or pending message outbox row, the cutover refused. An archive-only
   run could not carry live state forward, so it would silently drop work.
2. **Open `dispatch_outbox`.** With PG import enabled, leftover replayable
   dispatch outbox rows would replay and double-fire after cutover. Operators
   drained `pending`/`processing` rows first. Terminal `done` and `failed` rows
   were imported as history but did not block cutover.
3. **Pending `message_outbox`.** With PG import enabled, leftover Discord
   messages would never be delivered (the worker would switch to PG and forget
   the SQLite rows). The cutover refused unless the operator used
   `--allow-unsent-messages` to acknowledge the loss.

## Draining `message_outbox`

Before cutover, normal operation depended on the message-outbox worker
(`src/server/background.rs` and
`src/server/mod.rs`), which polled `status = 'pending'` rows every few seconds
and flipped them to `sent` once the local `/api/discord/send` HTTP loop
accepted them. The historical drain process was:

- `dcserver` had to be running and the local HTTP server had to be reachable. A
  stalled worker was the most common cause of accumulated pending rows.
- If the worker was stuck, a `dcserver` restart let recovery pick the worker up
  again and drain the queue.
- Operators verified `sqlite.pending_message_outbox` was `0` before doing the
  real import.

The retired `--allow-unsent-messages` flag acknowledged known-stale pending rows
(e.g. retired channels or messages the operator had already replayed manually).
This flag was intentionally noisy — it existed as an escape hatch, not as a
default.

## Retired command behavior

The historical dry-run and real-run paths used the same blocker logic and
printed the same JSON report, so the gate was visible before import. Both paths
populated `sqlite.pending_message_outbox` and surfaced the same blocker text
when the queue was non-empty.

The cutover and dry-run commands no longer exist in the current CLI.
