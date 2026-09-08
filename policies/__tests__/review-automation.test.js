const test = require("node:test");
const assert = require("node:assert/strict");

const { createExecRouter, createSqlRouter, loadPolicy } = require("./support/harness");

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
            result: JSON.stringify({
              completed_commit: "abc123",
              completed_worktree_path: "/repo",
              completed_branch: "wt/slot"
            }),
            context: JSON.stringify({ slot_index: 2, entry_id: "entry-slot" })
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
        entry_id: "entry-slot",
        slot_index: 2,
        reviewed_commit: "abc123",
        worktree_path: "/repo",
        branch: "wt/slot"
      }
    }
  ]);
});

test("review-automation creates a review-decision dispatch when an auto-completed review has no verdict", () => {
  const { policy, state } = loadPolicy("policies/review-automation.js", {
    cards: { "card-4": { id: "card-4", assigned_agent_id: "agent-4", title: "Needs decision", github_issue_number: 925, status: "review" } },
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
      },
      // #2051 Finding 26 (P2) — dedupe lookup. No pre-existing pending review-decision.
      {
        match: "AND dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched')",
        result: []
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
    cards: {
      "card-5": {
        id: "card-5",
        status: "review",
        pipeline_stage_id: null,
        repo_id: null
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "WHERE id = ? AND kanban_card_id = ? AND dispatch_type = 'review' LIMIT 1",
        result: [{ context: JSON.stringify({ review_mode: "noop_verification" }) }]
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

test("review-automation clears a completed pipeline stage after cards.get migration", () => {
  const { module, state } = loadPolicy("policies/review-automation.js", {
    cards: {
      "card-completed-stage": {
        id: "card-completed-stage",
        status: "review",
        pipeline_stage_id: "stage-complete",
        repo_id: null
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "SELECT status FROM kanban_cards WHERE id = ?",
        result: [{ status: "review" }]
      },
      {
        match: "WHERE id = ? AND kanban_card_id = ? AND dispatch_type = 'review' LIMIT 1",
        result: [{ context: JSON.stringify({ review_mode: "normal" }) }]
      },
      {
        match: "AND dispatch_type IN ('implementation', 'rework')",
        result: []
      }
    ])
  });

  module.__test.processVerdict(
    "card-completed-stage",
    "pass",
    { verdict: "pass" },
    { review_dispatch_id: "review-dispatch-completed-stage" }
  );

  assert.equal(
    state.executions.some(
      ({ sql, params }) =>
        sql.includes("SET pipeline_stage_id = NULL") &&
        params[0] === "card-completed-stage"
    ),
    true
  );
});

test("review-automation skips create-pr when reviewed work is already on origin mainline", () => {
  const { module, state } = loadPolicy("policies/review-automation.js", {
    cards: {
      "card-direct-push": {
        id: "card-direct-push",
        status: "review",
        pipeline_stage_id: null,
        repo_id: "itismyfield/AgentDesk"
      }
    },
    exec: createExecRouter([
      {
        match: (cmd, args) => cmd === "git" && args.includes("rev-parse") && args.includes("origin/main"),
        result: "abc123\n"
      },
      {
        match: (cmd, args) => cmd === "git" && args.includes("merge-base") && args.includes("abc123"),
        result: ""
      }
    ]),
    dbQuery: createSqlRouter([
      {
        match: "WHERE id = ? AND kanban_card_id = ? AND dispatch_type = 'review' LIMIT 1",
        result: [{ context: JSON.stringify({ review_mode: "normal" }) }]
      },
      {
        match: "trigger_after = 'review_pass'",
        result: []
      },
      {
        match: "AND dispatch_type IN ('implementation', 'rework')",
        result: [
          {
            id: "dispatch-direct-push",
            dispatch_type: "implementation",
            result: JSON.stringify({
              completed_commit: "abc123",
              completed_worktree_path: "/repo",
              completed_branch: "main"
            }),
            context: JSON.stringify({})
          }
        ]
      }
    ])
  });

  module.__test.processVerdict(
    "card-direct-push",
    "pass",
    { verdict: "pass" },
    { review_dispatch_id: "review-direct-push" }
  );

  assert.deepEqual(state.statusCalls, [{ cardId: "card-direct-push", status: "done", force: true }]);
  assert.equal(state.dispatchCreates.length, 0);
  assert.equal(
    state.logs.info.some((line) => line.includes("already on origin mainline")),
    true
  );
});

// #2051 Finding 6 (P1): without a round filter, the loader used to fall back
// to the newest review dispatch context — which could be an R1 noop record
// even though the card had moved on to R2. Confirm that when card.review_round
// is provided, the loader returns the matching round's context instead of the
// newest one.
test("loadLatestReviewDispatchContext returns the context matching card.review_round, not the newest", () => {
  const { module, state } = loadPolicy("policies/review-automation.js", {
    dbQuery: createSqlRouter([
      {
        match: "SELECT review_round FROM kanban_cards WHERE id = ?",
        result: [{ review_round: 2 }]
      },
      {
        // Newest-first order: a fresher R3 dispatch is listed before the
        // matching R2 dispatch. The loader must skip R3 and pick R2.
        match: "FROM task_dispatches WHERE kanban_card_id = ? AND dispatch_type = 'review'",
        result: [
          { context: JSON.stringify({ review_mode: "noop_verification", review_round_at_dispatch: 3 }), status: "completed" },
          { context: JSON.stringify({ review_mode: "normal", review_round_at_dispatch: 2 }), status: "completed" }
        ]
      }
    ])
  });

  const ctx = module.__test.loadLatestReviewDispatchContext("card-6", null);
  assert.equal(ctx.review_mode, "normal");
  assert.equal(ctx.review_round_at_dispatch, 2);
  // No warning expected when a matching round is found.
  assert.equal(state.logs.warn.length, 0);
});

test("loadLatestReviewDispatchContext falls back to newest and warns when no round matches", () => {
  const { module, state } = loadPolicy("policies/review-automation.js", {
    dbQuery: createSqlRouter([
      {
        match: "SELECT review_round FROM kanban_cards WHERE id = ?",
        result: [{ review_round: 5 }]
      },
      {
        match: "FROM task_dispatches WHERE kanban_card_id = ? AND dispatch_type = 'review'",
        result: [
          { context: JSON.stringify({ review_round_at_dispatch: 1 }), status: "completed" }
        ]
      }
    ])
  });

  const ctx = module.__test.loadLatestReviewDispatchContext("card-7", null);
  assert.equal(ctx.review_round_at_dispatch, 1);
  assert.equal(state.logs.warn.length, 1);
  assert.ok(/no review dispatch context matched/.test(state.logs.warn[0]));
});

// #2051 Finding 26 (P2): when an active review-decision dispatch already exists
// for the card, an auto-completed review fallback must NOT spawn another one.
test("review-automation dedupes review-decision dispatches when one is already pending", () => {
  const { policy, state } = loadPolicy("policies/review-automation.js", {
    cards: { "card-dup": { id: "card-dup", assigned_agent_id: "agent-dup", title: "Dup decision card", github_issue_number: 999, status: "review" } },
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches WHERE id = ?",
        result: [
          {
            id: "review-dispatch-dup",
            kanban_card_id: "card-dup",
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
            assigned_agent_id: "agent-dup",
            title: "Dup decision card",
            github_issue_number: 999,
            status: "review"
          }
        ]
      },
      // Pre-existing review-decision still pending → must short-circuit.
      {
        match: "AND dispatch_type = 'review-decision' AND status IN ('pending', 'dispatched')",
        result: [{ id: "existing-review-decision" }]
      }
    ])
  });

  policy.onDispatchCompleted({ dispatch_id: "review-dispatch-dup" });

  assert.equal(state.dispatchCreates.length, 0);
  assert.ok(state.logs.info.some(function (m) {
    return m.indexOf("already has an active review-decision dispatch") >= 0;
  }));
});

// #2051 Finding 21 (P2): max-round guard runs BEFORE recordEntry so the round
// is not committed when the cap has been hit. Reopen recovery depends on this:
// the card keeps its previous review_round and can resume.
test("review-automation skips recordEntry when shouldAdvanceRound would exceed max_review_rounds", () => {
  const { policy, state } = loadPolicy("policies/review-automation.js", {
    cards: {
      "card-cap": {
        id: "card-cap",
        status: "review",
        review_status: null,
        assigned_agent_id: "agent-cap"
      }
    },
    counterChannels: {
      "agent-cap": "discord://counter-cap"
    },
    config: { max_review_rounds: 3 },
    reviewEntryContext: {
      current_round: 3,
      completed_work_count: 4,
      should_advance_round: true,
      next_round: 4
    }
  });

  policy.onReviewEnter({ card_id: "card-cap" });

  // recordEntry must NOT have been called — the new round (4) was never committed.
  assert.equal(state.reviewRecordCalls.length, 0);
  // Manual intervention must have been raised so a reopen can resume cleanly.
  assert.equal(state.manualInterventions.length, 1);
  assert.equal(state.manualInterventions[0].cardId, "card-cap");
  assert.ok(/Max review rounds/.test(state.manualInterventions[0].reason));
});

// #5716 slice B: processTrackedMergeQueue was the only consumer of terminal
// state='create-pr' rows, so retry_count can no longer reach >= 3 on its own.
test("review-automation hands the first create-pr failure to an agent/operator", () => {
  const { agentdesk, state } = loadPolicy("policies/review-automation.js", {
    cards: { "card-cp1": { id: "card-cp1", status: "review", assigned_agent_id: "agent-cp1", github_issue_number: 5716 } },
    extraAgentdesk: {
      reviewAutomation: { recordPrCreateFailure: () => ({ ok: true, retry_count: 1, escalated: false }) }
    }
  });

  agentdesk.reviewAutomation.markPrCreateFailed("card-cp1", "no_open_pr_found");

  assert.deepEqual(state.statusCalls, [{ cardId: "card-cp1", status: "done", force: true }]);
  assert.equal(state.deadlockAlerts.length, 1);
  assert.match(state.deadlockAlerts[0].message, /Create-PR Handoff[\s\S]*no automatic retry/);
  assert.equal(state.executions.filter((e) => e.sql.indexOf("blocked_reason = ?") >= 0).pop().params[0], "pr:create_failed:no_open_pr_found");
});

// Fake pr_tracking that honours whichever predicates the sweep SQL actually
// carries, so dropping a clause changes the candidate set instead of passing
// against fixed mock rows.
function createPrTrackingFake(seed) {
  const table = seed.map((row) => Object.assign({ state: "create-pr", retry_count: 1, last_error: null }, row));
  return {
    query(sql) {
      const states = (/state IN \(([^)]*)\)/.exec(sql) || [null, "'create-pr'"])[1].split(",").map((s) => s.trim().replace(/'/g, ""));
      return table.filter((row) => states.indexOf(row.state) >= 0
        && (sql.indexOf("retry_count > 0") < 0 || row.retry_count > 0)
        && (sql.indexOf("NOT LIKE 'handed-off:%'") < 0 || String(row.last_error || "").indexOf("handed-off:") !== 0)
      ).slice(0, 20).map((row) => ({ card_id: row.card_id, last_error: row.last_error, retry_count: row.retry_count }));
    },
    execute(sql, params) {
      const row = table.find((item) => item.card_id === params[1]);
      if (row && ["create-pr", "escalated"].indexOf(row.state) >= 0) row.last_error = params[0];
    }
  };
}

const MARKER_SQL = "UPDATE pr_tracking SET last_error = ?";
test("review-automation sweeps stranded create-pr rows once and then drops them from the candidate set", () => {
  const fake = createPrTrackingFake([
    { card_id: "card-old1", last_error: "no_open_pr_found", retry_count: 1 },
    { card_id: "card-old2", state: "escalated", last_error: "no_open_pr_found", retry_count: 3 },
    { card_id: "card-live", retry_count: 0 }
  ]);
  const { policy, state } = loadPolicy("policies/review-automation.js", {
    cards: { "card-old1": { id: "card-old1", status: "in_progress" }, "card-old2": { id: "card-old2", status: "done" } },
    dbQuery: createSqlRouter([{ match: "FROM pr_tracking", result: (sql) => fake.query(sql) }]),
    dbExecute: (sql, params) => { if (sql.indexOf(MARKER_SQL) >= 0) fake.execute(sql, params); }
  });

  policy.onTick5min({});
  policy.onTick5min({});

  // card-old2 is 'escalated', card-old1's card never reached terminal, card-live
  // (retry_count 0) is an in-flight dispatch that must be left alone.
  assert.equal(state.deadlockAlerts.length, 2);
  assert.match(state.deadlockAlerts[0].message + state.deadlockAlerts[1].message, /card-old1[\s\S]*card-old2/);
  const markers = state.executions.filter((e) => e.sql.indexOf(MARKER_SQL) >= 0);
  assert.equal(markers.length, 2);
  assert.equal(markers[0].params[0], "handed-off:no_open_pr_found");
  const sweeps = state.queries.filter((q) => q.sql.indexOf("FROM pr_tracking") >= 0);
  assert.equal(sweeps.length, 2);
  assert.ok(sweeps[0].sql.indexOf("LIMIT 20") >= 0 && sweeps[0].sql.indexOf("retry_count > 0") >= 0);
});

test("review-automation keeps a create-pr failure retryable when the handoff alert is not delivered", () => {
  const { agentdesk, state } = loadPolicy("policies/review-automation.js", {
    cards: { "card-cp2": { id: "card-cp2", status: "review" } },
    globals: { notifyDeadlockManager: (message) => { state.deadlockAlerts.push({ message }); return false; } },
    extraAgentdesk: {
      reviewAutomation: { recordPrCreateFailure: () => ({ ok: true, retry_count: 1, escalated: false }) }
    }
  });

  agentdesk.reviewAutomation.markPrCreateFailed("card-cp2", "no_open_pr_found");

  assert.equal(state.deadlockAlerts.length, 1);
  assert.equal(state.kv.size, 0, "an undelivered alert must not seed the dedup key");
  assert.equal(state.executions.filter((e) => e.sql.indexOf(MARKER_SQL) >= 0).length, 0, "nor the durable marker");
});

test("review-automation alerts again for a new create-pr failure generation", () => {
  let retryCount = 0;
  const { agentdesk, state } = loadPolicy("policies/review-automation.js", {
    cards: { "card-cp3": { id: "card-cp3", status: "review" } },
    extraAgentdesk: {
      reviewAutomation: { recordPrCreateFailure: () => ({ ok: true, retry_count: ++retryCount, escalated: retryCount >= 3 }) }
    }
  });

  agentdesk.reviewAutomation.markPrCreateFailed("card-cp3", "no_open_pr_found");
  agentdesk.reviewAutomation.markPrCreateFailed("card-cp3", "no_open_pr_found");

  assert.equal(state.deadlockAlerts.length, 2);
  assert.deepEqual([...state.kv.keys()], ["pr_create_handoff:card-cp3:1", "pr_create_handoff:card-cp3:2"]);
});
