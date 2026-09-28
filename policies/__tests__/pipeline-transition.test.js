const { test } = require("node:test");
const assert = require("node:assert");
const fs = require("fs");

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
  let moves = [];
  const { module } = loadPolicy("policies/pipeline.js", {
    pipeline: {
      resolveForCard: (cardId) => ({
        states: [{ id: "ready", terminal: false }],
        transitions: [{ from: "ready", type: "gated" }]
      }),
      enterStage: (cardId, triggerAfter) => {
        moves.push({ cardId, triggerAfter });
        return { status: "entered", stage: { id: 1, stage_name: "deploy" } };
      }
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
  assert.equal(moves.length, 0);

  module.onCardTransition({ card_id: "card-1", to: "ready" });
  assert.deepEqual(moves, [{ cardId: "card-1", triggerAfter: "ready" }]);
});

test("pipeline picks the stage itself on a binary without enterStage", () => {
  const executions = [];
  const { module } = loadPolicy("policies/pipeline.js", {
    pipeline: {
      resolveForCard: () => ({
        states: [{ id: "ready", terminal: false }],
        transitions: [{ from: "ready", type: "gated" }]
      })
    },
    cards: { get: (cardId) => ({ id: cardId, repo_id: "repo-1" }) },
    db: {
      query: (sql, params) => {
        assert.match(sql, /FROM pipeline_stages WHERE repo_id = \? AND trigger_after = \?/);
        assert.deepEqual(params, ["repo-1", "ready"]);
        return [{ id: 7, stage_name: "deploy", agent_override_id: null }];
      },
      execute: (sql, params) => {
        executions.push({ sql, params });
        return { changes: 1 };
      }
    }
  });

  module.onCardTransition({ card_id: "card-1", to: "ready" });
  assert.equal(executions.length, 1);
  assert.match(executions[0].sql, /SET pipeline_stage_id = \?/);
  assert.deepEqual(executions[0].params, [7, "card-1"]);
});
