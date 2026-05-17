// Migrated from launchd: com.itismyfield.agent-feedback-briefing
// Original shell script: ~/.local/bin/agent-feedback-briefing.sh
// Schedule: 5 19 * * * (KST, 19:05 daily)
// Agent: ch-pmd
//
// Attach via the stage-paused sequence (see migration plan):
//   1. POST /api/routines with NO schedule:
//      { "script_ref": "migrated-launchd/agent-feedback-briefing.js",
//        "name": "agent-feedback-briefing", "agent_id": "ch-pmd",
//        "execution_strategy": "fresh", "timeout_secs": 1800 }
//   2. POST /api/routines/<id>/pause
//   3. PATCH /api/routines/<id> { "schedule": "5 19 * * *" }
//   4. Verify next_due_at is populated and in the future:
//      curl ... /api/routines/<id> | jq .routine.next_due_at
//      Capture as $NEXT_DUE.
//   5. SSH mac-mini, launchctl bootout the launchd label.
//   6. POST /api/routines/<id>/resume -d "{\"next_due_at\":\"$NEXT_DUE\"}"
//      (a bare {} body writes next_due_at=NULL and strands the routine).
// Do NOT POST with "schedule" included on attach — duplicate-send race.
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
