// Migrated from launchd: com.itismyfield.memento-hygiene
// Original shell script: ~/.local/bin/memento-hygiene.sh
// Repo-deployed shell script:
//   /Users/itismyfield/.adk/release/scripts/launchd-migrated/memento-hygiene.sh
// Schedule: 0 6 * * * (KST, 06:00 daily)
// Agent: personal-obiseo
//
// Attach via the stage-paused sequence:
//   1. POST /api/routines with NO schedule:
//      { "script_ref": "migrated-launchd/memento-hygiene.js",
//        "name": "memento-hygiene", "agent_id": "personal-obiseo",
//        "execution_strategy": "fresh", "timeout_secs": 1800 }
//   2. POST /api/routines/<id>/pause
//   3. PATCH /api/routines/<id> { "schedule": "0 6 * * *" }
//   4. Verify next_due_at and capture as $NEXT_DUE.
//   5. SSH mac-mini, launchctl bootout the launchd label.
//   6. POST /api/routines/<id>/resume -d "{\"next_due_at\":\"$NEXT_DUE\"}"
// Do NOT POST with "schedule" included on attach.
//
// CUTOVER SAFETY: This job mutates memento state (hygiene compaction).
// Use the stage-paused → cutover protocol in
// docs/launchd-to-routine-migration-plan.md to avoid running two compactions
// back-to-back.
agentdesk.routines.register({
  name: "memento-hygiene",
  tick(ctx) {
    return {
      action: "agent",
      prompt: [
        "Run the migrated launchd job 'memento-hygiene' for routine_id=" +
          ctx.routine.id,
        "Invoke the existing shell pipeline exactly as launchd does:",
        "  /Users/itismyfield/.adk/release/scripts/launchd-migrated/memento-hygiene.sh",
        "Working directory matches the original launchd job:",
        "  /Users/itismyfield/.adk/release/workspaces/agentfactory",
        "Return a one-line status summary (success | NO_REPLY | error: <msg>).",
      ].join("\n"),
    };
  },
});
