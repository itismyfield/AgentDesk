# launchd → AgentDesk Routine Migration Plan (#2202 §2/§3)

This document tracks the migration of 12 launchd cron jobs to AgentDesk
routines. **The launchd plists are intentionally left in place during the
24h+ verification window**; routines and launchd both fire, the operator
de-duplicates by removing the launchd plist once parity is confirmed.

## Routine scripts

All routine scripts live under `routines/migrated-launchd/`. Each routine's
`tick()` returns `action: "agent"` with a prompt instructing the attached
agent to invoke the same `~/.local/bin/*.sh` (or repo `scripts/*.sh`) entry
point the launchd plist used. This preserves the original prompt body,
target Discord channel, skill path, and any side effects unchanged.

## The 12 jobs

| # | launchd label | routine script_ref | cron (KST) | agent_id | status |
|---|---|---|---|---|---|
| 1 | `com.itismyfield.agent-feedback-briefing` | `migrated-launchd/agent-feedback-briefing.js` | `5 19 * * *` | `ch-pmd` | parallel-running |
| 2 | `com.itismyfield.ai-integrated-briefing` | `migrated-launchd/ai-integrated-briefing.js` | `10 9,21 * * *` | `project-newsbot` | parallel-running |
| 3 | `com.itismyfield.banchan-day-reminder.prep` | `migrated-launchd/banchan-day-reminder-prep.js` | `0 8 * * *` | `family-routine` | parallel-running |
| 4 | `com.itismyfield.banchan-day-reminder.cook` | `migrated-launchd/banchan-day-reminder-cook.js` | `0 18 * * *` | `family-routine` | parallel-running |
| 5 | `com.itismyfield.cookingheart-daily-briefing` | `migrated-launchd/cookingheart-daily-briefing.js` | `0 19 * * *` | `project-agentdesk` | parallel-running |
| 6 | `com.itismyfield.family-morning-briefing.obujang` | `migrated-launchd/family-morning-briefing-obujang.js` | `30 6 * * *` | `personal-obiseo` | parallel-running |
| 7 | `com.itismyfield.family-morning-briefing.yohoejang` | `migrated-launchd/family-morning-briefing-yohoejang.js` | `31 6 * * *` | `personal-yobiseo` | parallel-running |
| 8 | `com.itismyfield.memento-daily-report` | `migrated-launchd/memento-daily-report.js` | `0 9 * * *` | **TODO** | scripts-only (not attached) |
| 9 | `com.itismyfield.memento-hygiene` | `migrated-launchd/memento-hygiene.js` | `0 6 * * *` | **TODO** | scripts-only (not attached) |
| 10 | `com.itismyfield.memory-merge` | `migrated-launchd/memory-merge.js` | `0 6 * * *` | **TODO** | scripts-only (not attached) |
| 11 | `com.itismyfield.token-daily-report` | `migrated-launchd/token-daily-report.js` | `0 7 * * *` | `token-manager` | parallel-running |
| 12 | `com.agentdesk.queue-stability-batch` | `migrated-launchd/queue-stability-batch.js` | `0 4 * * *` | `project-agentdesk` | parallel-running |

Jobs 8/9/10 have no agent owner yet (the issue marks them `(담당자 확정
필요)`). The routine scripts ship for staging, but **do not attach them via
`POST /api/routines` until the operator picks an `agent_id`**. The launchd
plists keep firing in the meantime — no regression.

## Routine cron timezone

Routines use `routines.default_timezone = "Asia/Seoul"` (see
`src/config.rs`). Cron expressions in the table above match the original
launchd `StartCalendarInterval` wall-clock times exactly. DST is not a
factor in Asia/Seoul (KST is UTC+9 year-round, no DST), so no off-by-one
hour shift is possible between launchd and the routine scheduler.

## Operator: attach routines (once dcserver is up)

Run the following on whichever node is the cluster leader. The
`agentfactory` workspace (or any workspace with a checked-out repo that
includes `routines/migrated-launchd/`) must be deployed before the script
loader will see the new files.

```bash
REL_PORT="${AGENTDESK_REL_PORT:-8791}"
API="http://127.0.0.1:${REL_PORT}"

# Job 1 — agent-feedback-briefing
curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/agent-feedback-briefing.js",
  "name": "agent-feedback-briefing",
  "agent_id": "ch-pmd",
  "execution_strategy": "fresh",
  "schedule": "5 19 * * *",
  "timeout_secs": 1800
}'

# Job 2 — ai-integrated-briefing
curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/ai-integrated-briefing.js",
  "name": "ai-integrated-briefing",
  "agent_id": "project-newsbot",
  "execution_strategy": "fresh",
  "schedule": "10 9,21 * * *",
  "timeout_secs": 1800
}'

# Job 3 — banchan-day-reminder-prep
curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/banchan-day-reminder-prep.js",
  "name": "banchan-day-reminder-prep",
  "agent_id": "family-routine",
  "execution_strategy": "fresh",
  "schedule": "0 8 * * *",
  "timeout_secs": 900
}'

# Job 4 — banchan-day-reminder-cook
curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/banchan-day-reminder-cook.js",
  "name": "banchan-day-reminder-cook",
  "agent_id": "family-routine",
  "execution_strategy": "fresh",
  "schedule": "0 18 * * *",
  "timeout_secs": 900
}'

# Job 5 — cookingheart-daily-briefing
curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/cookingheart-daily-briefing.js",
  "name": "cookingheart-daily-briefing",
  "agent_id": "project-agentdesk",
  "execution_strategy": "fresh",
  "schedule": "0 19 * * *",
  "timeout_secs": 1800
}'

# Job 6 — family-morning-briefing-obujang
curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/family-morning-briefing-obujang.js",
  "name": "family-morning-briefing-obujang",
  "agent_id": "personal-obiseo",
  "execution_strategy": "fresh",
  "schedule": "30 6 * * *",
  "timeout_secs": 1800
}'

# Job 7 — family-morning-briefing-yohoejang
curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/family-morning-briefing-yohoejang.js",
  "name": "family-morning-briefing-yohoejang",
  "agent_id": "personal-yobiseo",
  "execution_strategy": "fresh",
  "schedule": "31 6 * * *",
  "timeout_secs": 1800
}'

# Job 11 — token-daily-report
curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/token-daily-report.js",
  "name": "token-daily-report",
  "agent_id": "token-manager",
  "execution_strategy": "fresh",
  "schedule": "0 7 * * *",
  "timeout_secs": 1800
}'

# Job 12 — queue-stability-batch
curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/queue-stability-batch.js",
  "name": "queue-stability-batch",
  "agent_id": "project-agentdesk",
  "execution_strategy": "fresh",
  "schedule": "0 4 * * *",
  "timeout_secs": 3600
}'

# Jobs 8, 9, 10 — DO NOT ATTACH until agent_id is set.
# When ready, replace AGENT_ID below and POST.
#
# curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
#   "script_ref": "migrated-launchd/memento-daily-report.js",
#   "name": "memento-daily-report",
#   "agent_id": "AGENT_ID",
#   "execution_strategy": "fresh",
#   "schedule": "0 9 * * *",
#   "timeout_secs": 1800
# }'
```

## Cross-leader prerequisite — script availability

**All 12 shell entrypoints currently live only on mac-mini** under
`/Users/itismyfield/.local/bin/*.sh`; the §3 entrypoint
`scripts/queue-stability-batch.sh` is in this repo and is present
wherever the workspace is deployed. Routines invoke the absolute path,
so a routine that fires while the `routine-runtime` lease is held by a
node missing the script will fail.

Before attaching any of jobs 1–11, the operator must do **one** of:

- (recommended) `rsync -av mac-mini:/Users/itismyfield/.local/bin/{agent-feedback-briefing,ai-integrated-briefing,banchan-day-reminder-prep,banchan-day-reminder-cook,cookingheart-daily-briefing,family-morning-briefing-obujang,family-morning-briefing-yohoejang,memento-daily-report,memento-hygiene,memory-merge,token-daily-report,run-claude-message-job}.sh /Users/itismyfield/.local/bin/`
  on every node eligible to hold the `routine-runtime` lease (today:
  mac-book), and confirm `ls -l ~/.local/bin/*.sh` matches on both
  hosts; **or**
- pin the `routine-runtime` worker to mac-mini for the duration of the
  parallel-run window via cluster config (`execution_scope` /
  preferred-leader pin) so only mac-mini ever holds the lease until the
  scripts are mirrored.

After the §1 lease-succession bug fix lands, the routine system is
**capable** of running these jobs from either leader; the entrypoints
just have to be present on the leader at fire time. Until the scripts
are moved into the repo (or `~/.adk/release/bin/` and deployed via
`adk-release`), this is a host-local dependency the operator must keep
in sync.

## Verification window (≥24 hours)

Because jobs 1, 2, 5, 6, 7, 11 send Discord messages, the operator
**must avoid true parallel-running** for those — the recipient would see
two copies of every briefing. Use the **stage-paused → cutover**
protocol instead:

### Stage-paused → cutover protocol (jobs with Discord side effects: 1, 2, 5, 6, 7, 11)

1. POST `/api/routines` to create each row (per the attach commands
   above).
2. Immediately `POST /api/routines/<id>/pause` so the routine is
   registered but does not fire. The launchd plist remains the sole
   sender.
3. On the cutover day for each job, SSH mac-mini and run
   `launchctl bootout user/$(id -u)/<launchd-label>` to stop launchd
   firing **for that label only**. Do not delete the plist file yet.
4. `POST /api/routines/<id>/resume` to enable the routine.
5. Watch `GET /api/routines/<id>/runs?limit=10` and the Discord target
   for the next scheduled fire to confirm the routine sends exactly one
   message with the same payload the launchd plist used to send.
6. After 24h clean operation, delete the plist file:
   `rm ~/Library/LaunchAgents/<launchd-label>.plist`. Rollback is no
   longer one-step after this; see Rollback below.

### True parallel-run (idempotent jobs: 3, 4, 12)

Jobs 3/4 are calendar-gated (`NO_REPLY` on non-반찬데이 days), and job
12 is idempotent (skips if a run is active/pending/paused). These can
parallel-run safely:

1. Attach (`POST /api/routines`) — routine starts firing immediately.
2. Watch `GET /api/routines/<id>/runs?limit=10` and the relevant
   channel/queue for parity with the launchd job.
3. After 24h, `launchctl bootout` + `rm` the plist on mac-mini.

### Jobs 8/9/10 — TODO agent_id

Do not attach these until the operator picks an `agent_id`. The launchd
plists keep firing in the meantime. Once the owner is chosen, follow
the stage-paused → cutover protocol (these jobs probably also write
external state, so safer than true parallel-run).

### Per-routine observability

Use `GET /api/routines/<id>/runs?limit=10` for each attached routine
(the documented `/api/routines/runs/search` endpoint requires a
non-empty `q` parameter, so the empty-`q` listing approach does not
work). Also use `GET /api/routines/metrics?agent_id=<id>` for
aggregate counts.

## Rollback

The rollback path **before** plist removal is one-step. After plist
removal, rollback requires re-loading the plist on mac-mini.

### Before plist removal (rollback = single API call)

1. `curl -sf "$API/api/routines/<id>/pause" -X POST` — the routine
   stops firing.
2. Verify the routine is paused: `curl -sf "$API/api/routines/<id>"`
   and check `"status": "paused"`.
3. The launchd plist (still loaded) continues to fire uninterrupted —
   the system is back to launchd-only.
4. If the routine row should be removed entirely:
   `curl -sf "$API/api/routines/<id>/detach" -X POST` (idempotent).

Note: there is **no** PATCH-status code path; do not try
`PATCH /api/routines/<id>` with `{"status":"paused"}` — the API
ignores unknown fields silently. Always use the dedicated
`/pause` / `/resume` / `/detach` subroutes.

### After plist removal (rollback requires re-load)

1. SSH mac-mini.
2. If the plist file was kept somewhere (recommended: move it to
   `~/Library/LaunchAgents.disabled/` instead of `rm`), copy it back to
   `~/Library/LaunchAgents/` and `launchctl bootstrap user/$(id -u)
   ~/Library/LaunchAgents/<label>.plist`. Otherwise restore from this
   repo's recorded plist content (see the issue body and this doc's
   schedule table).
3. `POST /api/routines/<id>/pause` so launchd is the sole sender
   again.

## Cross-leader correctness

Routines run on whichever node holds the `routine-runtime` leader-only
worker lease (see issue #2202 §1). After the §1 fix, lease succession
re-spawns `routine-runtime` on the new leader, so the migrated jobs fire
regardless of which physical node (mac-mini or mac-book) is leader at
schedule time — unlike launchd, which only fires on the node where the
plist is loaded (currently mac-mini). This is the principal reliability
gain of the migration **once the entrypoint scripts are mirrored to
every eligible leader** (see Cross-leader prerequisite above).
