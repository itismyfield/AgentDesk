// Migrated from launchd: com.itismyfield.agent-feedback-briefing
// Original shell script: ~/.local/bin/agent-feedback-briefing.sh
// Schedule: 5 19 * * * (KST, 19:05 daily)
// Agent: ch-pmd
//
// Attach this routine via POST /api/routines with:
//   {
//     "script_ref": "migrated-launchd/agent-feedback-briefing.js",
//     "name": "agent-feedback-briefing",
//     "agent_id": "ch-pmd",
//     "execution_strategy": "fresh",
//     "schedule": "5 19 * * *",
//     "timeout_secs": 1800
//   }
//
// CUTOVER SAFETY: This job sends to Discord. Use the stage-paused → cutover
// protocol in docs/launchd-to-routine-migration-plan.md (attach without
// schedule → pause → PATCH schedule → bootout launchd label → resume).
// True parallel-running would duplicate the Discord message.
agentdesk.routines.register({
  name: "agent-feedback-briefing",
  tick(ctx) {
    return {
      action: "agent",
      prompt: [
        "Run the migrated launchd job 'agent-feedback-briefing' for routine_id=" +
          ctx.routine.id,
        "Invoke the existing shell pipeline exactly as launchd does:",
        "  /Users/itismyfield/.local/bin/agent-feedback-briefing.sh",
        "This preserves the original prompt body, target channel, and skill path.",
        "Return a one-line status summary (success | NO_REPLY | error: <msg>) for the routine result.",
      ].join("\n"),
    };
  },
});
