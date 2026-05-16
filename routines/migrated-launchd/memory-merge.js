// Migrated from launchd: com.itismyfield.memory-merge
// Original shell script: ~/.local/bin/memory-merge.sh
// Schedule: 0 6 * * * (KST, 06:00 daily)
// Agent: TODO — operator must set agent_id before enabling this routine.
//        The issue marks this as "(담당자 확정 필요)".
//
// Attach this routine via POST /api/routines with:
//   {
//     "script_ref": "migrated-launchd/memory-merge.js",
//     "name": "memory-merge",
//     "agent_id": "<TODO: operator decides>",
//     "execution_strategy": "fresh",
//     "schedule": "0 6 * * *",
//     "timeout_secs": 1800
//   }
//
// Do not enable this routine (status=enabled) until agent_id is set. Until
// then, launchd continues to fire (no functional regression).
//
// The original launchd job sets AGENTDESK_MEMORY_MERGE_SKILL=
//   /Users/itismyfield/.adk/release/skills/memory-merge/SKILL.md
// The shell script must read this env var or fall back to the default skill
// path. Verify the script handles a missing env var before flipping the
// routine to status=enabled. If the script requires the env var, set it via
// the agent's environment configuration rather than per-routine.
//
// PARALLEL-RUN SAFETY: launchd plist remains active during verification.
agentdesk.routines.register({
  name: "memory-merge",
  tick(ctx) {
    return {
      action: "agent",
      prompt: [
        "Run the migrated launchd job 'memory-merge' for routine_id=" +
          ctx.routine.id,
        "Invoke the existing shell pipeline exactly as launchd does:",
        "  /Users/itismyfield/.local/bin/memory-merge.sh",
        "Working directory matches the original launchd job:",
        "  /Users/itismyfield/.adk/release/workspaces/agentfactory",
        "Ensure env var AGENTDESK_MEMORY_MERGE_SKILL points to the memory-merge",
        "SKILL.md (default: /Users/itismyfield/.adk/release/skills/memory-merge/SKILL.md).",
        "Return a one-line status summary (success | NO_REPLY | error: <msg>).",
      ].join("\n"),
    };
  },
});
