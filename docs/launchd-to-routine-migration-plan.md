# launchd → AgentDesk Routine Migration Plan (#2202 §2/§3)

This document tracks the migration of 12 launchd cron jobs to AgentDesk
routines. **The launchd plists are intentionally left in place during the
24h+ verification window**; routines and launchd both fire, the operator
de-duplicates by removing the launchd plist once parity is confirmed.

## Routine scripts

All routine scripts live under `routines/migrated-launchd/`. Each routine's
`tick()` returns `action: "agent"` with a prompt instructing the attached
agent to invoke the repo-deployed entrypoint under
`/Users/itismyfield/.adk/release/scripts/launchd-migrated/`. The entrypoints
were copied from the original `~/.local/bin/*.sh` launchd targets and are
deployed by `adk-release`, so leadership can move between eligible nodes
without a manual script rsync.

## The 12 jobs

| # | launchd label | routine script_ref | cron (KST) | agent_id | status |
|---|---|---|---|---|---|
| 1 | `com.itismyfield.agent-feedback-briefing` | `migrated-launchd/agent-feedback-briefing.js` | `5 19 * * *` | `ch-pmd` | cutover (stage-paused) |
| 2 | `com.itismyfield.ai-integrated-briefing` | `migrated-launchd/ai-integrated-briefing.js` | `10 9,21 * * *` | `project-newsbot` | cutover (stage-paused) |
| 3 | `com.itismyfield.banchan-day-reminder.prep` | `migrated-launchd/banchan-day-reminder-prep.js` | `0 8 * * *` | `family-routine` | cutover (stage-paused) |
| 4 | `com.itismyfield.banchan-day-reminder.cook` | `migrated-launchd/banchan-day-reminder-cook.js` | `0 18 * * *` | `family-routine` | cutover (stage-paused) |
| 5 | `com.itismyfield.cookingheart-daily-briefing` | `migrated-launchd/cookingheart-daily-briefing.js` | `0 19 * * *` | `project-agentdesk` | cutover (stage-paused) |
| 6 | `com.itismyfield.family-morning-briefing.obujang` | `migrated-launchd/family-morning-briefing-obujang.js` | `30 6 * * *` | `personal-obiseo` | cutover (stage-paused) |
| 7 | `com.itismyfield.family-morning-briefing.yohoejang` | `migrated-launchd/family-morning-briefing-yohoejang.js` | `31 6 * * *` | `personal-yobiseo` | cutover (stage-paused) |
| 8 | `com.itismyfield.memento-daily-report` | `migrated-launchd/memento-daily-report.js` | `0 9 * * *` | `personal-obiseo` | cutover (stage-paused) |
| 9 | `com.itismyfield.memento-hygiene` | `migrated-launchd/memento-hygiene.js` | `0 6 * * *` | `personal-obiseo` | cutover (stage-paused) |
| 10 | `com.itismyfield.memory-merge` | `migrated-launchd/memory-merge.js` | `0 6 * * *` | `project-agentdesk` | cutover (stage-paused) |
| 11 | `com.itismyfield.token-daily-report` | `migrated-launchd/token-daily-report.js` | `0 7 * * *` | `token-manager` | cutover (stage-paused) |
| 12 | `com.agentdesk.queue-stability-batch` | `migrated-launchd/queue-stability-batch.js` | `0 4 * * *` | `project-agentdesk` | parallel-run (idempotent) |

Owner decision for jobs 8/9/10: the two Memento store jobs belong to
`personal-obiseo` because they operate on the operator-private Memento
store/reporting channel; `memory-merge` belongs to `project-agentdesk`
because it maintains the AgentDesk-managed memory tier files and skill
catalog. They still use the stage-paused cutover flow because they mutate
external memory state and must not true-parallel-run with launchd.

## Routine cron timezone

Routines use `routines.default_timezone = "Asia/Seoul"` (see
`src/config.rs`). Cron expressions in the table above match the original
launchd `StartCalendarInterval` wall-clock times exactly. DST is not a
factor in Asia/Seoul (KST is UTC+9 year-round, no DST), so no off-by-one
hour shift is possible between launchd and the routine scheduler.

## Operator: attach routines (once dcserver is up + release is deployed)

Run on whichever node is the cluster leader. The workspace containing
`routines/migrated-launchd/` must be deployed before the script loader
will see the new files, and `scripts/launchd-migrated/` must be deployed
under `/Users/itismyfield/.adk/release/scripts/launchd-migrated/`.

The attach commands are split into three groups: Group A
(parallel-run-safe, attach with schedule), Group B (cutover via
stage-paused — attach without schedule, then PATCH schedule at cutover
time), and Group C (memory-state jobs; also stage-paused).

### Group A — attach with schedule (parallel-run safe: only job 12)

Only the queue-stability-batch script has a built-in idempotency guard
(skip if a run is active/pending/paused), so it is safe to fire from
both launchd and the routine during the verification window.

```bash
REL_PORT="${AGENTDESK_REL_PORT:-8791}"
API="http://127.0.0.1:${REL_PORT}"

# Job 12 — queue-stability-batch (script has idempotency guard)
curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/queue-stability-batch.js",
  "name": "queue-stability-batch",
  "agent_id": "project-agentdesk",
  "execution_strategy": "fresh",
  "schedule": "0 4 * * *",
  "timeout_secs": 3600
}'
```

### Group B — stage-paused attach without schedule (cutover jobs: 1, 2, 3, 4, 5, 6, 7, 11)

Jobs 3/4 (banchan reminders) are also in this group because the
verification window can land on 반찬데이, where the skill's calendar
guard allows a real Discord reminder; true parallel-run would
deliver duplicate reminders to the family channel.

These have user-visible side effects (Discord messages / DMs). They are
attached **without** `schedule` so the inserted row's `next_due_at`
stays null and routine-runtime cannot fire them. Immediately pause the
row as belt-and-suspenders, then follow the
**Stage-paused → cutover protocol** below to PATCH the real schedule at
cutover time.

```bash
# Capture the routine id from each POST response (jq .routine.id).

# Job 1 — agent-feedback-briefing (cutover schedule: 5 19 * * *)
ID1=$(curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/agent-feedback-briefing.js",
  "name": "agent-feedback-briefing",
  "agent_id": "ch-pmd",
  "execution_strategy": "fresh",
  "timeout_secs": 1800
}' | jq -r '.routine.id')
curl -sf "$API/api/routines/$ID1/pause" -X POST

# Job 2 — ai-integrated-briefing (cutover schedule: 10 9,21 * * *)
ID2=$(curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/ai-integrated-briefing.js",
  "name": "ai-integrated-briefing",
  "agent_id": "project-newsbot",
  "execution_strategy": "fresh",
  "timeout_secs": 1800
}' | jq -r '.routine.id')
curl -sf "$API/api/routines/$ID2/pause" -X POST

# Job 5 — cookingheart-daily-briefing (cutover schedule: 0 19 * * *)
ID5=$(curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/cookingheart-daily-briefing.js",
  "name": "cookingheart-daily-briefing",
  "agent_id": "project-agentdesk",
  "execution_strategy": "fresh",
  "timeout_secs": 1800
}' | jq -r '.routine.id')
curl -sf "$API/api/routines/$ID5/pause" -X POST

# Job 6 — family-morning-briefing-obujang (cutover schedule: 30 6 * * *)
ID6=$(curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/family-morning-briefing-obujang.js",
  "name": "family-morning-briefing-obujang",
  "agent_id": "personal-obiseo",
  "execution_strategy": "fresh",
  "timeout_secs": 1800
}' | jq -r '.routine.id')
curl -sf "$API/api/routines/$ID6/pause" -X POST

# Job 7 — family-morning-briefing-yohoejang (cutover schedule: 31 6 * * *)
ID7=$(curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/family-morning-briefing-yohoejang.js",
  "name": "family-morning-briefing-yohoejang",
  "agent_id": "personal-yobiseo",
  "execution_strategy": "fresh",
  "timeout_secs": 1800
}' | jq -r '.routine.id')
curl -sf "$API/api/routines/$ID7/pause" -X POST

# Job 3 — banchan-day-reminder-prep (cutover schedule: 0 8 * * *)
ID3=$(curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/banchan-day-reminder-prep.js",
  "name": "banchan-day-reminder-prep",
  "agent_id": "family-routine",
  "execution_strategy": "fresh",
  "timeout_secs": 900
}' | jq -r '.routine.id')
curl -sf "$API/api/routines/$ID3/pause" -X POST

# Job 4 — banchan-day-reminder-cook (cutover schedule: 0 18 * * *)
ID4=$(curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/banchan-day-reminder-cook.js",
  "name": "banchan-day-reminder-cook",
  "agent_id": "family-routine",
  "execution_strategy": "fresh",
  "timeout_secs": 900
}' | jq -r '.routine.id')
curl -sf "$API/api/routines/$ID4/pause" -X POST

# Job 11 — token-daily-report (cutover schedule: 0 7 * * *)
ID11=$(curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/token-daily-report.js",
  "name": "token-daily-report",
  "agent_id": "token-manager",
  "execution_strategy": "fresh",
  "timeout_secs": 1800
}' | jq -r '.routine.id')
curl -sf "$API/api/routines/$ID11/pause" -X POST

# Verify all eight are paused:
for ID in "$ID1" "$ID2" "$ID3" "$ID4" "$ID5" "$ID6" "$ID7" "$ID11"; do
  curl -sf "$API/api/routines/$ID" | jq -r '.routine | "\(.id) \(.status)"'
done
# Expected: every row reports "paused".
```

### Group C — stage-paused attach without schedule (memory-state jobs: 8, 9, 10)

The launchd plists for memento-daily-report, memento-hygiene, and
memory-merge keep firing until cutover. Attach these via the Group B
pattern (no schedule, then pause, then PATCH schedule at cutover) because
they mutate external state (memento store / merged memory files), so true
parallel-run is not safe.

```bash
# Job 8 — memento-daily-report (cutover schedule: 0 9 * * *)
ID8=$(curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/memento-daily-report.js",
  "name": "memento-daily-report",
  "agent_id": "personal-obiseo",
  "execution_strategy": "fresh",
  "timeout_secs": 1800
}' | jq -r '.routine.id')
curl -sf "$API/api/routines/$ID8/pause" -X POST

# Job 9 — memento-hygiene (cutover schedule: 0 6 * * *)
ID9=$(curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/memento-hygiene.js",
  "name": "memento-hygiene",
  "agent_id": "personal-obiseo",
  "execution_strategy": "fresh",
  "timeout_secs": 1800
}' | jq -r '.routine.id')
curl -sf "$API/api/routines/$ID9/pause" -X POST

# Job 10 — memory-merge (cutover schedule: 0 6 * * *)
ID10=$(curl -sf "$API/api/routines" -X POST -H 'Content-Type: application/json' -d '{
  "script_ref": "migrated-launchd/memory-merge.js",
  "name": "memory-merge",
  "agent_id": "project-agentdesk",
  "execution_strategy": "fresh",
  "timeout_secs": 1800
}' | jq -r '.routine.id')
curl -sf "$API/api/routines/$ID10/pause" -X POST
```

## Cross-leader prerequisite — script availability

Jobs 1–11 now invoke scripts staged by `adk-release` at
`/Users/itismyfield/.adk/release/scripts/launchd-migrated/*.sh`; the
helper `run-claude-message-job.sh` is staged in the same directory and
called via the script's own directory. Cross-leader failover therefore
uses the release artifact instead of host-local `~/.local/bin` state.

Before attaching any migrated job, deploy the release on every node
eligible to hold the `routine-runtime` lease and verify the directory:

```bash
ls -l /Users/itismyfield/.adk/release/scripts/launchd-migrated/*.sh | sort
```

No supported `preferred-leader` / `execution_scope` knob currently exists
to pin `routine-runtime` to mac-mini (`WORKER_SPECS` declares it
hardcoded `LeaderOnly`; the only way to keep the lease on mac-mini is to
keep mac-book down or out of the cluster). The release-deployed
entrypoint directory is the supported source of truth for routine
execution.

## Verification window (≥24 hours)

Because jobs 1, 2, 5, 6, 7, 11 send Discord messages, the operator
**must avoid true parallel-running** for those — the recipient would see
two copies of every briefing. Use the **stage-paused → cutover**
protocol instead:

### Stage-paused → cutover protocol (jobs with Discord side effects: 1, 2, 5, 6, 7, 11)

`POST /api/routines` always inserts the row as `status='enabled'` with a
computed `next_due_at`; there is no create-as-paused flag. Calling pause
in a second request opens a race: if the attach lands within one minute
of the cron's fire time, `routine-runtime` can claim the lease and send
the message before the pause arrives, producing a duplicate Discord
fire alongside the still-loaded launchd plist. To eliminate that race,
**attach without a schedule first**, then pause, then PATCH the schedule
in:

**Critical ordering:** PATCH the schedule **before** booting out
launchd, so the routine's `next_due_at` is computed and verifiable
while launchd is still firing. Only after the DB has a valid
`next_due_at` do you bootout launchd, then resume with that exact
`next_due_at` echoed back to the API. Passing `{}` to resume writes
`next_due_at = NULL`, which strands the routine (the seed loop only
re-runs at dcserver boot).

1. Attach the row **with no schedule** so the routine-runtime cannot
   pick a `next_due_at`:
   ```bash
   curl -sf "$API/api/routines" -X POST \
     -H 'Content-Type: application/json' \
     -d '{
       "script_ref": "migrated-launchd/cookingheart-daily-briefing.js",
       "name": "cookingheart-daily-briefing",
       "agent_id": "project-agentdesk",
       "execution_strategy": "fresh",
       "timeout_secs": 1800
     }'
   ```
   Note `schedule` is omitted — the routine has no `next_due_at`, so it
   cannot fire.
2. Pause the routine (belt-and-suspenders against any background
   resume that wrote a `next_due_at`):
   ```bash
   curl -sf "$API/api/routines/<id>/pause" -X POST
   ```
3. PATCH the schedule in. This automatically computes and stores
   `next_due_at`; the row stays paused so the lease loop ignores it.
   ```bash
   curl -sf "$API/api/routines/<id>" -X PATCH \
     -H 'Content-Type: application/json' \
     -d '{"schedule":"0 19 * * *"}'
   ```
4. **Verify** the row is paused **and** has a `next_due_at` strictly
   in the future:
   ```bash
   curl -sf "$API/api/routines/<id>" | jq '.routine | {status, schedule, next_due_at}'
   # Expected: status="paused", schedule matches, next_due_at is a
   # future RFC3339 timestamp at the right cron mark.
   ```
   Capture the value for step 6:
   ```bash
   NEXT_DUE=$(curl -sf "$API/api/routines/<id>" | jq -r '.routine.next_due_at')
   ```
5. SSH mac-mini and bootout launchd for the affected label only:
   ```bash
   launchctl bootout user/$(id -u)/<launchd-label>
   launchctl print user/$(id -u)/<launchd-label> 2>&1 | head -1
   # Expected: "Could not find service" (confirms bootout).
   ```
   Do **not** delete the plist file. Leave it in
   `~/Library/LaunchAgents/` so Rollback B remains a single
   `launchctl bootstrap` away.
6. Resume the routine and pass `next_due_at` explicitly to preserve
   the value PATCH computed. The resume route uses
   `Json<ResumeRoutineBody>`; without an explicit `next_due_at` the
   handler overwrites the column with NULL:
   ```bash
   curl -sf "$API/api/routines/<id>/resume" -X POST \
     -H 'Content-Type: application/json' \
     -d "{\"next_due_at\":\"$NEXT_DUE\"}"
   curl -sf "$API/api/routines/<id>" | jq '.routine | {status, next_due_at}'
   # Expected: status="enabled", next_due_at matches $NEXT_DUE.
   ```
7. Watch `GET /api/routines/<id>/runs?limit=10` and the Discord target
   for the next scheduled fire to confirm the routine sends exactly
   one message with the same payload the launchd plist used to send.
8. After 24h clean operation, **move** the plist file instead of
   removing it:
   ```bash
   mkdir -p ~/Library/LaunchAgents.disabled
   mv ~/Library/LaunchAgents/<launchd-label>.plist \
      ~/Library/LaunchAgents.disabled/
   ```
   This keeps Rollback B viable; Rollback C is only needed if the
   file is truly deleted.

Before promoting any of these jobs to production, smoke-test the
attach → pause → PATCH → bootout → resume sequence against a
throwaway routine (e.g. one of the `monitoring/` scripts pointed at a
test channel) to confirm the resume actually fires at the expected
minute and `next_due_at` stays populated after resume.

### True parallel-run (job 12 only)

Only `queue-stability-batch` has an in-script idempotency guard (skips
if a run is active/pending/paused), so it is safe to fire from both
launchd and the routine for 24h. Jobs 3 and 4 were originally proposed
as parallel-run-safe via calendar gating, but the verification window
can overlap an actual 반찬데이 — duplicate reminders would land in the
family channel. They have been moved to the Group B stage-paused
cutover protocol.

1. Attach (`POST /api/routines`) — routine starts firing immediately.
2. Watch `GET /api/routines/<id>/runs?limit=10` and the relevant
   channel/queue for parity with the launchd job.
3. After 24h, `launchctl bootout` then **move (not rm)** the plist:
   `mv ~/Library/LaunchAgents/com.agentdesk.queue-stability-batch.plist
   ~/Library/LaunchAgents.disabled/` so Rollback C is avoidable.

### Jobs 8/9/10 — memory-state cutover

These now have concrete owners, but still use the stage-paused →
cutover protocol. The launchd plists keep firing until the cutover
moment because these jobs write/report Memento and memory-tier state.

### Per-routine observability

Use `GET /api/routines/<id>/runs?limit=10` for each attached routine
(the documented `/api/routines/runs/search` endpoint requires a
non-empty `q` parameter, so the empty-`q` listing approach does not
work). Also use `GET /api/routines/metrics?agent_id=<id>` for
aggregate counts.

## Rollback

The cutover protocol moves the system through three states. Each has a
different correct rollback path:

| State | launchd plist | launchd loaded? | routine | Rollback restores |
|---|---|---|---|---|
| Pre-attach | on disk | loaded (firing) | n/a | nothing to do |
| Stage-paused (attached + paused) | on disk | loaded (firing) | paused | pause/detach routine |
| Mid-cutover (launchd bootout, routine resumed) | on disk | **not loaded** | enabled (firing) | re-bootstrap launchd, **then** pause routine |
| Post-removal | deleted | not loaded | enabled (firing) | restore plist file, bootstrap, **then** pause routine |

Note: there is **no** PATCH-status code path; do not try
`PATCH /api/routines/<id>` with `{"status":"paused"}` — the API
ignores unknown fields silently. Always use the dedicated
`/pause` / `/resume` / `/detach` subroutes.

### Rollback A — Stage-paused state (launchd still loaded)

1. `curl -sf "$API/api/routines/<id>/pause" -X POST` (no-op if already
   paused).
2. Verify the routine is paused: `curl -sf "$API/api/routines/<id>"`
   and check `"status": "paused"`.
3. The launchd plist is still loaded → already firing on schedule. The
   system is back to launchd-only.
4. Optional: `curl -sf "$API/api/routines/<id>/detach" -X POST` to
   remove the row entirely (idempotent).

### Rollback B — Mid-cutover state (launchd booted out, routine firing)

This is the **critical** failure-mode rollback. Pausing the routine
without first reloading the plist would leave **nothing firing**
between the rollback moment and the next schedule.

Required order:

1. SSH mac-mini.
2. Re-bootstrap launchd for the affected label (the plist file is
   still in `~/Library/LaunchAgents/` because the cutover protocol
   explicitly does **not** `rm` it until after the 24h window):
   ```bash
   launchctl bootstrap user/$(id -u) ~/Library/LaunchAgents/<label>.plist
   launchctl print user/$(id -u)/<label> | head -1  # verify it loaded
   ```
3. Only after the bootstrap succeeds, pause the routine:
   ```bash
   curl -sf "$API/api/routines/<id>/pause" -X POST
   curl -sf "$API/api/routines/<id>" | jq '.routine.status'  # "paused"
   ```
4. Confirm launchd is the sole sender by waiting for the next cron
   minute and verifying exactly one fire reaches the target channel.

### Rollback C — Post-removal state (plist file deleted)

1. SSH mac-mini.
2. Recreate the plist file in `~/Library/LaunchAgents/<label>.plist`
   from the recorded content (every plist content is captured verbatim
   in the original issue #2202 §2 table; the schedules in this doc's
   table at the top are the canonical source of truth for cron
   timing).
3. `launchctl bootstrap user/$(id -u) ~/Library/LaunchAgents/<label>.plist`.
4. After bootstrap confirms loaded, `curl -sf "$API/api/routines/<id>/pause" -X POST`.

**Recommendation to avoid Rollback C entirely:** instead of `rm` at
the end of the 24h window, move the plist to
`~/Library/LaunchAgents.disabled/<label>.plist`. Rollback becomes a
copy-back + bootstrap (Rollback B equivalent) rather than recreation
from documentation.

## Cross-leader correctness

Routines run on whichever node holds the `routine-runtime` leader-only
worker lease (see issue #2202 §1). After the §1 fix, lease succession
re-spawns `routine-runtime` on the new leader, so the migrated jobs fire
regardless of which physical node (mac-mini or mac-book) is leader at
schedule time — unlike launchd, which only fires on the node where the
plist is loaded (currently mac-mini). This is the principal reliability
gain of the migration **once the release-deployed entrypoint directory
exists on every eligible leader** (see Cross-leader prerequisite above).
