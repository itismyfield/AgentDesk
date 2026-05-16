// Migrated from launchd: com.itismyfield.family-morning-briefing.obujang
// Original shell script: ~/.local/bin/family-morning-briefing-obujang.sh
// Schedule: 30 6 * * * (KST, 06:30 daily)
// Agent: personal-obiseo
//
// Attach this routine via POST /api/routines with:
//   {
//     "script_ref": "migrated-launchd/family-morning-briefing-obujang.js",
//     "name": "family-morning-briefing-obujang",
//     "agent_id": "personal-obiseo",
//     "execution_strategy": "fresh",
//     "schedule": "30 6 * * *",
//     "timeout_secs": 1800
//   }
//
// PARALLEL-RUN SAFETY: launchd plist remains active during verification.
// Morning briefings have user-visible side effects (Discord DM); the operator
// must visually verify only one fire is reaching the channel before removing
// the launchd plist.
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
