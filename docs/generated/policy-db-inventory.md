# Policy raw-DB inventory (manually maintained)

Manual inventory of `agentdesk.db.query/execute` usage under `policies/`. This
doc is **not** produced by `scripts/generate_inventory_docs.py`; it is refreshed
by hand from the reproducible command below.

Classification key:
- **read**: SELECT via `agentdesk.db.query` on non-`kv_meta` tables.
- **mutation**: `agentdesk.db.execute` on non-`kv_meta` tables (UPDATE/INSERT/DELETE).
- **kv**: any access (query or execute) targeting `kv_meta` table — should migrate to `agentdesk.kv.*`.

## Reproducible command

To enumerate every raw-DB callsite (file:line) and recount the totals below:

```bash
# All raw-DB callsites under policies/ (excludes test fixtures):
grep -rnE "agentdesk\.db\.(query|execute)" policies/ --include='*.js' | grep -v '/__tests__/'

# Totals by op (keeps file paths so the __tests__ filter works, then strips them):
grep -rnE "agentdesk\.db\.(query|execute)" policies/ --include='*.js' \
  | grep -v '/__tests__/' \
  | grep -oE "agentdesk\.db\.(query|execute)" | sort | uniq -c
```

`agentdesk.db.query` is a SELECT/read; `agentdesk.db.execute` is a
mutation (UPDATE/INSERT/DELETE). To split out `kv_meta` (kv) traffic that should
move to `agentdesk.kv.*`, grep the matched lines for the `kv_meta` table name.

## Totals

Measured on branch head with the command above (excludes `policies/__tests__/`):

| op | count |
|---|---|
| `agentdesk.db.query` (read) | 127 |
| `agentdesk.db.execute` (mutation) | 57 |
| **total** | **184** |

These 184 callsites are spread across 30 policy files under `policies/`. One
additional callsite lives in `policies/__tests__/` and is excluded from the
migration debt total.

## Migration slice notes

- **First slice**: `policies/ci-recovery.js` — all 8 raw-DB callsites were
  migrated to the `agentdesk.ciRecovery.*` typed facade
  (`setBlockedReason`, `getCardStatus`, `getReworkCardInfo`, `listWaitingForCi`).
  The file now has zero `agentdesk.db.*` callsites and carries
  `// typed-facade-slice:start ci-recovery` / `:end` markers to flag regressions.
- **Capability-manifest first rollout**: `policies/timeouts/active-monitor.js`,
  `policies/review-automation.js`, and `policies/merge-automation.js` now have
  adjacent `*.cap.yaml` manifests in `db.raw_sql.mode: legacy` with pinned
  `no_silent_growth` baselines.
- **Next candidates (mutation-heavy)**: `review-automation.js`,
  `merge-automation.js`, `kanban-rules.js`, and the `auto-queue` lib modules
  (`lib/auto-queue-phase-gate.js`, `lib/auto-queue-lifecycle.js`).
- **Escape hatch**: callers that still require legacy `agentdesk.db.*` must
  annotate the SQL with
  `/* legacy-raw-db: policy=<name> capability=<intent> source_event=<hook> */`
  so the audit log at `policy.raw_db_audit` captures
  `policy_name, capability, sql_category, source_event` (see
  `src/engine/ops/db_ops.rs` — `emit_raw_db_audit` / `parse_raw_db_marker`).
