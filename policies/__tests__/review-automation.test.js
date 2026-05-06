const test = require("node:test");
const assert = require("node:assert/strict");

const { createSqlRouter, loadPolicy } = require("./support/harness");

test("review-automation immediately terminals cards when review_enabled is false", () => {
  const { policy, state } = loadPolicy("policies/review-automation.js", {
    config: { review_enabled: false },
    cards: {
      "card-1": {
        id: "card-1",
        status: "review",
        review_status: null,
        assigned_agent_id: "agent-1"
      }
    }
  });

  policy.onReviewEnter({ card_id: "card-1" });

  assert.deepEqual(state.statusCalls, [{ cardId: "card-1", status: "done", force: true }]);
  assert.deepEqual(state.reviewStatusCalls, [
    {
      cardId: "card-1",
      reviewStatus: null,
      options: { blocked_reason: null }
    }
  ]);
});

test("review-automation auto-approves review entry when the assigned agent has no counter-model channel", () => {
  const { policy, state } = loadPolicy("policies/review-automation.js", {
    cards: {
      "card-2": {
        id: "card-2",
        status: "review",
        review_status: null,
        assigned_agent_id: "agent-2"
      }
    },
    counterChannels: {}
  });

  policy.onReviewEnter({ card_id: "card-2" });

  assert.deepEqual(state.statusCalls, [{ cardId: "card-2", status: "done", force: true }]);
  assert.deepEqual(state.reviewStatusCalls, [
    {
      cardId: "card-2",
      reviewStatus: null,
      options: { blocked_reason: null }
    }
  ]);
});

test("review-automation keeps canonical review state but defers dispatch creation while active work still exists", () => {
  const { policy, state } = loadPolicy("policies/review-automation.js", {
    hasActiveWork: true,
    cards: {
      "card-3": {
        id: "card-3",
        status: "review",
        review_status: null,
        assigned_agent_id: "agent-3"
      }
    },
    counterChannels: {
      "agent-3": "discord://counter-review"
    }
  });

  policy.onReviewEnter({ card_id: "card-3" });

  assert.deepEqual(state.reviewStatusCalls, [
    {
      cardId: "card-3",
      reviewStatus: "reviewing",
      options: {
        review_entered_at: "now",
        blocked_reason: null,
        exclude_status: "done"
      }
    }
  ]);
  assert.deepEqual(state.reviewStateSyncs, [
    {
      cardId: "card-3",
      status: "reviewing",
      options: { review_round: 1 }
    }
  ]);
  assert.equal(state.dispatchCreates.length, 0);
});

test("review-automation carries the completed work slot into review dispatch context", () => {
  const { policy, state } = loadPolicy("policies/review-automation.js", {
    cards: {
      "card-slot-review": {
        id: "card-slot-review",
        status: "review",
        review_status: null,
        assigned_agent_id: "agent-slot"
      }
    },
    counterChannels: {
      "agent-slot": "discord://counter-review"
    },
    dbQuery: createSqlRouter([
      {
        match: "AND dispatch_type IN ('implementation', 'rework')",
        result: [
          {
            id: "dispatch-work-slot",
            dispatch_type: "implementation",
            result: JSON.stringify({ completed_commit: "abc123" }),
            context: JSON.stringify({ slot_index: 2 })
          }
        ]
      }
    ])
  });

  policy.onReviewEnter({ card_id: "card-slot-review" });

  assert.deepEqual(state.dispatchCreates, [
    {
      cardId: "card-slot-review",
      agentId: "agent-slot",
      dispatchType: "review",
      title: "[Review R1] card-slot-review",
      context: {
        parent_dispatch_id: "dispatch-work-slot",
        slot_index: 2
      }
    }
  ]);
});

test("review-automation creates a review-decision dispatch when an auto-completed review has no verdict", () => {
  const { policy, state } = loadPolicy("policies/review-automation.js", {
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches WHERE id = ?",
        result: [
          {
            id: "review-dispatch-1",
            kanban_card_id: "card-4",
            dispatch_type: "review",
            result: JSON.stringify({ auto_completed: true }),
            context: "{}"
          }
        ]
      },
      {
        match: "FROM kanban_cards WHERE id = ?",
        result: [
          {
            assigned_agent_id: "agent-4",
            title: "Needs decision",
            github_issue_number: 925,
            status: "review"
          }
        ]
      }
    ])
  });

  policy.onDispatchCompleted({ dispatch_id: "review-dispatch-1" });

  assert.deepEqual(state.dispatchCreates, [
    {
      cardId: "card-4",
      agentId: "agent-4",
      dispatchType: "review-decision",
      title: "[Review Decision] #925 Needs decision",
      context: null
    }
  ]);
});

test("review-automation noop verification passes go terminal without creating a PR dispatch", () => {
  const { module, state } = loadPolicy("policies/review-automation.js", {
    dbQuery: createSqlRouter([
      {
        match: "SELECT status FROM kanban_cards WHERE id = ?",
        result: [{ status: "review" }]
      },
      {
        match: "WHERE id = ? AND kanban_card_id = ? AND dispatch_type = 'review' LIMIT 1",
        result: [{ context: JSON.stringify({ review_mode: "noop_verification" }) }]
      },
      {
        match: "SELECT pipeline_stage_id, repo_id FROM kanban_cards WHERE id = ?",
        result: [{ pipeline_stage_id: null, repo_id: null }]
      },
      {
        match: "AND dispatch_type IN ('implementation', 'rework')",
        result: []
      }
    ])
  });

  module.__test.processVerdict(
    "card-5",
    "pass",
    { verdict: "pass" },
    { review_dispatch_id: "review-dispatch-5" }
  );

  assert.deepEqual(state.reviewStatusCalls, [
    {
      cardId: "card-5",
      reviewStatus: null,
      options: { suggestion_pending_at: null }
    }
  ]);
  assert.deepEqual(state.reviewStateSyncs, [
    {
      cardId: "card-5",
      status: "idle",
      options: { last_verdict: "pass" }
    }
  ]);
  assert.deepEqual(state.statusCalls, [{ cardId: "card-5", status: "done", force: true }]);
  assert.equal(state.dispatchCreates.length, 0);
});
