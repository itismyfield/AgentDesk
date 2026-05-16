// Migrated from launchd: com.itismyfield.banchan-day-reminder.cook
// Original shell script: ~/.local/bin/banchan-day-reminder-cook.sh
// Schedule: 0 18 * * * (KST, 18:00 daily)
// Agent: family-routine
//
// Attach this routine via POST /api/routines with:
//   {
//     "script_ref": "migrated-launchd/banchan-day-reminder-cook.js",
//     "name": "banchan-day-reminder-cook",
//     "agent_id": "family-routine",
//     "execution_strategy": "fresh",
//     "schedule": "0 18 * * *",
//     "timeout_secs": 900
//   }
//
// NOTE: Calendar-driven — the skill returns NO_REPLY on non-반찬데이 days. The
// 18:00 fire is intentional and matches the original launchd cadence.
//
// PARALLEL-RUN SAFETY: launchd plist remains active during verification.
agentdesk.routines.register({
  name: "banchan-day-reminder-cook",
  tick(ctx) {
    return {
      action: "agent",
      prompt: [
        "Run the migrated launchd job 'banchan-day-reminder.cook' for routine_id=" +
          ctx.routine.id,
        "Invoke the existing shell pipeline exactly as launchd does:",
        "  /Users/itismyfield/.local/bin/banchan-day-reminder-cook.sh",
        "The skill performs calendar lookup; NO_REPLY is the correct result on",
        "non-반찬데이 days. Do not second-guess the skill's calendar logic.",
        "Return a one-line status summary (success | NO_REPLY | error: <msg>).",
      ].join("\n"),
    };
  },
});
