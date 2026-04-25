# DB × File Duplication Audit (2026-04-24)

Issue: #1096 (910-2) — identify PostgreSQL tables that duplicate data already
canonically represented on the filesystem (YAML configs, skill manifests,
JSON role maps) and recommend per-table remediation.

## Methodology

1. **Table enumeration.** `ls migrations/postgres/*.sql` then
   `grep -Eho 'CREATE TABLE IF NOT EXISTS [a-z_]+'` yielded **52 unique
   tables** across migrations 0001–0018.
2. **Source classification.** For each table, locate writers in `src/` and
   `policies/` (`grep -rn 'INSERT INTO <table>'`) and cross-check for file
   loaders (`agentdesk.yaml`, `policies/default-pipeline.yaml`,
   `skills/*`, `role_map.json`). A table is **file-canonical** iff a file
   loader exists that can fully reconstruct its rows on startup.
3. **Recommendation derivation.** Based on whether the row set is
   (a) produced by runtime events only, (b) declared in a file and merely
   mirrored, or (c) partially declared / partially runtime.

Classification key:

- **keep-db-canonical** — rows are generated at runtime; no file source
  exists or could exist. Leave as-is.
- **file-canonical** — file is the authoritative source; the DB copy is
  either a startup-sync mirror or a legacy duplicate. Recommend removal
  or read-only materialized view.
- **materialized-view candidate** — partial overlap (some fields come from
  file, others are runtime state). Keep a single DB row per file entity,
  refresh file-derived columns at startup, guard non-file columns as
  runtime-only.

## Tables

| Table | Rows (est) | Canonical Source | Recommendation | Notes |
|-------|-----------|------------------|----------------|-------|
| `agents` | ~20 | **file**: `config/agentdesk.yaml` (`agents:` block) | materialized-view | `sync_agents_from_config` (src/db/agents.rs:284) mirrors yaml rows; runtime fields (`status`, `xp`, `last_seen_at`) stay in DB. Add readonly guard on file-sourced columns (`id`, `name`, `name_ko`, `provider`, `discord_channel_*`, `department`). |
| `pipeline_stages` | ~7 per repo | **file**: `policies/default-pipeline.yaml` | materialized-view | `src/pipeline.rs:60` + `src/launch.rs:11` + `src/cli/dcserver.rs:1259` re-seed from yaml on startup; `kanban.rs:3125` also inserts fallback rows. Keep DB only as per-repo override cache; readonly guard on `stage_name`, `stage_order`, `trigger_after` unless operator uses repo override. |
| `skills` | ~15 | **file**: `skills/*/` filesystem + `skills/manifest.json` | materialized-view | `skills_api.rs:212/766` upserts from filesystem walks. `source_path` column already points to the file of record. Soft-delete via 0007 migration suggests we treat filesystem removal → DB soft-delete. Readonly guard on `name`, `description`, `trigger_patterns`, `source_path`. |
| `offices` | 1–3 | **file**: `config/agentdesk.yaml` (derived from `department` + office layout) | file-canonical → remove | No runtime writers found; purely a UI grouping layer. Recommend dropping DB table and computing grouping in-memory from yaml. |
| `departments` | ~6 | **file**: `config/agentdesk.yaml` (`agents[].department`) | file-canonical → remove | Same as `offices`; derivable from yaml. Remove table, compute from `agents` + optional `org_schema.yaml`. |
| `office_agents` | ~20 | **file**: `config/agentdesk.yaml` (agent→department mapping) | file-canonical → remove | Pure join table over two file-canonical entities. Replace with in-memory view. |
| `github_repos` | 1–5 | mixed (yaml `github.repos` + runtime sync timestamps) | materialized-view | Recommend canonical list in `agentdesk.yaml`; keep `last_synced_at` as runtime column only. |
| `turns` | 10k–1M | runtime only | keep-db-canonical | Per-turn telemetry; no file source possible. |
| `session_transcripts` | 10k–100k | runtime only | keep-db-canonical | Full transcript capture; retention via 0016. |
| `session_transcripts_archive` | grows | runtime only | keep-db-canonical | Archive target for retention. |
| `sessions` | 1k–10k | runtime only | keep-db-canonical | Discord thread/channel session map. |
| `messages` | 10k+ | runtime only | keep-db-canonical | Raw message log. |
| `message_outbox` | ephemeral | runtime only | keep-db-canonical | Outbox dedup (0005). |
| `dispatch_outbox` | ephemeral | runtime only | keep-db-canonical | Outbox pattern. |
| `task_dispatches` | 1k–10k | runtime only | keep-db-canonical | Dispatch log. |
| `task_dispatches_monthly_aggregate` | small | derived | keep-db-canonical | Aggregate of `task_dispatches` (retention job). |
| `dispatch_queue` | small | runtime only | keep-db-canonical | Active dispatch queue. |
| `dispatch_events` | 10k+ | runtime only | keep-db-canonical | State-machine event log. |
| `kanban_cards` | 100–1k | runtime only | keep-db-canonical | Authoritative card state. |
| `kanban_audit_logs` | 10k+ | runtime only | keep-db-canonical | Immutable audit trail. |
| `card_review_state` | small | runtime only | keep-db-canonical | Review FSM state. |
| `card_retrospectives` | small | runtime only | keep-db-canonical | Retro log. |
| `auto_queue_runs` | 1k+ | runtime only | keep-db-canonical | Auto-queue run log. |
| `auto_queue_entries` | 10k | runtime only | keep-db-canonical | Entry log. |
| `auto_queue_slots` | small | runtime only | keep-db-canonical | Slot reservations. |
| `auto_queue_entry_transitions` | 10k | runtime only | keep-db-canonical | Transition trail. |
| `auto_queue_entry_dispatch_history` | 10k | runtime only | keep-db-canonical | Dispatch attempts. |
| `auto_queue_phase_gates` | small | runtime only | keep-db-canonical | Phase gate ledger. |
| `meetings` | 100+ | runtime only | keep-db-canonical | Round-table meetings. |
| `meeting_transcripts` | 1k+ | runtime only | keep-db-canonical | Meeting utterances. |
| `review_decisions` | 1k+ | runtime only | keep-db-canonical | Review decision log. |
| `review_tuning_outcomes` | 100+ | runtime only | keep-db-canonical | Outcome tracking. |
| `pr_tracking` | 100+ | runtime only | keep-db-canonical | PR lifecycle log. |
| `agent_archive` | small | runtime only | keep-db-canonical | Offboarded agents (0014). |
| `agent_quality_event` | 10k+ | runtime only | keep-db-canonical | Quality signal stream (0012). |
| `agent_quality_daily` | small | derived | keep-db-canonical | Daily rollup (0013). |
| `audit_logs` | 10k+ | runtime only | keep-db-canonical | System audit. |
| `runtime_decisions` | 1k+ | runtime only | keep-db-canonical | Router decisions. |
| `observability_events` | 100k+ | runtime only | keep-db-canonical | Obs backbone (0009). |
| `observability_counter_snapshots` | small | derived | keep-db-canonical | Counter snapshots. |
| `slo_aggregates` | small | derived | keep-db-canonical | SLO rollups (0015). |
| `slo_alert_cooldowns` | small | runtime only | keep-db-canonical | Alert debounce. |
| `api_friction_events` | 1k+ | runtime only | keep-db-canonical | Friction signal stream. |
| `api_friction_issues` | 100 | runtime only | keep-db-canonical | Friction rollups. |
| `pending_dm_replies` | small | runtime only | keep-db-canonical | DM queue. |
| `rate_limit_cache` | small | runtime only | keep-db-canonical | Rate limiter state. |
| `session_termination_events` | 10k | runtime only | keep-db-canonical | Termination ledger. |
| `deferred_hooks` | small | runtime only | keep-db-canonical | Hook queue. |
| `skill_usage` | 10k+ | runtime only | keep-db-canonical | Skill telemetry (skills metadata is file-canonical, usage is runtime). |
| `memento_feedback_turn_stats` | 1k | runtime only | keep-db-canonical | Feedback counters. |
| `turn_analytics_monthly_aggregate` | small | derived | keep-db-canonical | Monthly rollup of `turns` (0016). |
| `kv_meta` | 100 | runtime only | keep-db-canonical | KV store (migrating callers to `agentdesk.kv.*` per #1007). |

**Total audited: 52 tables.**

## Recommended actions summary

### file-canonical → remove from DB (3 tables)

- `offices`, `departments`, `office_agents` — derivable in-memory from
  `config/agentdesk.yaml`. No runtime writers found.

### materialized-view with startup sync + readonly guard (4 tables)

- `agents` — yaml drives identity/routing columns; runtime state
  (`status`, `xp`, `last_seen_at`) remains writable.
- `pipeline_stages` — default-pipeline.yaml re-seeds on each start; keep
  DB copy only for per-repo overrides, guard default columns as readonly.
- `skills` — filesystem walk is authoritative; `source_path` already
  references the file of record. Guard against non-sync writes.
- `github_repos` — yaml for repo list, runtime for `last_synced_at`.

### keep-db-canonical (45 tables)

All logs, ledgers, aggregates, outboxes, session/turn/meeting/dispatch
tables, kanban state, observability, SLO, friction, KV, and retention
aggregates. No file source exists or could reasonably exist.

## Follow-up work

1. File companion issues to:
   - Delete `offices` / `departments` / `office_agents` migrations or mark
     read-only via a new migration.
   - Add readonly column guards (trigger or application-level check) to
     the four materialized-view tables.
2. Consolidate on a single startup sync pass that asserts:
   yaml → `agents` + `pipeline_stages`, fs → `skills`, yaml →
   `github_repos`. Log drift counters via the observability backbone.
3. Out of scope here: `settings` has no dedicated table — settings YAML
   is read by `src/services/discord/settings.rs` and scalar runtime
   settings live in `kv_meta`. No duplication to resolve.
