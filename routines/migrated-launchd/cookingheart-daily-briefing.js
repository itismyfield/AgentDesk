// Migrated from launchd: com.itismyfield.cookingheart-daily-briefing
// Original shell script: ~/.local/bin/cookingheart-daily-briefing.sh
// Schedule: 0 19 * * * (KST, 19:00 daily)
// Agent: project-agentdesk
//
// Attach this routine via POST /api/routines with:
//   {
//     "script_ref": "migrated-launchd/cookingheart-daily-briefing.js",
//     "name": "cookingheart-daily-briefing",
//     "agent_id": "project-agentdesk",
//     "execution_strategy": "fresh",
//     "schedule": "0 19 * * *",
//     "timeout_secs": 1800
//   }
//
// PARALLEL-RUN SAFETY: launchd plist remains active during verification.
agentdesk.routines.register({
  name: "cookingheart-daily-briefing",
  tick(ctx) {
    return {
      action: "agent",
      prompt: [
        "Run the migrated launchd job 'cookingheart-daily-briefing' for routine_id=" +
          ctx.routine.id,
        "Invoke the existing shell pipeline exactly as launchd does:",
        "  /Users/itismyfield/.local/bin/cookingheart-daily-briefing.sh",
        "This preserves the original prompt body, target channel, and skill path.",
        "Return a one-line status summary (success | NO_REPLY | error: <msg>).",
      ].join("\n"),
    };
  },
});
