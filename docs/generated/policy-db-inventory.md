# Policy raw-DB inventory (#1007)
Generated: inventory of `agentdesk.db.query/execute` usage under `policies/`.
Classification key:
- **read**: SELECT via `agentdesk.db.query` on non-`kv_meta` tables.
- **mutation**: `agentdesk.db.execute` on non-`kv_meta` tables (UPDATE/INSERT/DELETE).
- **kv**: any access (query or execute) targeting `kv_meta` table — should migrate to `agentdesk.kv.*`.
- **test**: callsite inside test fixtures / `__tests__/`.

## Totals

| category | count |
|---|---|
| read | 112 |
| mutation | 36 |
| kv | 41 |
| test | 0 |

Total remaining raw-db callsites: **189** (post first migration slice).
Already migrated via this slice: **8** (see `ci-recovery.js` via `agentdesk.ciRecovery`).

## Per-file breakdown

| file | read | mutation | kv | test | total |
|---|---|---|---|---|---|
| `policies/00-escalation.js` | 3 | 2 | 9 | 0 | 14 |
| `policies/00-pr-tracking.js` | 6 | 1 | 2 | 0 | 9 |
| `policies/auto-queue.js` | 26 | 2 | 0 | 0 | 28 |
| `policies/kanban-rules.js` | 13 | 7 | 1 | 0 | 21 |
| `policies/lib/timeouts-helpers.js` | 4 | 1 | 0 | 0 | 5 |
| `policies/merge-automation.js` | 10 | 5 | 4 | 0 | 19 |
| `policies/pipeline.js` | 2 | 1 | 0 | 0 | 3 |
| `policies/review-automation.js` | 22 | 9 | 0 | 0 | 31 |
| `policies/timeouts.js` | 0 | 0 | 1 | 0 | 1 |
| `policies/timeouts/active-monitor.js` | 4 | 2 | 14 | 0 | 20 |
| `policies/timeouts/card-timeouts.js` | 2 | 1 | 0 | 0 | 3 |
| `policies/timeouts/dispatch-maintenance.js` | 3 | 3 | 1 | 0 | 7 |
| `policies/timeouts/idle-kill.js` | 2 | 0 | 0 | 0 | 2 |
| `policies/timeouts/long-turn-monitor.js` | 2 | 0 | 6 | 0 | 8 |
| `policies/timeouts/orphan-dispatch.js` | 1 | 0 | 1 | 0 | 2 |
| `policies/timeouts/reconciliation.js` | 3 | 1 | 2 | 0 | 6 |
| `policies/timeouts/review-auto-accept.js` | 3 | 1 | 0 | 0 | 4 |
| `policies/timeouts/review-timeouts.js` | 4 | 0 | 0 | 0 | 4 |
| `policies/timeouts/workspace-branch-guard.js` | 2 | 0 | 0 | 0 | 2 |

## Full callsite listing

| file:line | category | sql kind |
|---|---|---|
| `policies/pipeline.js:25` | read | select |
| `policies/pipeline.js:31` | read | select |
| `policies/pipeline.js:36` | mutation | update |
| `policies/00-pr-tracking.js:19` | read | select |
| `policies/00-pr-tracking.js:28` | read | select |
| `policies/00-pr-tracking.js:38` | mutation | insert |
| `policies/00-pr-tracking.js:95` | kv | select |
| `policies/00-pr-tracking.js:108` | kv | select |
| `policies/00-pr-tracking.js:129` | read | select |
| `policies/00-pr-tracking.js:139` | read | select |
| `policies/00-pr-tracking.js:157` | read | unknown |
| `policies/00-pr-tracking.js:198` | read | select |
| `policies/00-escalation.js:36` | read | select |
| `policies/00-escalation.js:75` | mutation | update |
| `policies/00-escalation.js:139` | read | select |
| `policies/00-escalation.js:148` | mutation | update |
| `policies/00-escalation.js:240` | read | select |
| `policies/00-escalation.js:271` | kv | select |
| `policies/00-escalation.js:285` | kv | upsert |
| `policies/00-escalation.js:308` | kv | select |
| `policies/00-escalation.js:314` | kv | delete |
| `policies/00-escalation.js:318` | kv | delete |
| `policies/00-escalation.js:324` | kv | delete |
| `policies/00-escalation.js:328` | kv | select |
| `policies/00-escalation.js:350` | kv | upsert |
| `policies/00-escalation.js:354` | kv | delete |
| `policies/lib/timeouts-helpers.js:77` | read | select |
| `policies/lib/timeouts-helpers.js:149` | read | select |
| `policies/lib/timeouts-helpers.js:159` | read | select |
| `policies/lib/timeouts-helpers.js:216` | read | select |
| `policies/lib/timeouts-helpers.js:225` | mutation | update |
| `policies/merge-automation.js:52` | read | select |
| `policies/merge-automation.js:73` | mutation | update |
| `policies/merge-automation.js:178` | kv | select |
| `policies/merge-automation.js:188` | kv | upsert |
| `policies/merge-automation.js:196` | kv | delete |
| `policies/merge-automation.js:235` | read | select |
| `policies/merge-automation.js:252` | read | select |
| `policies/merge-automation.js:374` | read | select |
| `policies/merge-automation.js:400` | read | select |
| `policies/merge-automation.js:927` | mutation | update |
| `policies/merge-automation.js:958` | mutation | update |
| `policies/merge-automation.js:1086` | mutation | update |
| `policies/merge-automation.js:1168` | read | select |
| `policies/merge-automation.js:1178` | read | select |
| `policies/merge-automation.js:1396` | read | select |
| `policies/merge-automation.js:1411` | read | select |
| `policies/merge-automation.js:1632` | mutation | insert |
| `policies/merge-automation.js:1943` | kv | select |
| `policies/merge-automation.js:2257` | read | select |
| `policies/timeouts/idle-kill.js:38` | read | select |
| `policies/timeouts/idle-kill.js:47` | read | select |
| `policies/timeouts/active-monitor.js:54` | kv | select |
| `policies/timeouts/active-monitor.js:60` | kv | delete |
| `policies/timeouts/active-monitor.js:67` | read | select |
| `policies/timeouts/active-monitor.js:83` | read | select |
| `policies/timeouts/active-monitor.js:88` | read | select |
| `policies/timeouts/active-monitor.js:99` | mutation | update |
| `policies/timeouts/active-monitor.js:111` | read | select |
| `policies/timeouts/active-monitor.js:129` | kv | delete |
| `policies/timeouts/active-monitor.js:166` | kv | delete |
| `policies/timeouts/active-monitor.js:172` | kv | delete |
| `policies/timeouts/active-monitor.js:178` | kv | select |
| `policies/timeouts/active-monitor.js:265` | mutation | insert |
| `policies/timeouts/active-monitor.js:273` | kv | upsert |
| `policies/timeouts/active-monitor.js:288` | kv | upsert |
| `policies/timeouts/active-monitor.js:292` | kv | upsert |
| `policies/timeouts/active-monitor.js:308` | kv | select |
| `policies/timeouts/active-monitor.js:313` | kv | select |
| `policies/timeouts/active-monitor.js:318` | kv | delete |
| `policies/timeouts/active-monitor.js:323` | kv | select |
| `policies/timeouts/active-monitor.js:331` | kv | delete |
| `policies/timeouts/dispatch-maintenance.js:31` | mutation | delete |
| `policies/timeouts/dispatch-maintenance.js:39` | read | select |
| `policies/timeouts/dispatch-maintenance.js:67` | read | select |
| `policies/timeouts/dispatch-maintenance.js:74` | mutation | delete |
| `policies/timeouts/dispatch-maintenance.js:84` | kv | select |
| `policies/timeouts/dispatch-maintenance.js:100` | mutation | insert |
| `policies/timeouts/dispatch-maintenance.js:117` | read | select |
| `policies/timeouts/workspace-branch-guard.js:31` | read | select |
| `policies/timeouts/workspace-branch-guard.js:36` | read | select |
| `policies/timeouts/orphan-dispatch.js:38` | kv | select |
| `policies/timeouts/orphan-dispatch.js:53` | read | select |
| `policies/timeouts/reconciliation.js:34` | kv | select |
| `policies/timeouts/reconciliation.js:39` | kv | delete |
| `policies/timeouts/reconciliation.js:46` | read | select |
| `policies/timeouts/reconciliation.js:58` | read | select |
| `policies/timeouts/reconciliation.js:81` | mutation | update |
| `policies/timeouts/reconciliation.js:127` | read | select |
| `policies/timeouts/review-timeouts.js:35` | read | select |
| `policies/timeouts/review-timeouts.js:59` | read | select |
| `policies/timeouts/review-timeouts.js:79` | read | select |
| `policies/timeouts/review-timeouts.js:102` | read | select |
| `policies/timeouts/review-auto-accept.js:38` | read | select |
| `policies/timeouts/review-auto-accept.js:59` | read | select |
| `policies/timeouts/review-auto-accept.js:66` | read | select |
| `policies/timeouts/review-auto-accept.js:79` | mutation | insert |
| `policies/timeouts/card-timeouts.js:35` | read | select |
| `policies/timeouts/card-timeouts.js:69` | mutation | update |
| `policies/timeouts/card-timeouts.js:89` | read | select |
| `policies/timeouts/long-turn-monitor.js:53` | kv | select |
| `policies/timeouts/long-turn-monitor.js:59` | read | select |
| `policies/timeouts/long-turn-monitor.js:69` | read | select |
| `policies/timeouts/long-turn-monitor.js:84` | kv | upsert |
| `policies/timeouts/long-turn-monitor.js:91` | kv | select |
| `policies/timeouts/long-turn-monitor.js:103` | kv | delete |
| `policies/timeouts/long-turn-monitor.js:107` | kv | select |
| `policies/timeouts/long-turn-monitor.js:109` | kv | delete |
| `policies/review-automation.js:32` | read | select |
| `policies/review-automation.js:289` | read | select |
| `policies/review-automation.js:320` | read | select |
| `policies/review-automation.js:436` | read | select |
| `policies/review-automation.js:460` | mutation | update |
| `policies/review-automation.js:494` | read | select |
| `policies/review-automation.js:543` | read | select |
| `policies/review-automation.js:663` | read | select |
| `policies/review-automation.js:794` | read | select |
| `policies/review-automation.js:890` | read | select |
| `policies/review-automation.js:972` | mutation | update |
| `policies/review-automation.js:993` | mutation | update |
| `policies/review-automation.js:1000` | mutation | update |
| `policies/review-automation.js:1017` | read | select |
| `policies/review-automation.js:1027` | read | select |
| `policies/review-automation.js:1047` | read | select |
| `policies/review-automation.js:1110` | read | select |
| `policies/review-automation.js:1121` | read | select |
| `policies/review-automation.js:1126` | read | select |
| `policies/review-automation.js:1134` | read | select |
| `policies/review-automation.js:1147` | mutation | update |
| `policies/review-automation.js:1175` | mutation | update |
| `policies/review-automation.js:1184` | read | select |
| `policies/review-automation.js:1191` | mutation | update |
| `policies/review-automation.js:1209` | read | select |
| `policies/review-automation.js:1229` | read | select |
| `policies/review-automation.js:1254` | mutation | update |
| `policies/review-automation.js:1284` | read | select |
| `policies/review-automation.js:1309` | read | select |
| `policies/review-automation.js:1332` | mutation | update |
| `policies/review-automation.js:1339` | read | select |
| `policies/auto-queue.js:23` | read | select |
| `policies/auto-queue.js:34` | read | select |
| `policies/auto-queue.js:241` | read | select |
| `policies/auto-queue.js:283` | read | select |
| `policies/auto-queue.js:310` | read | select |
| `policies/auto-queue.js:321` | read | select |
| `policies/auto-queue.js:343` | read | select |
| `policies/auto-queue.js:553` | read | select |
| `policies/auto-queue.js:576` | read | select |
| `policies/auto-queue.js:598` | read | select |
| `policies/auto-queue.js:616` | read | select |
| `policies/auto-queue.js:714` | read | select |
| `policies/auto-queue.js:791` | read | select |
| `policies/auto-queue.js:800` | read | select |
| `policies/auto-queue.js:821` | mutation | update |
| `policies/auto-queue.js:835` | mutation | update |
| `policies/auto-queue.js:846` | read | select |
| `policies/auto-queue.js:919` | read | select |
| `policies/auto-queue.js:927` | read | select |
| `policies/auto-queue.js:970` | read | select |
| `policies/auto-queue.js:975` | read | select |
| `policies/auto-queue.js:1000` | read | select |
| `policies/auto-queue.js:1014` | read | select |
| `policies/auto-queue.js:1038` | read | select |
| `policies/auto-queue.js:1052` | read | select |
| `policies/auto-queue.js:1096` | read | select |
| `policies/auto-queue.js:1325` | read | select |
| `policies/auto-queue.js:1331` | read | select |
| `policies/timeouts.js:76` | kv | delete |
| `policies/kanban-rules.js:25` | read | select |
| `policies/kanban-rules.js:83` | read | select |
| `policies/kanban-rules.js:103` | mutation | update |
| `policies/kanban-rules.js:263` | read | select |
| `policies/kanban-rules.js:274` | read | select |
| `policies/kanban-rules.js:302` | read | select |
| `policies/kanban-rules.js:339` | kv | select |
| `policies/kanban-rules.js:351` | read | select |
| `policies/kanban-rules.js:364` | read | select |
| `policies/kanban-rules.js:388` | read | select |
| `policies/kanban-rules.js:408` | read | select |
| `policies/kanban-rules.js:476` | read | select |
| `policies/kanban-rules.js:499` | read | select |
| `policies/kanban-rules.js:533` | mutation | update |
| `policies/kanban-rules.js:569` | mutation | update |
| `policies/kanban-rules.js:593` | mutation | update |
| `policies/kanban-rules.js:610` | mutation | update |
| `policies/kanban-rules.js:679` | read | select |
| `policies/kanban-rules.js:713` | mutation | update |
| `policies/kanban-rules.js:733` | read | select |
| `policies/kanban-rules.js:764` | mutation | update |

## Migration slice notes

- **First slice (this change)**: `policies/ci-recovery.js` — all 8 raw-DB callsites migrated to `agentdesk.ciRecovery.*` typed facade (`setBlockedReason`, `getCardStatus`, `getReworkCardInfo`, `listWaitingForCi`).
- **Next candidates (mutation-heavy)**: `00-escalation.js` (14 hits, many `kv_meta` — consider `agentdesk.kv.*` first), `kanban-rules.js` (21), `merge-automation.js` (19).
- **Guard**: see `src/engine/ops/tests.rs::ci_recovery_slice_blocks_raw_db_reintroduction` — slice markers in `ci-recovery.js` prevent regressions.
- **Escape hatch**: callers that still require legacy `agentdesk.db.*` must annotate with `/* legacy-raw-db: policy=<name> capability=<intent> source_event=<hook> */` so the audit log at `policy.raw_db_audit` captures `policy_name, capability, sql_category, source_event`.
