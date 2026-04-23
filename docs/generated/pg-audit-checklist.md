# PG Audit Checklist: Cycle-1 Baseline

Generated from repo scan on 2026-04-23 for issue #948. This is a documentation baseline for later fix PRs, not a claim of full exhaustiveness.

Status legend:

- `follow-up`: concrete site still worth a fix or explicit decision in a later sub-PR.
- `hardened`: current code already carries the PG-specific cast/decode guard; keep as reference.
- `baseline`: schema or inventory reference that anchors later fixes.

## 1. INT4 <-> INT8 mismatches around migration 0008

| Location | Status | Note |
| --- | --- | --- |
| `migrations/postgres/0008_int4_to_bigint_audit.sql:16-20,25-27,32-49` | `baseline` | Canonical migration widening `thread_group`, `batch_phase`, `thread_group_count`, `review_round`, `github_issue_number`, `pr_number`, `stage_order`, `timeout_minutes`, `max_retries`, and related fields to `BIGINT`. |
| `src/dispatch/dispatch_context.rs:1509-1544` | `hardened` | PG helpers now decode `kanban_cards.github_issue_number` and `pr_tracking.pr_number` directly as `Option<i64>`, removing the stale `INT4` widening path around migration 0008. |
| `src/dispatch/dispatch_context.rs:1551-1569` | `hardened` | Parent-dispatch lookup now decodes `task_dispatches.chain_depth` as `Option<i64>` and increments the BIGINT-native value directly. |
| `src/services/api_friction.rs:607-640` | `hardened` | PG source-context query now reads `kc.github_issue_number` as `Option<i64>` so the BIGINT value survives without an intermediate `i32`. |
| `src/server/routes/pipeline.rs:1099-1102` | `hardened` | `pg_stage_row_to_json` now decodes `stage_order`, `timeout_minutes`, and `max_retries` directly as `i64` / `Option<i64>`. |
| `src/services/retrospectives.rs:551-599` | `hardened` | PG retrospective builder explicitly casts `github_issue_number` and `review_round` to `::BIGINT` before `i64` decode. |
| `src/db/auto_queue.rs:1861-1885` | `hardened` | PG auto-queue status query already normalizes `github_issue_number`, `thread_group`, `batch_phase`, and `review_round` to `BIGINT`. |

## 2. JSONB decode sites and missing `::text` / silent JSON fallbacks

| Location | Status | Note |
| --- | --- | --- |
| `src/db/schema.rs:158-164,914-915` | `baseline` | In-repo inventory comment explicitly calls out `metadata`, `channel_thread_map`, and `thread_id_map` as PG `JSONB` fields that break string decoders without `::text`. |
| `src/services/auto_queue/runtime.rs:126-139` | `hardened` | PG slot-clear loader avoids string decoding entirely by fetching `thread_id_map` as `Option<serde_json::Value>`. |
| `src/server/routes/dispatches/discord_delivery.rs:1266-1294,1334-1354` | `hardened` | PG slot-thread helpers fetch `thread_id_map::text` and rebind with `$1::jsonb`; good reference for later repairs. |
| `src/engine/ops/cards_ops.rs:627-655` | `hardened` | Card JSON decode fallback still returns `Null` for malformed `metadata`, `channel_thread_map`, and `deferred_dod_json`, but now emits `tracing::warn!` instead of silently swallowing the bad payload. |
| `src/services/retrospectives.rs:242-243,465-466,617-618` | `hardened` | Retrospective `result_json` fallback still rewrites malformed JSON into a normalized string `Value`, and now logs `tracing::warn!` so bad dispatch payloads are observable. |
| `src/server/routes/dispatches/outbox.rs:587-603,606-608,816-818,1556-1583` | `hardened` | Outbox review verdict extraction plus dispatch completion/message formatting still tolerate malformed `result_json`, `context_json`, and `dispatch_context` by falling back to safe defaults, and now log `tracing::warn!` on those malformed decode paths. |

## 3. TIMESTAMPTZ decode sites using `Option<String>` or alternate decode types

| Location | Status | Note |
| --- | --- | --- |
| `src/engine/ops/cards_ops.rs:310-332` | `hardened` | Dedicated PG select casts all string-decoded `TIMESTAMPTZ` columns (`created_at`, `updated_at`, `requested_at`, `suggestion_pending_at`, `review_entered_at`, `awaiting_dod_at`, `started_at`, `completed_at`) to `::text`. |
| `src/services/dispatches.rs:383-397,565-573` | `hardened` | PG dispatch listing casts `created_at`, `updated_at`, and `completed_at` to text before `String` / `Option<String>` decode. |
| `src/server/routes/reviews.rs:66-80` | `hardened` | Review decision loader casts `decided_at::text` before `Option<String>` decode. |
| `src/server/routes/stats.rs:150-188` | `hardened` | Session stats use `TO_CHAR(last_heartbeat AT TIME ZONE 'UTC', ...)` instead of raw timestamp-to-string decode. |
| `src/github/mod.rs:552-577` | `hardened` | `list_repos_pg` casts `last_synced_at::text` before loading `RepoRow.last_synced_at`. |

Initial pass did not surface a clear production PG site still decoding raw `TIMESTAMPTZ` directly into `Option<String>` without a cast. Later sub-PRs should still sweep lower-traffic handlers for missed queries.

## 4. Silent `Err -> fallback` patterns

| Location | Status | Note |
| --- | --- | --- |
| `src/server/routes/offices.rs:479-480,657-658` | `hardened` | Office-agent PG routes now log lookup failures and return `500` instead of silently degrading `office_exists_pg(...)` errors into office-not-found behavior. |
| `src/dispatch/dispatch_context.rs:1516-1519,1539-1544` | `hardened` | Card issue / PR lookup helpers still fall back to `None`, but PG fetch/decode failures now emit `tracing::warn!` instead of disappearing behind `.await.ok().flatten()`. |
| `src/services/discord/turn_bridge/completion_guard.rs:822-894` | `hardened` | PG completion-hint loading now logs query failures, per-column decode failures, and malformed `td.context` JSON before falling back to the SQLite/no-hints path. |
| `src/engine/ops/cards_ops.rs:652-655` | `hardened` | Invalid card JSON still becomes `Value::Null` for compatibility, but malformed payloads now emit `tracing::warn!`; the fallback is no longer silent. |
| `src/services/retrospectives.rs:242-243,465-466,617-618` | `hardened` | Retrospective result decoding still preserves a string fallback for malformed JSON, but the malformed payload is now observable via `tracing::warn!`. |
| `src/services/message_outbox.rs:128-150,226-236` | `hardened` | Best-effort PG enqueue failures still return `false`, but warning logs now carry backend / reason-code / session-key context so the fallback path is observable. |

## 5. SQLite <-> PG dual-write propagation gaps

| Location | Status | Note |
| --- | --- | --- |
| `src/services/api_friction.rs:181-190` | `follow-up` | Event recording chooses PG or SQLite, not both; mixed-runtime cutover can leave one backend without the event row. |
| `src/services/message_outbox.rs:123-150,226-244` | `follow-up` | Lifecycle notifications and outbox enqueue paths are single-backend writes once a PG pool is present. |
| `src/services/retrospectives.rs:91-98` | `follow-up` | Card retrospective creation routes to PG when available and skips SQLite persistence entirely. |
| `src/services/discord_dm_reply_store.rs:74-90,152-166` | `follow-up` | Pending DM reply state is backend-selective; registration and reads diverge depending on whether PG is attached. |
| `src/db/auto_queue.rs:1519-1530,1596-1605,1616-1627` | `follow-up` | SQLite-only slot rebinding helpers are still retained behind `TODO(#839)`, marking remaining mixed-backend auto-queue surface area. |

## Later sub-PR guidance

- Fix PRs for category 1 should start with `src/dispatch/dispatch_context.rs`, `src/services/api_friction.rs`, and `src/server/routes/pipeline.rs`, because they still encode stale `INT4` assumptions in PG code.
- Fix PRs for categories 2 and 4 should prioritize places where malformed JSON or failed PG reads currently degrade to `Null`, stringified payloads, or `false`.
- Fix PRs for category 5 need an explicit transition decision per subsystem: dual-write temporarily, or declare PG-only ownership and remove the SQLite compatibility path.
