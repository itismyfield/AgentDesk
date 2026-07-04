agentdesk.routines.register({
  name: "script-only-summary",
  tick(ctx) {
    return {
      action: "complete",
      result: {
        scriptRef: ctx.routine.script_ref,
        runId: ctx.run.id,
      },
      lastResult: "script summary complete",
    };
  },
});
