agentdesk.routines.register({
  name: "monitoring-working-watchdog",
  tick(ctx) {
    return {
      action: "complete",
      result: {
        routineId: ctx.routine.id,
        status: "ok",
      },
      lastResult: "working watchdog complete",
    };
  },
});
