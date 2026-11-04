# Policy Typed Facade

Phase 1 keeps `agentdesk.db.query/execute` available for unmigrated areas, but new or migrated policy slices should prefer narrow typed facades implemented in Rust.
Once a slice is migrated, raw DB access is explicitly blocked for that slice by test coverage instead of remaining a soft convention.

## Goals

- Keep policy logic out of raw schema details where a stable domain API is possible.
- Validate inputs and return shapes in Rust before JS sees them.
- Make common reads and state changes searchable by capability instead of SQL strings.

## Phase 1 Surface

Current typed facade entrypoints:

| Namespace | Function | Notes |
| --- | --- | --- |
| `agentdesk.cards` | `get(card_id)` | Returns a typed card snapshot or `null`. JSON columns such as `metadata` are parsed into objects when valid. |
| `agentdesk.cards` | `list(filter)` | Supports bounded filters such as `status`, `statuses`, `repo_id`, `assigned_agent_id`, `unassigned`, `metadata_present`, `github_issue_number`, `limit`. |
| `agentdesk.cards` | `assign(card_id, agent_id)` | Validates both card and agent existence before updating assignment. |
| `agentdesk.cards` | `setPriority(card_id, priority)` | Accepts only `urgent`, `high`, `medium`, `low`. |
| `agentdesk.agents` | `get(agent_id)` | Returns agent metadata plus `primary_channel`, `counter_model_channel`, and `all_channels`. |
| `agentdesk.agents` | `primaryChannel(agent_id)` | Alias over the existing resolver for policy ergonomics. |
| `agentdesk.review` | `getVerdict(card_id)` | Returns canonical review verdict state from `card_review_state` with fallback to the latest completed review dispatch result. |
| `agentdesk.review` | `entryContext(card_id)` | Returns review-entry planning data: current round, completed implementation/rework count, and the next round decision without exposing `task_dispatches` SQL. |
| `agentdesk.review` | `recordEntry(card_id, opts)` | Updates `kanban_cards.review_round` and `updated_at` for the review-entry flow with an optional terminal-state guard. |
| `agentdesk.reviewState` | `sync(card_id, state, opts)` | Synchronizes the canonical review verdict state in `card_review_state`. |
| `agentdesk.kanban` | `setStatus(card_id, new_status, force?)` | Updates the card status without raw `kanban_cards.status` UPDATEs. |
| `agentdesk.kanban` | `setReviewStatus(card_id, status, opts)` | Updates the review status without raw `kanban_cards.review_status` UPDATEs. |
| `agentdesk.queue` | `status()` | Returns typed queue and outbox counts without exposing raw queue tables to JS. |
| `agentdesk.autoQueue` | `updateEntryStatus(entry_id, status, source, opts)` | Updates `auto_queue_entries` status cleanly, logging the transition. |
| `agentdesk.timeouts` | `maintainStaleSessions()` | The policy entrypoint for stale busy-session maintenance. The Rust owner selects candidates, applies thresholds and liveness decisions, constructs repairs, and performs housekeeping. |
| `agentdesk.timeouts` | `observeSessionHost(session_key)` | Owner read I/O. Resolves the full key to its sessions row and host; returns `live`/`dead` only for a legacy tmux row whose probe answered, otherwise `unknown` with the deferral `reason`. |
| `agentdesk.dispatch` | `create(...)` | Existing typed command, retained as-is. |
| `agentdesk.kv` | `get/set/delete(...)` | Typed exact-key KV facade, preferred over `kv_meta` SQL. |
| `agentdesk.kv` | `deleteMany(keys)` | Deletes exact keys with one PostgreSQL `TEXT[]` bind and returns `{ ok, deleted }`. |

## Return Shape Conventions

- Card and agent objects keep snake_case keys to minimize policy churn.
- Nullable DB fields remain `null`.
- Parsed JSON columns return objects when valid, otherwise `null`.
- Read facades return `null` for missing entities instead of throwing.
- Command facades throw only on validation or execution failure.

## Raw DB JSON Columns

`agentdesk.db.query(sql, params)` is the compatibility API for remaining raw SQL policy slices.
It keeps the historical bridge behavior: PostgreSQL `json` and `jsonb` columns are returned as serialized JSON strings.

Use `agentdesk.db.queryJson(sql, params)` only when a transitional raw query must read PostgreSQL `json` or `jsonb` values as native JS values.
Other scalar columns keep the same shape as `query`; the difference is limited to `json`/`jsonb` result columns.

## Migration Guidance

- Prefer `agentdesk.cards.list(...)` over ad hoc `SELECT ... FROM kanban_cards`.
- Prefer `agentdesk.cards.assign(...)` and `agentdesk.cards.setPriority(...)` over direct `UPDATE kanban_cards`.
- Prefer `agentdesk.review.entryContext(...)` and `agentdesk.review.recordEntry(...)` for review round planning over ad hoc `task_dispatches`/`kanban_cards.review_round` SQL.
- For stale busy-session maintenance, call `agentdesk.timeouts.maintainStaleSessions()` only. Candidate selection, timing, host/inflight decisions, repair arguments, and cleanup belong to the Rust owner. Candidate-scan, repair, deadlock-cleanup, and termination-audit operations are not exposed to policy JS.
- Prefer `agentdesk.kv.*` over `kv_meta` SQL; use `deleteMany(keys)` for exact-key batches instead of scalar delete loops.
- Keep `agentdesk.db.*` for gaps, debugging, or transitional policy code only.

## First Migration Slice

`policies/triage-rules.js` has been migrated off raw SQL onto:

- `agentdesk.cards.list`
- `agentdesk.cards.assign`
- `agentdesk.cards.setPriority`
- `agentdesk.agents.get`
- `agentdesk.kv.get/set`

`policies/review-automation.js` now has a dedicated `review-entry` slice migrated off raw SQL onto:

- `agentdesk.cards.get`
- `agentdesk.review.entryContext`
- `agentdesk.review.recordEntry`
- `agentdesk.kanban.setReviewStatus`
- `agentdesk.agents.resolveCounterModelChannel`

The migrated slice covers `onReviewEnter` and its round-planning logic. A static test blocks any future `agentdesk.db.*` reintroduction inside that slice.

`policies/timeouts/active-monitor.js` calls only `agentdesk.timeouts.maintainStaleSessions()` for Section I stale busy-session maintenance.

The owner is Rust: `services::stale_turn_reconciler::policy_maintenance` owns the 10-minute then 30-minute policy scans, shared host cache, inflight/liveness decisions, repair requests, and counter/history cleanup. The same facade retains the separate automatic sweep (5-minute grace) and keyed reconciliation contracts.

Policy JS has no candidate-scan, repair, deadlock-cleanup, or termination-audit entrypoints. `observeSessionHost` remains read I/O for the Rust owner; `maintainStaleSessions` is the active-monitor policy maintenance command. The timeout namespace also exposes `maintainUnsentNotifications` for the separate notification recovery owner. A runtime test pins the exact timeout namespace after all globals are registered, including its three raw bridge functions. A source-scan test also prevents production `policies/**/*.js` from calling the scan, repair, or deadlock-cleanup operations; the legacy oracle remains test-only.

The active-monitor policy capability manifest sets `db.raw_sql.mode=forbidden`, so static capability checks fail if `agentdesk.db.*` is reintroduced in that file.

## Backlog

Typed-facade gaps are tracked as explicit follow-up slices instead of silent exceptions:

| Policy | Pending slice | Needed facade direction |
| --- | --- | --- |
| `policies/review-automation.js` | verdict handling, pipeline-stage lookup, PR tracking card reads | typed review outcome + pipeline stage query facade |
| `policies/kanban-rules.js` | preflight helpers, dispatch-completion reads, metadata merge helpers | typed preflight facade + card metadata patch facade |
| `policies/timeouts.js` | remaining stale dispatch/orphan/long-turn alert queries outside active-monitor Section I | typed timeout scan facade + kv sweep facade |

These slices remain allowed to use `agentdesk.db.*` until they are migrated and individually placed behind the same enforcement pattern.
