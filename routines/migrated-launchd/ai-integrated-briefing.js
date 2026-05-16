// Migrated from launchd: com.itismyfield.ai-integrated-briefing
// Original shell script: ~/.local/bin/ai-integrated-briefing.sh
// Schedule: 10 9,21 * * * (KST, 09:10 and 21:10 daily)
// Agent: project-newsbot
//
// Attach via the stage-paused sequence (see migration plan):
//   1. POST /api/routines with NO schedule:
//      { "script_ref": "migrated-launchd/ai-integrated-briefing.js",
//        "name": "ai-integrated-briefing", "agent_id": "project-newsbot",
//        "execution_strategy": "fresh", "timeout_secs": 1800 }
//   2. POST /api/routines/<id>/pause
//   3. PATCH /api/routines/<id> { "schedule": "10 9,21 * * *" }
//   4. Verify next_due_at and capture as $NEXT_DUE.
//   5. SSH mac-mini, launchctl bootout the launchd label.
//   6. POST /api/routines/<id>/resume -d "{\"next_due_at\":\"$NEXT_DUE\"}"
// Do NOT POST with "schedule" included on attach — duplicate-send race.
//
// CUTOVER SAFETY: This job sends to Discord. Use the stage-paused → cutover
// protocol in docs/launchd-to-routine-migration-plan.md (attach without
// schedule → pause → PATCH schedule → bootout launchd label → resume).
// True parallel-running would duplicate the Discord message.
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
