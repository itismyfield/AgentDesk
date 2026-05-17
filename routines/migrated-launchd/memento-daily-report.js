// Migrated from launchd: com.itismyfield.memento-daily-report
// Original shell script: ~/.local/bin/memento-daily-report.sh
// Repo-deployed shell script:
//   /Users/itismyfield/.adk/release/scripts/launchd-migrated/memento-daily-report.sh
// Schedule: 0 9 * * * (KST, 09:00 daily)
// Agent: personal-obiseo
//
// Attach via the stage-paused sequence:
//   1. POST /api/routines with NO schedule, NO TODO:
//      { "script_ref": "migrated-launchd/memento-daily-report.js",
//        "name": "memento-daily-report",
//        "agent_id": "personal-obiseo",
//        "execution_strategy": "fresh", "timeout_secs": 1800 }
//   2. POST /api/routines/<id>/pause
//   3. PATCH /api/routines/<id> { "schedule": "0 9 * * *" }
//   4. Verify next_due_at and capture as $NEXT_DUE.
//   5. SSH mac-mini, launchctl bootout the launchd label.
//   6. POST /api/routines/<id>/resume -d "{\"next_due_at\":\"$NEXT_DUE\"}"
// Do NOT POST with "schedule" included on attach.
//
// CUTOVER SAFETY: This job may write/report side effects (memento snapshot).
// Use the stage-paused → cutover protocol in
// docs/launchd-to-routine-migration-plan.md to avoid duplicate writes.
agentdesk.routines.register({
  name: "memento-daily-report",
  tick(ctx) {
    return {
      action: "agent",
      prompt: [
        "Run the migrated launchd job 'memento-daily-report' for routine_id=" +
          ctx.routine.id,
        "Invoke the existing shell pipeline exactly as launchd does:",
        "  /Users/itismyfield/.adk/release/scripts/launchd-migrated/memento-daily-report.sh",
        "Working directory matches the original launchd job:",
        "  /Users/itismyfield/.adk/release/workspaces/agentfactory",
        "Return a one-line status summary (success | NO_REPLY | error: <msg>).",
      ].join("\n"),
    };
  },
});
