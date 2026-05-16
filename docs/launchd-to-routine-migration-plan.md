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

## Verification window (≥24 hours)

1. After attach, watch `/api/routines/runs/search?q=&limit=50` and the
   target Discord channels for **each** of the 9 attached jobs to confirm
   the routine fires and produces the same payload as the launchd job.
2. For jobs 3/4 (banchan-day-reminder), verify that on a non-반찬데이 day
   the routine returns `NO_REPLY` (the skill's calendar-driven guard is
   intact). On the next 반찬데이, verify the message lands.
3. For jobs 6/7 (family morning briefings), the operator must visually
   confirm only **one** briefing reaches Discord (i.e. routine + launchd
   are producing the same content; the recipient sees two identical
   messages during the window). Acceptable for the verification window;
   not acceptable post-cutover.
4. After 24h clean parity, remove each launchd plist:
   ```bash
   launchctl bootout user/$(id -u)/com.itismyfield.agent-feedback-briefing
   rm ~/Library/LaunchAgents/com.itismyfield.agent-feedback-briefing.plist
   # repeat per label
   ```
   Note: the launchd plists live on **mac-mini**, not mac-book. SSH there
   to perform removal.
5. For job 12, the plist lives on mac-mini at
   `~/Library/LaunchAgents/com.agentdesk.queue-stability-batch.plist`.

## Rollback

If a routine misbehaves or the operator wants to revert:

1. `curl -sf "$API/api/routines/<id>" -X PATCH -H 'Content-Type: application/json' -d '{"status":"paused"}'`
2. The launchd plist (which was never removed) continues to fire
   uninterrupted — the system is back to launchd-only.
3. If the routine row should be removed entirely:
   `curl -sf "$API/api/routines/<id>/detach" -X POST`.

The launchd plist removal step at the end of the verification window is
the only **irreversible** action. As long as the plist is still in
`~/Library/LaunchAgents/`, rollback is a single PATCH.

## Cross-leader correctness

Routines run on whichever node holds the `routine-runtime` leader-only
worker lease (see issue #2202 §1). After the §1 fix, lease succession
re-spawns `routine-runtime` on the new leader, so the migrated jobs fire
regardless of which physical node (mac-mini or mac-book) is leader at
schedule time — unlike launchd, which only fires on the node where the
plist is loaded (currently mac-mini). This is the principal reliability
gain of the migration.
