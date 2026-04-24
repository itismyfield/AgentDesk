# Cost Efficiency Campaign 908 — Before/After Report

**Campaign**: `908` (Memory + Recall token-cost reduction)
**Tracking issue**: #1089 (908-7, this PR)
**Status**: template — populate metrics post-deployment.

This document captures the per-item cost-savings story for the 908 sub-tasks.
Each section pairs a short description with a before/after metric pair so the
campaign's full impact can be reviewed at a glance once data is collected.

> Numbers below are placeholders (`TBD`) until the maintenance job
> (`memory.memento_consolidation`) and the surrounding observability sinks
> have produced at least two full weekly cycles of telemetry.

## Methodology

- **Recall response size**: averaged across all `services::memory` calls
  during a 7-day window. Sourced from the `memento_recall_completed` and
  `memento_call_metrics_snapshot` observability streams.
- **Token throttle hits**: counted via `memento_throttle::note_*` events.
- **Fragment counts**: pulled from `memory_consolidate` payload
  (`before_count` / `after_count`) emitted by the new
  `memento_consolidation_completed` event.
- **Sample window**: report two adjacent windows — one immediately
  pre-deployment (control) and one ≥7 days post-deployment.

## 908-1 — Memento recall throttle (request dedup)

| Metric                         | Before (T-7d) | After (T+7d) | Δ |
|--------------------------------|---------------|--------------|---|
| Avg recall calls / agent / day | TBD           | TBD          | TBD |
| Dedup-hit rate (%)             | TBD           | TBD          | TBD |
| Avg recall response bytes      | TBD           | TBD          | TBD |

Notes: TBD.

## 908-2 — Memento `remember` fingerprint cache

| Metric                              | Before | After | Δ |
|-------------------------------------|--------|-------|---|
| `remember` calls / agent / day      | TBD    | TBD   | TBD |
| Dedup short-circuit rate (%)        | TBD    | TBD   | TBD |
| Memento egress bytes (remember/24h) | TBD    | TBD   | TBD |

Notes: TBD.

## 908-3 — Working / core memory line caps

| Metric                              | Before | After | Δ |
|-------------------------------------|--------|-------|---|
| Avg working-memory line count       | TBD    | TBD   | TBD |
| Avg core-memory line count          | TBD    | TBD   | TBD |
| Avg recall payload bytes            | TBD    | TBD   | TBD |

Notes: caps live in
`src/services/memory/memento.rs`
(`MAX_WORKING_MEMORY_LINES`, `MAX_MEMORY_LINES`, `MAX_SKIP_LINES`).

## 908-4 — Model output guard (16 KB cap)

| Metric                       | Before | After | Δ |
|------------------------------|--------|-------|---|
| Truncation events / day      | TBD    | TBD   | TBD |
| Avg memento payload bytes    | TBD    | TBD   | TBD |
| P99 memento payload bytes    | TBD    | TBD   | TBD |

Notes: guard constant `MEMENTO_MODEL_OUTPUT_MAX_BYTES = 16 * 1024`.

## 908-5 — Recall response cache reuse

| Metric                                  | Before | After | Δ |
|-----------------------------------------|--------|-------|---|
| Cache-hit rate on identical requests    | TBD    | TBD   | TBD |
| Avg recall latency (ms)                 | TBD    | TBD   | TBD |
| Memento recall calls / 24h              | TBD    | TBD   | TBD |

Notes: TBD.

## 908-6 — Throttle observability counters

| Metric                                    | Before | After | Δ |
|-------------------------------------------|--------|-------|---|
| `note_memento_remote_call` total / day    | TBD    | TBD   | TBD |
| `note_memento_dedup_hit` total / day      | TBD    | TBD   | TBD |
| Counter-derived $ savings (estimated)     | TBD    | TBD   | TBD |

Notes: counters surfaced via `memento_call_metrics_snapshot()`.

## 908-7 — Weekly memento consolidation (this PR)

| Metric                                      | Before (T-0) | After (T+7d) | Δ |
|---------------------------------------------|--------------|--------------|---|
| Total memento fragments                     | TBD          | TBD          | TBD |
| Avg recall response bytes                   | TBD          | TBD          | TBD |
| `memento_consolidation_completed.merged`    | n/a          | TBD          | TBD |
| `memento_consolidation_completed.savings`   | n/a          | TBD          | TBD |

The maintenance scheduler logs each run as a
`memento_consolidation_completed` observability event with
`before_count` / `after_count` / `merged_count` / `savings`.
Aggregate across at least two consecutive weekly runs before drawing
conclusions about steady-state savings.

## Aggregate impact (campaign rollup)

| Roll-up metric                                  | Before | After | Δ |
|-------------------------------------------------|--------|-------|---|
| Avg recall response bytes (all backends)        | TBD    | TBD   | TBD |
| Memento egress bytes / 24h (all tools)          | TBD    | TBD   | TBD |
| Estimated weekly memento $ cost                 | TBD    | TBD   | TBD |
| Estimated weekly $ savings vs. T-0 baseline     | TBD    | TBD   | TBD |

## How to refresh this report

1. Wait at least 7 days after the maintenance job has been live in
   production (`memory.memento_consolidation` visible in
   `/api/cron-jobs` with `state.last_status="ok"`).
2. Pull the relevant observability snapshots:
   - `memento_consolidation_completed` (this PR)
   - `memento_recall_completed`, `memento_call_metrics_snapshot`
3. Replace each `TBD` cell with the measured value and add a 1-line note
   under the table when the delta is non-trivial.
4. Link this document from the PR description for #1089 (908-7) and from
   any follow-up campaign reviews.
