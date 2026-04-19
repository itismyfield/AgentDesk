# PostgreSQL Cutover

`agentdesk migrate postgres-cutover` is the operator entrypoint for moving the
SQLite runtime over to PostgreSQL. It produces a JSON preflight report, can
write JSONL archives of historical tables, and (unless `--skip-pg-import` is
passed) streams the live state into the configured PostgreSQL database.

## What gets counted

`SqliteCutoverCounts` (see `src/cli/migrate/postgres_cutover.rs`) reports both
historical totals and live-state pressure:

- `audit_logs`, `session_transcripts` — historical bulk that gets archived
  and/or copied into PG.
- `active_dispatches` — `task_dispatches` rows in `pending` or `dispatched`.
- `working_sessions` — `sessions` rows in `working`.
- `open_dispatch_outbox` — `dispatch_outbox` rows that have not reached `done`.
- `pending_message_outbox` — `message_outbox` rows still in `status = 'pending'`.
  These are Discord messages enqueued by the policy engine that the
  message-outbox worker has not yet delivered.

The PG-side counts use the same column names so the report can be compared
before/after import.

## Pre-flight gates

`cutover_blocker` enforces three rules:

1. **Archive-only with live state.** If `--skip-pg-import` is passed but
   SQLite still has any in-flight dispatch, working session, open dispatch
   outbox, or pending message outbox row, the cutover refuses. An archive-only
   run cannot carry live state forward, so it would silently drop work.
2. **Open `dispatch_outbox`.** With PG import enabled, leftover dispatch
   outbox rows would replay and double-fire after cutover. Drain them first.
3. **Pending `message_outbox`.** With PG import enabled, leftover Discord
   messages would never be delivered (the worker switches to PG and forgets
   the SQLite rows). The cutover refuses unless the operator passes
   `--allow-unsent-messages` to acknowledge the loss.

## Draining `message_outbox`

In normal operation the message-outbox worker (`src/server/background.rs` and
`src/server/mod.rs`) polls `status = 'pending'` rows every few seconds and
flips them to `sent` once the local `/api/send` HTTP loop accepts them. To
drain before cutover:

- Confirm `dcserver` is running and the local HTTP server is reachable. A
  stalled worker is the most common cause of accumulated pending rows.
- If the worker is stuck, restart `dcserver` — recovery picks the worker up
  again and the queue drains.
- Re-run `agentdesk migrate postgres-cutover --dry-run` and verify
  `sqlite.pending_message_outbox` is `0` before doing the real import.

If the pending rows are known stale (e.g. retired channels or operator
already manually replayed them), pass `--allow-unsent-messages` to bypass the
gate. This flag is intentionally noisy — it exists as an escape hatch, not as
a default.

## Dry-run vs real run

`--dry-run` runs through the same blocker logic and prints the same JSON
report, so the gate is visible before you commit. Both paths populate
`sqlite.pending_message_outbox` and surface the same blocker text when the
queue is non-empty.
