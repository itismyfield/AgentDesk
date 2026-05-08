const { test } = require("node:test");
const assert = require("node:assert");
const fs = require("fs");

function createSqlRouter(routes) {
  return function dbQuery(sql, params) {
    for (let r of routes) {
      if (typeof r.match === "string" && sql.includes(r.match)) {
        return typeof r.result === "function" ? r.result(params) : r.result || [];
      }
      if (r.match instanceof RegExp && r.match.test(sql)) {
        return typeof r.result === "function" ? r.result(params) : r.result || [];
      }
    }
    return [];
  };
}

function loadPolicy(path, mockContext) {
  const code = fs.readFileSync(path, "utf8");
  let registered = null;
  const agentdesk = {
    registerPolicy: (p) => { registered = p; },
    log: { info: () => {}, warn: () => {}, error: () => {} },
    ...mockContext
  };
  const fn = new Function("agentdesk", code);
  fn(agentdesk);
  return { module: registered, agentdesk };
}

test("pipeline onCardTransition uses typed facade agentdesk.cards.get", () => {
  let executeCalls = [];
  const { module } = loadPolicy("policies/pipeline.js", {
    db: {
      query: createSqlRouter([
        {
          match: "SELECT id, stage_name, agent_override_id FROM pipeline_stages",
          result: [{ id: "stage-1", stage_name: "deploy", agent_override_id: null }]
        }
      ]),
      execute: (sql, params) => { executeCalls.push({ sql, params }); }
    },
    pipeline: {
      resolveForCard: (cardId) => ({
        states: [{ id: "ready", terminal: false }],
        transitions: [{ from: "ready", type: "gated" }]
      })
    },
    cards: {
      get: (cardId) => {
        if (cardId === "card-missing") return null;
        if (cardId === "card-1") return { id: "card-1", repo_id: "repo-1" };
        return null;
      }
    }
  });

  module.onCardTransition({ card_id: "card-missing", to: "ready" });
  assert.equal(executeCalls.length, 0);

  module.onCardTransition({ card_id: "card-1", to: "ready" });
  assert.equal(executeCalls.length, 1);
  assert.ok(executeCalls[0].sql.includes("UPDATE kanban_cards SET pipeline_stage_id"));
});
