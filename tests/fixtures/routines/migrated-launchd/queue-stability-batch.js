agentdesk.routines.register({
  name: "queue-stability-batch",
  metadata: {
    migrated_launchd: {
      entrypoint: "scripts/launchd-migrated/queue-stability-batch.sh",
    },
  },
  tick(ctx) {
    return {
      action: "agent",
      prompt: `Run queue stability batch for ${ctx.run.id}.`,
      checkpoint: {
        runId: ctx.run.id,
      },
      lastResult: "queue stability batch dispatched",
    };
  },
});
