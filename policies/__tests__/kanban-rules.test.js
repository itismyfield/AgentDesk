const test = require("node:test");
const assert = require("node:assert/strict");

const { createExecRouter, createSqlRouter, loadPolicy, toPlain } = require("./support/harness");

test("kanban-rules preflight returns already_applied when the linked GitHub issue is closed", () => {
  const { module } = loadPolicy("policies/kanban-rules.js", {
    dbQuery: createSqlRouter([
      {
        match: "FROM kanban_cards kc WHERE kc.id = ?",
        result: [
          {
            id: "card-1",
            title: "Closed issue card",
            github_issue_number: 925,
            github_issue_url: "https://github.com/itismyfield/AgentDesk/issues/925",
            status: "requested",
            description: "This issue body is comfortably longer than thirty characters.",
            assigned_agent_id: "agent-1",
            metadata: "{}",
            blocked_reason: null
          }
        ]
      }
    ]),
    exec: createExecRouter([
      {
        match: (cmd, args) =>
          cmd === "gh" &&
          args[0] === "issue" &&
          args[1] === "view" &&
          args.includes("--json") &&
          args.includes("state"),
        result: "CLOSED\n"
      }
    ])
  });

  const result = toPlain(module.__test.runPreflight("card-1"));
  assert.equal(result.status, "already_applied");
  assert.equal(result.summary, "GitHub issue #925 is closed");
});

test("kanban-rules skips preflight once for api_reopen cards and preserves other metadata", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    dbQuery: createSqlRouter([
      {
        match: "SELECT metadata FROM kanban_cards WHERE id = ?",
        result: [
          {
            metadata: JSON.stringify({
              skip_preflight_once: "api_reopen",
              keep: "value"
            })
          }
        ]
      }
    ])
  });

  policy.onCardTransition({ card_id: "card-1", from: "backlog", to: "requested" });

  assert.equal(state.executions.length, 1);
  assert.match(state.executions[0].sql, /UPDATE kanban_cards SET metadata = \?/);
  const written = state.executions[0].params[0];
  assert.equal(typeof written, "object");
  assert.equal(written.keep, "value");
  assert.equal(written.preflight_status, "skipped");
  assert.equal(written.preflight_summary, "Skipped for API reopen");
  assert.ok(typeof written.preflight_checked_at === "string" && written.preflight_checked_at.length > 0);
  assert.equal(written.skip_preflight_once, undefined);
  assert.equal(state.statusCalls.length, 0);
});

test("kanban-rules preflight already_applied transitions the card to done and skips pending queue entries", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    dbQuery: createSqlRouter([
      { match: "SELECT metadata FROM kanban_cards WHERE id = ?", result: [] },
      {
        match: "FROM kanban_cards kc WHERE kc.id = ?",
        result: [
          {
            id: "card-2",
            title: "Already implemented",
            github_issue_number: null,
            github_issue_url: null,
            status: "requested",
            description: "This body is long enough to pass the description length gate.",
            assigned_agent_id: "agent-1",
            metadata: null,
            blocked_reason: null
          }
        ]
      },
      {
        match: "SELECT id FROM task_dispatches WHERE kanban_card_id = ? AND dispatch_type = 'implementation' AND status = 'completed'",
        result: [{ id: "dispatch-1" }]
      },
      {
        match: "SELECT id FROM auto_queue_entries WHERE kanban_card_id = ? AND status = 'pending'",
        result: [{ id: "entry-1" }, { id: "entry-2" }]
      }
    ])
  });

  policy.onCardTransition({ card_id: "card-2", from: "backlog", to: "requested" });

  assert.deepEqual(state.statusCalls, [{ cardId: "card-2", status: "done", force: true }]);
  assert.deepEqual(state.autoQueueStatusUpdates, [
    { entryId: "entry-1", status: "skipped", reason: "preflight_invalid", extra: null },
    { entryId: "entry-2", status: "skipped", reason: "preflight_invalid", extra: null }
  ]);
});

test("kanban-rules sends completed rework dispatches directly back to review", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches WHERE id = ?",
        result: [
          {
            id: "dispatch-2",
            kanban_card_id: "card-3",
            to_agent_id: "agent-3",
            dispatch_type: "rework",
            chain_depth: 0,
            created_at: "2026-04-22 16:00:00",
            result: "{}",
            context: "{}",
            status: "completed"
          }
        ]
      },
      {
        match: "FROM kanban_cards WHERE id = ?",
        result: [
          {
            id: "card-3",
            title: "Rework card",
            status: "in_progress",
            priority: "medium",
            assigned_agent_id: "agent-3",
            deferred_dod_json: null
          }
        ]
      }
    ])
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-2" });

  assert.deepEqual(state.statusCalls, [{ cardId: "card-3", status: "review", force: false }]);
});

test("kanban-rules marks DoD-only gate failures as awaiting_dod instead of escalating immediately", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches WHERE id = ?",
        result: [
          {
            id: "dispatch-3",
            kanban_card_id: "card-4",
            to_agent_id: "agent-4",
            dispatch_type: "implementation",
            chain_depth: 1,
            created_at: "2026-04-22 16:10:00",
            result: "{}",
            context: "{}",
            status: "completed"
          }
        ]
      },
      {
        match: "FROM kanban_cards WHERE id = ?",
        result: [
          {
            id: "card-4",
            title: "DoD incomplete card",
            status: "in_progress",
            priority: "high",
            assigned_agent_id: "agent-4",
            deferred_dod_json: JSON.stringify({
              items: ["add tests", "update docs"],
              verified: ["add tests"]
            })
          }
        ]
      }
    ])
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-3" });

  assert.deepEqual(state.statusCalls, [{ cardId: "card-4", status: "review", force: false }]);
  assert.deepEqual(state.reviewStatusCalls, [
    {
      cardId: "card-4",
      reviewStatus: "awaiting_dod",
      options: { awaiting_dod_at: "now" }
    }
  ]);
  assert.deepEqual(state.reviewStateSyncs, [
    {
      cardId: "card-4",
      status: "awaiting_dod",
      options: {}
    }
  ]);
});

test("kanban-rules _loadCardAlertContext uses typed facade agentdesk.cards.get", () => {
  const { module } = loadPolicy("policies/kanban-rules.js", {
    dbQuery: createSqlRouter([]),
    cards: {
      "card-minimal": { id: "card-minimal" },
      "card-full": {
        id: "card-full",
        assigned_agent_id: "agent-123",
        title: "My Full Title",
        github_issue_number: 456
      }
    }
  });

  const loadAlertContext = module._loadCardAlertContext;

  assert.equal(loadAlertContext("card-missing"), null);

  assert.deepEqual(Object.assign({}, loadAlertContext("card-minimal")), {
    card_id: "card-minimal",
    assigned_agent_id: null,
    title: "card-minimal",
    github_issue_number: null
  });

  assert.deepEqual(Object.assign({}, loadAlertContext("card-full")), {
    card_id: "card-full",
    assigned_agent_id: "agent-123",
    title: "My Full Title",
    github_issue_number: 456
  });
});
