// Migrated from launchd: com.itismyfield.family-morning-briefing.obujang
// Original shell script: ~/.local/bin/family-morning-briefing-obujang.sh
// Schedule: 30 6 * * * (KST, 06:30 daily)
// Agent: personal-obiseo
//
// Attach via the stage-paused sequence (see migration plan):
//   1. POST /api/routines with NO schedule (no schedule means no fire):
//      { "script_ref": "migrated-launchd/family-morning-briefing-obujang.js",
//        "name": "family-morning-briefing-obujang",
//        "agent_id": "personal-obiseo",
//        "execution_strategy": "fresh", "timeout_secs": 1800 }
//   2. POST /api/routines/<id>/pause
//   3. PATCH /api/routines/<id> { "schedule": "30 6 * * *" }
//   4. Verify next_due_at and capture as $NEXT_DUE.
//   5. SSH mac-mini, launchctl bootout the launchd label.
//   6. POST /api/routines/<id>/resume -d "{\"next_due_at\":\"$NEXT_DUE\"}"
// Do NOT POST with "schedule" included on attach — duplicate-send race.
//
// CUTOVER SAFETY: This job sends a personal morning briefing to Discord. Use
// the stage-paused → cutover protocol in
// docs/launchd-to-routine-migration-plan.md (attach without schedule → pause
// → PATCH schedule → bootout launchd label → resume). True parallel-running
// would deliver two briefings to the recipient every morning.
agentdesk.routines.register({
  name: "family-morning-briefing-obujang",
  tick(ctx) {
    return {
      action: "agent",
      prompt: [
        "Run the migrated launchd job 'family-morning-briefing.obujang' for routine_id=" +
          ctx.routine.id,
        "Invoke the existing shell pipeline exactly as launchd does:",
        "  /Users/itismyfield/.local/bin/family-morning-briefing-obujang.sh",
        "Preserve the original prompt body, target channel, weather/calendar/reminders",
        "skill path, and Discord destination unchanged.",
        "Return a one-line status summary (success | NO_REPLY | error: <msg>).",
      ].join("\n"),
    };
  },
});
