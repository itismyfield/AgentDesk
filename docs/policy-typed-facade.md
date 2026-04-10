# Policy Typed Facade

Phase 1 keeps `agentdesk.db.query/execute` available, but new or migrated policies should prefer narrow typed facades implemented in Rust.

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
- Prefer `agentdesk.kv.*` over `kv_meta` SQL.
- Keep `agentdesk.db.*` for gaps, debugging, or transitional policy code only.

## Initial Migration

`policies/triage-rules.js` has been migrated off raw SQL onto:

- `agentdesk.cards.list`
- `agentdesk.cards.assign`
- `agentdesk.cards.setPriority`
- `agentdesk.agents.get`
- `agentdesk.kv.get/set`

This provides one real policy path that no longer depends on hand-written SQL while the rest of the policy layer migrates incrementally.
