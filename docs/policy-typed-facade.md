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
| `agentdesk.queue` | `status()` | Returns typed queue and outbox counts without exposing raw queue tables to JS. |
| `agentdesk.dispatch` | `create(...)` | Existing typed command, retained as-is. |
| `agentdesk.kv` | `get/set/delete(...)` | Existing typed KV facade, preferred over `kv_meta` SQL. |

## Return Shape Conventions

- Card and agent objects keep snake_case keys to minimize policy churn.
- Nullable DB fields remain `null`.
- Parsed JSON columns return objects when valid, otherwise `null`.
- Read facades return `null` for missing entities instead of throwing.
- Command facades throw only on validation or execution failure.

## Migration Guidance

- Prefer `agentdesk.cards.list(...)` over ad hoc `SELECT ... FROM kanban_cards`.
- Prefer `agentdesk.cards.assign(...)` and `agentdesk.cards.setPriority(...)` over direct `UPDATE kanban_cards`.
- Prefer `agentdesk.review.entryContext(...)` and `agentdesk.review.recordEntry(...)` for review round planning over ad hoc `task_dispatches`/`kanban_cards.review_round` SQL.
- Prefer `agentdesk.kv.*` over `kv_meta` SQL.
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

## Backlog

Typed-facade gaps are tracked as explicit follow-up slices instead of silent exceptions:

| Policy | Pending slice | Needed facade direction |
| --- | --- | --- |
| `policies/review-automation.js` | verdict handling, pipeline-stage lookup, PR tracking card reads | typed review outcome + pipeline stage query facade |
| `policies/kanban-rules.js` | preflight helpers, dispatch-completion reads, metadata merge helpers | typed preflight facade + card metadata patch facade |
| `policies/timeouts.js` | stale dispatch/session scans, kv cleanup, long-turn alert queries | typed timeout scan facade + kv sweep facade |

These slices remain allowed to use `agentdesk.db.*` until they are migrated and individually placed behind the same enforcement pattern.
