// Migrated from launchd: com.itismyfield.family-morning-briefing.yohoejang
// Original shell script: ~/.local/bin/family-morning-briefing-yohoejang.sh
// Schedule: 31 6 * * * (KST, 06:31 daily)
// Agent: personal-yobiseo
//
// Attach this routine via POST /api/routines with:
//   {
//     "script_ref": "migrated-launchd/family-morning-briefing-yohoejang.js",
//     "name": "family-morning-briefing-yohoejang",
//     "agent_id": "personal-yobiseo",
//     "execution_strategy": "fresh",
//     "schedule": "31 6 * * *",
//     "timeout_secs": 1800
//   }
//
// PARALLEL-RUN SAFETY: launchd plist remains active during verification.
// Morning briefings have user-visible side effects (Discord DM); operator
// must visually verify only one fire reaches the channel before removing plist.
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
