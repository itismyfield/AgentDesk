const test = require("node:test");
const assert = require("node:assert/strict");
const { loadPolicy, toPlain } = require("./support/harness");

for (const busy of [false, true]) {
  test(`auto-queue ${busy ? "defers" : "continues"} its group after an agent busy probe`, () => {
    const { module, agentdesk, state } = loadPolicy("policies/lib/auto-queue-lifecycle.js");
    agentdesk.db.query = (sql, params) => {
      if (sql.includes("total_remaining")) return [{ total_remaining: 2, group_remaining: 2 }];
      assert.match(sql, /FROM kanban_cards WHERE assigned_agent_id = \?/);
      assert.match(sql, /LIMIT 1$/);
      assert.deepEqual(Array.from(params), ["agent-a", "requested", "in_progress", "review"]);
      return busy ? [{ id: "active-card" }] : [];
    };
    module.continueRunAfterEntry("run-a", "agent-a", 2, null, null);
    assert.deepEqual(toPlain(state.autoQueueActivations), busy ? [] : [{
      runId: { run_id: "run-a", active_only: true, agent_id: "agent-a", thread_group: 2 },
    }]);
  });
}
