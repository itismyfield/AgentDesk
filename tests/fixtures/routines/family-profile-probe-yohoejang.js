agentdesk.routines.register({
  name: "family-profile-probe-yohoejang",
  tick(ctx) {
    const checkpoint = Object.assign({}, ctx.checkpoint || {});
    const plan = checkpoint.plan || {};
    const triggerDate = plan.date || String(ctx.now).slice(0, 10);

    delete checkpoint.lastTriggeredDate;
    checkpoint.pendingDelivery = {
      kind: "family-profile-probe",
      triggerDate,
      routineId: ctx.routine.id,
    };

    return {
      action: "agent",
      dmUserId: "586090878444240926",
      prompt: "Ask 요회장 one family profile follow-up question.",
      checkpoint,
      lastResult: "family profile probe pending delivery",
    };
  },
});
