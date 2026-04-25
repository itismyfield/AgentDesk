const test = require("node:test");
const assert = require("node:assert/strict");

const { loadPolicy, createSqlRouter } = require("./support/harness");

// #1082: phase_gate.js installs agentdesk.phaseGate with a registry API and
// built-in checks (merge_verified / issue_closed / build_passed) that
// replicate the previously hard-coded phase-gate logic.

test("phase_gate registry installs with built-in gates", () => {
  const { agentdesk } = loadPolicy("policies/phase_gate.js", {});

  assert.ok(agentdesk.phaseGate, "phaseGate namespace should be installed");
  assert.equal(typeof agentdesk.phaseGate.register, "function");
  assert.equal(typeof agentdesk.phaseGate.evaluate, "function");

  const names = agentdesk.phaseGate.names();
  assert.ok(names.includes("merge_verified"));
  assert.ok(names.includes("issue_closed"));
  assert.ok(names.includes("build_passed"));
});

test("phase_gate evaluate returns unregistered for unknown gates", () => {
  const { agentdesk } = loadPolicy("policies/phase_gate.js", {});
  const result = agentdesk.phaseGate.evaluate("never_defined", { card_id: "c1" });
  assert.equal(result.passed, false);
  assert.match(result.reason, /phase_gate_unregistered:never_defined/);
});

test("phase_gate merge_verified built-in reads pr_merge_verified_at", () => {
  const { agentdesk } = loadPolicy("policies/phase_gate.js", {
    dbQuery: createSqlRouter([
      {
        match: "SELECT pr_merge_verified_at FROM kanban_cards",
        result: [{ pr_merge_verified_at: "2026-04-25T00:00:00Z" }]
      }
    ])
  });

  const result = agentdesk.phaseGate.evaluate("merge_verified", { card_id: "c1" });
  assert.equal(result.passed, true);
});

test("phase_gate custom register overrides and returns reason on failure", () => {
  const { agentdesk } = loadPolicy("policies/phase_gate.js", {});

  agentdesk.phaseGate.register("qa_passed", function (ctx) {
    return { passed: false, reason: "qa_not_run:" + ctx.card_id };
  });

  const result = agentdesk.phaseGate.evaluate("qa_passed", { card_id: "c42" });
  assert.equal(result.passed, false);
  assert.equal(result.reason, "qa_not_run:c42");
});

test("phase_gate evaluate wraps thrown errors", () => {
  const { agentdesk } = loadPolicy("policies/phase_gate.js", {});

  agentdesk.phaseGate.register("boom", function () {
    throw new Error("kapow");
  });

  const result = agentdesk.phaseGate.evaluate("boom", {});
  assert.equal(result.passed, false);
  assert.match(result.reason, /phase_gate_error:boom/);
});
