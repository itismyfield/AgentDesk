const test = require("node:test");
const assert = require("node:assert/strict");

const { loadPolicy } = require("./support/harness");

// triage-rules is route-only (issue #3598): it auto-assigns backlog cards by
// `agent:<id>` label and sets priority by `priority:*` label. It must NOT emit
// any PMD "classification request" ping. Unlabeled cards are a no-op.
//
// The default agentdesk mock does not expose cards.list/assign/setPriority or
// agents.get, so we inject them via extraAgentdesk and record their calls.
function loadTriage(cards, agents) {
  const assignCalls = [];
  const setPriorityCalls = [];
  const listCalls = [];
  const infoLogs = [];

  const { state, policy } = loadPolicy("policies/triage-rules.js", {
    extraAgentdesk: {
      cards: {
        list(filter) {
          listCalls.push(filter);
          return cards;
        },
        assign(cardId, agentId) {
          assignCalls.push({ cardId, agentId });
        },
        setPriority(cardId, priority) {
          setPriorityCalls.push({ cardId, priority });
        }
      },
      agents: {
        get(agentId) {
          return Object.prototype.hasOwnProperty.call(agents, agentId)
            ? agents[agentId]
            : null;
        }
      },
      log: {
        info(message) {
          infoLogs.push(String(message));
        },
        debug() {},
        warn() {},
        error() {}
      }
    }
  });

  assert.ok(policy, "triage-rules policy should be registered");
  assert.equal(typeof policy.onTick, "function", "policy should expose onTick");
  policy.onTick();

  return { state, assignCalls, setPriorityCalls, listCalls, infoLogs };
}

test("agent:<id> label assigns the card to the resolved agent", () => {
  const { state, assignCalls, setPriorityCalls } = loadTriage(
    [{ id: "card-1", metadata: { labels: "agent:alpha" } }],
    { alpha: { id: "alpha" } }
  );

  assert.deepEqual(assignCalls, [{ cardId: "card-1", agentId: "alpha" }]);
  assert.equal(setPriorityCalls.length, 0);
  // route-only: never pings PMD
  assert.equal(state.messageQueues.length, 0);
});

test("agent label resolves via ch- prefix fallback when exact match misses", () => {
  const { state, assignCalls } = loadTriage(
    [{ id: "card-2", metadata: { labels: "agent:beta" } }],
    { "ch-beta": { id: "ch-beta" } } // exact "beta" misses, "ch-beta" hits
  );

  assert.deepEqual(assignCalls, [{ cardId: "card-2", agentId: "ch-beta" }]);
  assert.equal(state.messageQueues.length, 0);
});

test("unlabeled card is a no-op (no assign, no priority, no PMD ping)", () => {
  const { state, assignCalls, setPriorityCalls } = loadTriage(
    [{ id: "card-3", metadata: { labels: "" } }],
    {}
  );

  assert.deepEqual(assignCalls, []);
  assert.deepEqual(setPriorityCalls, []);
  assert.equal(state.messageQueues.length, 0);
});

test("priority-only label sets priority without assigning (card stays unassigned)", () => {
  const { state, assignCalls, setPriorityCalls } = loadTriage(
    [{ id: "card-4", metadata: { labels: "priority:high" } }],
    {}
  );

  assert.deepEqual(setPriorityCalls, [{ cardId: "card-4", priority: "high" }]);
  assert.equal(assignCalls.length, 0);
  assert.equal(state.messageQueues.length, 0);
});

test("agent label that resolves to no agent does not assign", () => {
  const { assignCalls } = loadTriage(
    [{ id: "card-5", metadata: { labels: "agent:ghost" } }],
    {} // neither "ghost" nor "ch-ghost" resolves
  );

  assert.equal(assignCalls.length, 0);
});

test("regression: PMD classification ping is never emitted across mixed cards", () => {
  const { state } = loadTriage(
    [
      { id: "c-agent", metadata: { labels: "agent:alpha" } },
      { id: "c-prio", metadata: { labels: "priority:urgent" } },
      {
        id: "c-bare",
        metadata: { labels: "" },
        github_issue_url: "https://github.com/x/y/issues/1",
        github_issue_number: 1,
        title: "bare"
      }
    ],
    { alpha: { id: "alpha" } }
  );

  // No announce ping for any card, including the unlabeled one that previously
  // triggered a PMD classification request.
  assert.equal(state.messageQueues.length, 0);
});
