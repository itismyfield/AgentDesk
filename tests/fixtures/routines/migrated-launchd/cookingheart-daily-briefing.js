agentdesk.routines.register({
  name: "cookingheart-daily-briefing",
  metadata: {
    migrated_launchd: {
      entrypoint: "scripts/launchd-migrated/cookingheart-daily-briefing.sh",
    },
  },
  tick(ctx) {
    return {
      action: "agent",
      prompt: `Build CookingHeart daily briefing for ${ctx.now}.`,
      checkpoint: {
        runId: ctx.run.id,
      },
      lastResult: "daily briefing dispatched",
    };
  },
});
