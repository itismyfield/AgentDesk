agentdesk.routines.register({
  name: "agent-checkpoint-review",
  tick(ctx) {
    return {
      action: "agent",
      prompt: `Review routine checkpoint for ${ctx.routine.id}.`,
      checkpoint: {
        reviewedRunId: ctx.run.id,
      },
      lastResult: "checkpoint review dispatched",
    };
  },
});
