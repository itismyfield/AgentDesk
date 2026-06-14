// Example attach body:
// {
//   "script_ref": "agent-checkpoint-review.js",
//   "agent_id": "maker",
//   "execution_strategy": "fresh",
//   "schedule": "30 9 * * 1-5",
//   "timeout_secs": 120
// }
agentdesk.routines.register({
  name: "agent-checkpoint-review",
  tick(ctx) {
    const priorCount =
      ctx.checkpoint && Number.isFinite(ctx.checkpoint.agentReviewCount)
        ? ctx.checkpoint.agentReviewCount
        : 0;
    const agentReviewCount = priorCount + 1;
    const checkpoint = {
      agentReviewCount,
      requestedAt: ctx.now,
    };

    return {
      action: "agent",
      prompt: [
        "Review this routine checkpoint using only the fields in this message.",
        "Do not read files, run commands, browse, or inspect repository state.",
        "Reply in 1-3 concise bullets with: status, risk, and any operational follow-up.",
        `routine_id: ${ctx.routine.id}`,
        `routine_name: ${ctx.routine.name}`,
        `script_ref: ${ctx.routine.script_ref}`,
        `run_id: ${ctx.run.id}`,
        `agent_review_count: ${agentReviewCount}`,
        `fresh_context_guaranteed: ${ctx.routine.fresh_context_guaranteed}`,
      ].join("\n"),
      checkpoint,
    };
  },
});
