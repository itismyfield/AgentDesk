// Migrated from launchd: com.itismyfield.memory-merge
// Original shell script: ~/.local/bin/memory-merge.sh
// Repo-deployed shell script:
//   scripts/launchd-migrated/memory-merge.sh (below AGENTDESK_ROOT_DIR)
// Schedule: 0 6 * * * (KST, 06:00 daily)
// Agent: project-agentdesk
//
// Attach via the stage-paused sequence:
//   1. POST /api/routines with NO schedule:
//      { "script_ref": "migrated-launchd/memory-merge.js",
//        "name": "memory-merge", "agent_id": "project-agentdesk",
//        "execution_strategy": "fresh", "timeout_secs": 1800 }
//   2. POST /api/routines/<id>/pause
//   3. PATCH /api/routines/<id> { "schedule": "0 6 * * *" }
//   4. Verify next_due_at and capture as $NEXT_DUE.
//   5. SSH mac-mini, launchctl bootout the launchd label.
//   6. POST /api/routines/<id>/resume -d "{\"next_due_at\":\"$NEXT_DUE\"}"
// Do NOT POST with "schedule" included on attach.
//
// The original launchd job sets AGENTDESK_MEMORY_MERGE_SKILL=
//   $AGENTDESK_ROOT_DIR/skills/memory-merge/SKILL.md
// The shell script must read this env var or fall back to the default skill
// path. Verify the script handles a missing env var before flipping the
// routine to status=enabled. If the script requires the env var, set it via
// the agent's environment configuration rather than per-routine.
//
// CUTOVER SAFETY: This job mutates the 4-tier memory store. Use the
// stage-paused → cutover protocol in
// docs/launchd-to-routine-migration-plan.md to avoid running two merges
// back-to-back.
agentdesk.routines.register({
  name: "memory-merge",
  tick(ctx) {
    return {
      action: "agent",
      prompt: [
        "Run the migrated launchd job 'memory-merge' for routine_id=" +
          ctx.routine.id,
        "Resolve the release root from AGENTDESK_ROOT_DIR, or ~/.adk/release if unset.",
        "Invoke this root-relative shell pipeline exactly as launchd does:",
        "  scripts/launchd-migrated/memory-merge.sh",
        "Use AGENTDESK_MIGRATED_AGENTFACTORY_WORKDIR when set; otherwise use AGENTDESK_ROOT_DIR + '/workspaces/agentfactory'.",
        "Ensure env var AGENTDESK_MEMORY_MERGE_SKILL points to the memory-merge",
        "SKILL.md if the root-relative default is not correct.",
        "Return a one-line status summary (success | NO_REPLY | error: <msg>).",
      ].join("\n"),
    };
  },
});
