// Migrated from launchd: com.itismyfield.banchan-day-reminder.cook
// Original shell script: ~/.local/bin/banchan-day-reminder-cook.sh
// Schedule: 0 18 * * * (KST, 18:00 daily)
// Agent: family-routine
//
// Attach via the stage-paused sequence (verification window can land on
// 반찬데이, where calendar gating allows a real Discord reminder; true
// parallel-run would duplicate that reminder):
//   1. POST /api/routines with NO schedule:
//      { "script_ref": "migrated-launchd/banchan-day-reminder-cook.js",
//        "name": "banchan-day-reminder-cook", "agent_id": "family-routine",
//        "execution_strategy": "fresh", "timeout_secs": 900 }
//   2. POST /api/routines/<id>/pause
//   3. PATCH /api/routines/<id> { "schedule": "0 18 * * *" }
//   4. Verify next_due_at and capture as $NEXT_DUE.
//   5. SSH mac-mini, launchctl bootout the launchd label.
//   6. POST /api/routines/<id>/resume -d "{\"next_due_at\":\"$NEXT_DUE\"}"
// Do NOT POST with "schedule" included on attach.
//
// NOTE: Calendar-driven — the skill returns NO_REPLY on non-반찬데이 days. The
// 18:00 fire is intentional and matches the original launchd cadence.
//
// CUTOVER SAFETY: Calendar-gated, but the verification window could land
// on 반찬데이 and produce duplicate Discord reminders. Use the stage-paused
// → cutover protocol above to avoid that risk.
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
