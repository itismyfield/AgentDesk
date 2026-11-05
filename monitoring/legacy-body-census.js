// Deployment template: register and enable through the existing routine workflow.
agentdesk.routines.register({
  name: "legacy-body-census",
  tick(ctx) {
    const rows = ctx.observations || [];
    const observation = rows.find(row => row.kind === "legacy_body_census_report");
    const report = observation ? observation.report : {
      stage1: "INCONCLUSIVE", complete: false, reasons: ["REPORT_OBSERVATION_MISSING"]
    };
    return { action: "complete", result: report, lastResult: report.stage1 };
  },
});
