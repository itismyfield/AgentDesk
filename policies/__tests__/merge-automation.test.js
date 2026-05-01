const test = require("node:test");
const assert = require("node:assert/strict");

const { createSqlRouter, loadPolicy } = require("./support/harness");

test("merge automation blocks tracked PR when required phase evidence is missing for head SHA", () => {
  const { module } = loadPolicy("policies/merge-automation.js", {
    exec(cmd, args) {
      assert.equal(cmd, "gh");
      assert.equal(JSON.stringify(args.slice(0, 3)), JSON.stringify(["run", "list", "--branch"]));
      return JSON.stringify([{
        databaseId: 101,
        status: "completed",
        conclusion: "success",
        headSha: "abc123",
        event: "push"
      }]);
    },
    dbQuery: createSqlRouter([
      {
        match: "SELECT required_phases FROM issue_specs WHERE card_id = ?",
        result: [{ required_phases: JSON.stringify(["unreal-smoke", "api-regression"]) }]
      },
      {
        match: "SELECT id FROM test_phase_runs WHERE phase_key = ?",
        result(_sql, params) {
          return params[0] === "unreal-smoke" ? [{ id: "tpr-1" }] : [];
        }
      }
    ])
  });

  const readiness = module.__test.verifyTrackedPrMergeReadiness({
    card_id: "card-1",
    repo_id: "itismyfield/AgentDesk",
    branch: "work/card-1",
    head_sha: "abc123"
  }, "abc123");

  assert.equal(readiness.ok, false);
  assert.match(readiness.reason, /missing required phase evidence/);
  assert.match(readiness.reason, /api-regression/);
});

test("merge automation accepts tracked PR when CI and all required phase evidence pass", () => {
  const { module } = loadPolicy("policies/merge-automation.js", {
    exec() {
      return JSON.stringify([{
        databaseId: 102,
        status: "completed",
        conclusion: "success",
        headSha: "def456",
        event: "push"
      }]);
    },
    dbQuery: createSqlRouter([
      {
        match: "SELECT required_phases FROM issue_specs WHERE card_id = ?",
        result: [{ required_phases: ["unreal-smoke"] }]
      },
      {
        match: "SELECT id FROM test_phase_runs WHERE phase_key = ?",
        result: [{ id: "tpr-2" }]
      }
    ])
  });

  const readiness = module.__test.verifyTrackedPrMergeReadiness({
    card_id: "card-2",
    repo_id: "itismyfield/AgentDesk",
    branch: "work/card-2",
    head_sha: "def456"
  }, "def456");

  assert.equal(readiness.ok, true);
  assert.equal(readiness.run.databaseId, 102);
});
