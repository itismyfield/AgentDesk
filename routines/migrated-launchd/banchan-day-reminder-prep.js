// Migrated from launchd: com.itismyfield.banchan-day-reminder.prep
// Original shell script: ~/.local/bin/banchan-day-reminder-prep.sh
// Schedule: 0 8 * * * (KST, 08:00 daily)
// Agent: family-routine
//
// Attach via the stage-paused sequence (the verification window can land
// on 반찬데이, where calendar gating allows a real Discord reminder; true
// parallel-run would duplicate that reminder):
//   1. POST /api/routines with NO schedule:
//      { "script_ref": "migrated-launchd/banchan-day-reminder-prep.js",
//        "name": "banchan-day-reminder-prep", "agent_id": "family-routine",
//        "execution_strategy": "fresh", "timeout_secs": 900 }
//   2. POST /api/routines/<id>/pause
//   3. At cutover: launchctl bootout the launchd label, then
//      PATCH /api/routines/<id> { "schedule": "0 8 * * *" }
//      and POST /api/routines/<id>/resume -d '{}'
// Do NOT POST with "schedule" included.
//
// NOTE: The shell script + skill 'banchan-day-reminder' performs the calendar
// lookup itself; the daily 08:00 fire is intentional — the skill returns
// NO_REPLY on days when 반찬데이 is not relevant. This routine preserves that
// behavior unchanged by delegating to the same shell entrypoint.
//
// CUTOVER SAFETY: Calendar-gated, but the verification window could land
// on 반찬데이 and produce duplicate Discord reminders. Use the stage-paused
// → cutover protocol above to avoid that risk.
agentdesk.routines.register({
  name: "banchan-day-reminder-prep",
  tick(ctx) {
    return {
      action: "agent",
      prompt: [
        "Run the migrated launchd job 'banchan-day-reminder.prep' for routine_id=" +
          ctx.routine.id,
        "Invoke the existing shell pipeline exactly as launchd does:",
        "  /Users/itismyfield/.local/bin/banchan-day-reminder-prep.sh",
        "The skill performs calendar lookup; NO_REPLY is the correct result on",
        "non-반찬데이 days. Do not second-guess the skill's calendar logic.",
        "Return a one-line status summary (success | NO_REPLY | error: <msg>).",
      ].join("\n"),
    };
  },
});
