# Storage Retention Policy

Vector-by-vector retention, deletion, and archive rules for the AgentDesk runtime,
plus the operator runbook for initial cleanup and the post-rollout observation
report. Scope covers disk-backed artifacts (`target/`, worktrees, log files, hang
dumps) and Postgres-backed event tables.

Tracking issue: [#1094](https://github.com/itismyfield-org/agentdesk/issues/1094)
("storage 보존 정책 문서 + 최초 회수 + 2주 관찰 리포트").

Prerequisite context:

- [#1091 / 909-2] Dynamic maintenance scheduler (`register_maintenance_job`).
- [#1092 / 909-3] Storage maintenance jobs wired: `target_sweep`,
  `worktree_orphan_sweep`, `hang_dump_cleanup`.
- [#1093 / 909-4] DB retention job wired: `db_retention`.
- Cross-reference: [`docs/ci/sccache-setup.md`](ci/sccache-setup.md) — why
  `target/` bloats and how sccache dampens rebuild cost post-sweep.
- Cross-reference: [`docs/ops/codex-campaign-prompt-2026-04-24-r4.md`](ops/codex-campaign-prompt-2026-04-24-r4.md) —
  campaign runbook that called 909-5 out as the closing deliverable.

---

## 1. Policy Overview

AgentDesk produces four durable storage vectors. Each has a distinct *producer*,
*growth pattern*, and *acceptable steady-state ceiling*. The retention policy for
each vector is owned by exactly one maintenance job; nothing else is permitted to
reach into these paths during normal operation.

Steady-state targets (post-cleanup):

| Vector              | Steady-state ceiling | Prior observed peak |
|---------------------|----------------------|---------------------|
| `target/`           | ~10–20 GB            | 178 GB              |
| Worktrees           | < 5 GB               | unbounded (orphans) |
| Log files + dumps   | < 2 GB               | grows weekly        |
| Postgres event data | < 1 GB               | grows per-dispatch  |

Total runtime disk budget after 909-3/909-4/909-5 land: **≤ 30 GB** on a healthy
host — small enough to live inside the SSD comfort zone of a Mac mini deployment.

---

## 2. Retention Matrix

Each row is a (vector × action × job) tuple. If a vector is not in this table,
**nothing** touches it automatically — add a row before writing a sweeper.

| Vector                                    | Retention                                   | Action                                                      | Maintenance Job                   | Cadence  |
|-------------------------------------------|---------------------------------------------|-------------------------------------------------------------|-----------------------------------|----------|
| `target/` (Cargo build outputs)           | 30 days since last access                   | `cargo sweep --time 30`; 50 GB escape hatch                  | `storage.target_sweep`            | Monthly  |
| `~/.adk/release/worktrees/<id>/`          | Until the owning dispatch is no longer `pending`/`dispatched` | `git worktree remove --force` + `rm -rf` on orphans           | `storage.worktree_orphan_sweep`   | Hourly   |
| `logs/adk-hang-*.txt` (hang diagnostics)  | 14 days                                     | Delete files older than the window                           | `storage.hang_dump_cleanup`       | Weekly   |
| PG `message_outbox` (status='sent')       | 7 days                                      | `DELETE`                                                     | `storage.db_retention`            | Weekly   |
| PG `auto_queue_entries` (status='completed') | 30 days                                  | `DELETE`                                                     | `storage.db_retention`            | Weekly   |
| PG `agent_quality_event`                  | 90 days                                     | Monthly aggregate insert into summary table, then `DELETE`   | `storage.db_retention`            | Weekly   |
| PG `session_transcripts`                  | 90 days                                     | Copy to archive table, then `DELETE`                         | `storage.db_retention`            | Weekly   |
| PG `task_dispatches`                      | 90 days                                     | Monthly aggregate insert, then `DELETE`                      | `storage.db_retention`            | Weekly   |
| PG `kanban_cards`                         | **Forever**                                 | None — intentional permanent history                         | —                                 | —        |
| `dcserver.stdout.log` / `dcserver.stderr.log` | TBD                                     | Deferred to a follow-up PR (requires `tracing-appender::rolling`) | *(not yet registered)*            | —        |

Job source locations under `src/services/maintenance/jobs/`:
`target_sweep.rs`, `worktree_orphan_sweep.rs`, `hang_dump_cleanup.rs`,
`db_retention.rs`, `mod.rs::spawn_storage_maintenance_jobs`.

Config knobs (all live in `Config::default_runtime()` per job):

- `target_sweep.sweep_time_days` — default 30.
- `target_sweep.disk_threshold_bytes` — default 50 GB.
- `hang_dump_cleanup.max_age` — default 14 days.
- `worktree_orphan_sweep.dry_run` — default false in production, true in tests.

---

## 3. Initial Cleanup Procedure (One-Time, Manual Ops)

When 909-5 rolls out on a host that has been running without automated sweepers,
`target/` has historically grown to **178 GB**. The automated monthly
`storage.target_sweep` respects `--time 30`, which means it will only free
artifacts older than 30 days — so the first tick will under-recover on a fresh
install. Run the manual cleanup below **once** per host, then let the monthly job
take over.

### 3.1 Prerequisites

```bash
# Install cargo-sweep if not already present.
cargo install cargo-sweep

# Verify install.
cargo sweep --version
```

### 3.2 Dry-run inventory (recommended first)

```bash
# Measure current target/ size before touching anything.
du -sh ~/.adk/release/workspaces/agentdesk/target/

# Dry-run cargo sweep with an aggressive window; see what WOULD be removed.
cd ~/.adk/release/workspaces/agentdesk
cargo sweep --time 0 --dry-run
```

Record both numbers in the 2-week observation report (§5) under "baseline".

### 3.3 Execute cleanup

Choose one — `cargo sweep --time 0` is the lighter hammer, `cargo clean` is the
full reset.

**Option A — Full reset (`cargo clean`).** Fastest reclamation, but forces a
cold rebuild on the next `cargo build`. Use when the host has sccache wired
up correctly (see `docs/ci/sccache-setup.md`); cold rebuild will repopulate
from the sccache cache and complete in a fraction of the uncached cost.

```bash
cd ~/.adk/release/workspaces/agentdesk
cargo clean

# Confirm.
du -sh target/ 2>/dev/null || echo "target/ removed"
```

**Option B — Incremental sweep (`cargo sweep --time 0`).** Keeps recently-used
artifacts, removes everything else. Safer when sccache is not configured — the
next build only rebuilds the genuinely-evicted deps.

```bash
cd ~/.adk/release/workspaces/agentdesk
cargo sweep --time 0

du -sh target/
```

### 3.4 Verify steady state

After cleanup, one round-trip build + typical dev activity should stabilize
`target/` in the **~10–20 GB** band. Campaign worktrees inherit
`.cargo/config.toml`, which uses `sccache` and disables Cargo incremental
builds so one-off debug/test runs do not leave a separate
`target/debug/incremental/` cache in every worktree.

```bash
# Force a full rebuild to seed the new baseline.
cargo build --release

du -sh target/
# Expected: ~10 GB (release-only) to ~20 GB (release + debug + tests).
```

If post-rebuild size exceeds **30 GB**, something is wrong — see §6.

### 3.5 Enable automated maintenance

The scheduler starts registering jobs automatically via
`spawn_storage_maintenance_jobs` on server boot (`src/server/boot.rs`). No
operator action required beyond restarting `dcserver` after the deploy. Verify:

```bash
curl -s http://localhost:8791/api/cron-jobs | jq '.[] | select(.name | startswith("storage."))'
```

Each of `storage.target_sweep`, `storage.worktree_orphan_sweep`,
`storage.hang_dump_cleanup`, `storage.db_retention` should be present with
`enabled: true` and a `schedule.every_ms` matching its cadence row above.

---

## 4. Parallel Vectors (Worktrees, Logs, PG)

The `target/` cleanup above is the only vector with a documented manual
intervention — the others either self-heal within an hour
(`worktree_orphan_sweep`) or accumulate slowly enough that waiting for the first
weekly tick is cheaper than a manual step. Still, on the very first deploy:

- **Worktrees**: list anything under `~/.adk/release/worktrees/` that doesn't
  correspond to an active dispatch and delete it. The hourly orphan sweep will
  catch residuals, but a one-shot manual clean accelerates recovery:

    ```bash
    ls -la ~/.adk/release/worktrees/
    # Cross-check against:
    psql -c "select id, status, cwd from task_dispatches where status in ('pending','dispatched')"
    # Delete confirmed orphans with `git worktree remove --force` from the main repo.
    ```

- **Log files**: `adk-hang-*.txt` older than 14 days can be deleted immediately
  with a one-liner; the weekly job will keep it tidy afterwards.

    ```bash
    find ~/.adk/release/logs -name 'adk-hang-*.txt' -mtime +14 -delete
    ```

- **Postgres**: `storage.db_retention` runs weekly and is self-bootstrapping.
  No manual DELETEs recommended — if you need to force it, trigger via the
  cron-jobs API (`POST /api/cron-jobs/run/storage.db_retention`).

---

## 5. Two-Week Observation Report (Template)

Fill this in once at **T + 3 days**, **T + 7 days**, **T + 14 days** after
initial cleanup. Commit the filled copy to `docs/reports/storage-retention-<date>.md`
and link it from the tracking issue.

```
# Storage Retention Observation Report — <YYYY-MM-DD>

- Host: <mac-mini | mac-book | ...>
- Deploy commit: <sha>
- Initial cleanup commit: <sha>
- Report window start: <YYYY-MM-DD HH:MM KST>
- Observer: <agent/person>

## Baseline (immediately before cleanup)

| Vector                  | Size       | Notes       |
|-------------------------|------------|-------------|
| target/                 | <GB>       | TBD         |
| worktrees/              | <GB>       | TBD         |
| logs/ adk-hang-*.txt    | <MB>       | TBD         |
| PG total (all tables)   | <GB>       | TBD         |

## T + 3 days

| Vector                  | Size       | Delta vs baseline |
|-------------------------|------------|-------------------|
| target/                 | <GB>       | TBD               |
| worktrees/              | <GB>       | TBD               |
| logs/                   | <MB>       | TBD               |
| PG total                | <GB>       | TBD               |

Job run counts (from /api/cron-jobs):

- storage.target_sweep: <n> runs, <n> errors
- storage.worktree_orphan_sweep: <n> runs, <n> errors
- storage.hang_dump_cleanup: <n> runs, <n> errors
- storage.db_retention: <n> runs, <n> errors

## T + 7 days

(same shape as T + 3 days)

## T + 14 days

(same shape as T + 3 days)

## Findings

- TBD — did target/ stay under 30 GB?
- TBD — any orphan worktree accumulation?
- TBD — PG retention hit-rate vs expected.

## Action Items

- TBD
```

Put the filled reports in `docs/reports/` alongside
`cost-efficiency-908.md`; the file naming convention is
`storage-retention-<yyyy-mm-dd>.md`.

---

## 6. Troubleshooting

### 6.1 `target/` still grows past 30 GB after cleanup

Possible causes:

1. **sccache not hitting cache.** Check `sccache --show-stats`. If "Compile
   requests" is high but "Cache hits" is low, the cache isn't warmed. See
   `docs/ci/sccache-setup.md` §4.
2. **`storage.target_sweep` not running.** Confirm via `/api/cron-jobs` that the
   job is `enabled: true` and `last_run_at` is within the last 30 days. If
   `last_status = "error"`, inspect logs for "target_sweep completed" entries —
   the tracing line includes `cargo_sweep_available` and `invoked_sweep`, which
   localize the failure immediately.
3. **`cargo-sweep` binary missing on the host.** The job logs a warning and
   returns `Ok(())` when the binary isn't on PATH — `which cargo-sweep` to
   verify; reinstall via `cargo install cargo-sweep` if needed.

### 6.2 Worktrees pile up under `~/.adk/release/worktrees/`

- Check that the hourly job is running: `/api/cron-jobs | grep worktree_orphan_sweep`.
- If Postgres is down, the job self-skips to avoid false-positive deletes —
  you'll see a `pg_unavailable = true` log entry. Restore PG first.
- The orphan detector compares directory paths against
  `sessions.cwd` for dispatches with `status IN ('pending','dispatched')`. A
  worktree stuck in `dispatched` for hours will block sweep until the dispatch
  transitions — cancel or time-out the dispatch to unblock.

### 6.3 `storage.db_retention` reports zero rows affected

Expected on a fresh DB. If persisting beyond a week:

- Verify `migrations/postgres/0016_retention_tables.sql` ran (creates the
  archive/aggregate tables referenced by the job).
- Confirm Postgres pool is wired — `mod.rs` logs
  `"storage.db_retention skipped (postgres pool unavailable)"` when the pool is
  `None`.
- Force a dry-run to inspect candidate rows:
  `POST /api/cron-jobs/run/storage.db_retention?dry_run=1` (if exposed) or call
  `db_retention_job(&pool, true)` from an admin shell.

### 6.4 Host disk keeps growing despite all jobs running green

Something is writing **outside** the four tracked vectors. Candidates:

- `dcserver.stdout.log` / `dcserver.stderr.log` — log rotation is an explicit
  deferred item (see row 10 in §2). Until that lands, rotate manually:
  `> dcserver.stdout.log` while the server is restarted.
- MCP server caches (memento, codex-helper) — check `~/.cache/` and
  `~/Library/Caches/`.
- Cargo registry under `~/.cargo/registry/` — bounded but can reach several GB;
  run `cargo cache --autoclean` quarterly.

Report novel vectors back to the tracking issue so the retention matrix above
gets a new row.

---

## 7. Related Documents

- [`docs/ci/sccache-setup.md`](ci/sccache-setup.md) — sccache config that keeps
  post-sweep rebuild cost predictable.
- [`docs/ops/codex-campaign-prompt-2026-04-24-r4.md`](ops/codex-campaign-prompt-2026-04-24-r4.md) —
  campaign 909 runbook (909-5 = this doc).
- [`docs/source-of-truth.md`](source-of-truth.md) — §retention for the
  long-form rationale behind the 7/30/90-day horizons.
- [`docs/reports/cost-efficiency-908.md`](reports/cost-efficiency-908.md) —
  parallel cost-observability report template that this one mirrors.
