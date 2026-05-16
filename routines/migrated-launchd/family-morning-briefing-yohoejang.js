// Migrated from launchd: com.itismyfield.family-morning-briefing.yohoejang
// Original shell script: ~/.local/bin/family-morning-briefing-yohoejang.sh
// Schedule: 31 6 * * * (KST, 06:31 daily)
// Agent: personal-yobiseo
//
// Attach via the stage-paused sequence (see migration plan):
//   1. POST /api/routines with NO schedule:
//      { "script_ref": "migrated-launchd/family-morning-briefing-yohoejang.js",
//        "name": "family-morning-briefing-yohoejang",
//        "agent_id": "personal-yobiseo",
//        "execution_strategy": "fresh", "timeout_secs": 1800 }
//   2. POST /api/routines/<id>/pause
//   3. At cutover: launchctl bootout the launchd label, then
//      PATCH /api/routines/<id> { "schedule": "31 6 * * *" }
//      and POST /api/routines/<id>/resume -d '{}'
// Do NOT POST with "schedule" included — that opens a duplicate-send race.
//
// CUTOVER SAFETY: This job sends a personal morning briefing to Discord. Use
// the stage-paused → cutover protocol in
// docs/launchd-to-routine-migration-plan.md (attach without schedule → pause
// → PATCH schedule → bootout launchd label → resume). True parallel-running
// would deliver two briefings to the recipient every morning.
agentdesk.routines.register({
  name: "family-morning-briefing-yohoejang",
  tick(ctx) {
    return {
      action: "agent",
      prompt: [
        "Run the migrated launchd job 'family-morning-briefing.yohoejang' for routine_id=" +
          ctx.routine.id,
        "Invoke the existing shell pipeline exactly as launchd does:",
        "  /Users/itismyfield/.local/bin/family-morning-briefing-yohoejang.sh",
        "Preserve the original prompt body, target channel, weather/calendar/reminders",
        "skill path, and Discord destination unchanged.",
        "Return a one-line status summary (success | NO_REPLY | error: <msg>).",
      ].join("\n"),
    };
  },
});
