// Migrated from launchd: com.itismyfield.token-daily-report
// Original shell script: ~/.local/bin/token-daily-report.sh
// Schedule: 0 7 * * * (KST, 07:00 daily)
// Agent: token-manager
//
// Attach via the stage-paused sequence (see migration plan):
//   1. POST /api/routines with NO schedule:
//      { "script_ref": "migrated-launchd/token-daily-report.js",
//        "name": "token-daily-report", "agent_id": "token-manager",
//        "execution_strategy": "fresh", "timeout_secs": 1800 }
//   2. POST /api/routines/<id>/pause
//   3. At cutover: launchctl bootout the launchd label, then
//      PATCH /api/routines/<id> { "schedule": "0 7 * * *" }
//      and POST /api/routines/<id>/resume -d '{}'
// Do NOT POST with "schedule" included — that opens a duplicate-send race.
//
// CUTOVER SAFETY: This job sends a Discord report. Use the stage-paused →
// cutover protocol in docs/launchd-to-routine-migration-plan.md (attach
// without schedule → pause → PATCH schedule → bootout launchd label →
// resume). True parallel-running would duplicate the Discord message.
agentdesk.routines.register({
  name: "token-daily-report",
  tick(ctx) {
    return {
      action: "agent",
      prompt: [
        "Run the migrated launchd job 'token-daily-report' for routine_id=" +
          ctx.routine.id,
        "Invoke the existing shell pipeline exactly as launchd does:",
        "  /Users/itismyfield/.local/bin/token-daily-report.sh",
        "Return a one-line status summary (success | NO_REPLY | error: <msg>).",
      ].join("\n"),
    };
  },
});
