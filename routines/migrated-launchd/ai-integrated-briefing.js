// Migrated from launchd: com.itismyfield.ai-integrated-briefing
// Original shell script: ~/.local/bin/ai-integrated-briefing.sh
// Schedule: 10 9,21 * * * (KST, 09:10 and 21:10 daily)
// Agent: project-newsbot
//
// Attach this routine via POST /api/routines with:
//   {
//     "script_ref": "migrated-launchd/ai-integrated-briefing.js",
//     "name": "ai-integrated-briefing",
//     "agent_id": "project-newsbot",
//     "execution_strategy": "fresh",
//     "schedule": "10 9,21 * * *",
//     "timeout_secs": 1800
//   }
//
// PARALLEL-RUN SAFETY: launchd plist remains active during the verification window.
agentdesk.routines.register({
  name: "ai-integrated-briefing",
  tick(ctx) {
    return {
      action: "agent",
      prompt: [
        "Run the migrated launchd job 'ai-integrated-briefing' for routine_id=" +
          ctx.routine.id,
        "Invoke the existing shell pipeline exactly as launchd does:",
        "  /Users/itismyfield/.local/bin/ai-integrated-briefing.sh",
        "This preserves the original prompt body, target channel, and skill path.",
        "Return a one-line status summary (success | NO_REPLY | error: <msg>) for the routine result.",
      ].join("\n"),
    };
  },
});
