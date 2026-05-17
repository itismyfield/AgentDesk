// Migrated from launchd: com.agentdesk.queue-stability-batch (issue #2202 §3)
// Original shell script: scripts/queue-stability-batch.sh (in this repo)
// Schedule: 0 4 * * * (KST, 04:00 daily)
// Agent: project-agentdesk (operator confirms — this is the AgentDesk
//        maintenance routine and uses AGENT_ID=project-agentdesk inside
//        the shell script).
//
// Attach this routine via POST /api/routines with:
//   {
//     "script_ref": "migrated-launchd/queue-stability-batch.js",
//     "name": "queue-stability-batch",
//     "agent_id": "project-agentdesk",
//     "execution_strategy": "fresh",
//     "schedule": "0 4 * * *",
//     "timeout_secs": 3600
//   }
//
// The shell script is idempotent (skips if active/pending/paused run exists),
// so a parallel-run window with launchd is safe — at most one phase will be
// queued per day even if both fire.
//
// PARALLEL-RUN SAFETY: launchd plist remains active during verification.
agentdesk.routines.register({
  name: "queue-stability-batch",
  tick(ctx) {
    return {
      action: "agent",
      prompt: [
        "Run the migrated launchd job 'queue-stability-batch' for routine_id=" +
          ctx.routine.id,
        "Invoke the existing shell entrypoint exactly as launchd does:",
        "  /Users/itismyfield/.adk/release/workspaces/agentdesk/scripts/queue-stability-batch.sh",
        "The script is idempotent (skips if a run is active/pending/paused);",
        "do not bypass that guard.",
        "Return a one-line status summary (success | skipped: <reason> | error: <msg>).",
      ].join("\n"),
    };
  },
});
