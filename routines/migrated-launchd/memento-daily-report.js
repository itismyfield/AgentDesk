// Migrated from launchd: com.itismyfield.memento-daily-report
// Original shell script: ~/.local/bin/memento-daily-report.sh
// Schedule: 0 9 * * * (KST, 09:00 daily)
// Agent: TODO — operator must set agent_id before enabling this routine.
//        The issue marks this as "(담당자 확정 필요)".
//
// Attach this routine via POST /api/routines with:
//   {
//     "script_ref": "migrated-launchd/memento-daily-report.js",
//     "name": "memento-daily-report",
//     "agent_id": "<TODO: operator decides>",
//     "execution_strategy": "fresh",
//     "schedule": "0 9 * * *",
//     "timeout_secs": 1800
//   }
//
// Do not enable this routine (status=enabled) until agent_id is set. Until
// then, launchd continues to fire (no functional regression).
//
// PARALLEL-RUN SAFETY: launchd plist remains active during verification.
agentdesk.routines.register({
  name: "memento-daily-report",
  tick(ctx) {
    return {
      action: "agent",
      prompt: [
        "Run the migrated launchd job 'memento-daily-report' for routine_id=" +
          ctx.routine.id,
        "Invoke the existing shell pipeline exactly as launchd does:",
        "  /Users/itismyfield/.local/bin/memento-daily-report.sh",
        "Working directory matches the original launchd job:",
        "  /Users/itismyfield/.adk/release/workspaces/agentfactory",
        "Return a one-line status summary (success | NO_REPLY | error: <msg>).",
      ].join("\n"),
    };
  },
});
