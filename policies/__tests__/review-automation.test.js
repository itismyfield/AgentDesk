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
  const table = seed.map((row) => Object.assign({ state: "create-pr", retry_count: 1, last_error: null, dispatch_generation: "", age_days: 1 }, row));
  return {
    query(sql) {
      const states = (/state IN \(([^)]*)\)/.exec(sql) || [null, "'create-pr'"])[1].split(",").map((s) => s.trim().replace(/'/g, ""));
      return table.filter((row) => states.indexOf(row.state) >= 0
        && (sql.indexOf("retry_count > 0") < 0 || row.retry_count > 0)
        && (sql.indexOf("NOT LIKE 'handed-off:%'") < 0 || String(row.last_error || "").indexOf("handed-off:") !== 0)
        && (sql.indexOf("'-30 days'") < 0 || row.age_days < 30)
      ).slice(0, 20).map((row) => ({ card_id: row.card_id, last_error: row.last_error, retry_count: row.retry_count,
        dispatch_generation: sql.indexOf("dispatch_generation FROM") >= 0 ? row.dispatch_generation : undefined }));
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
    { card_id: "card-live", retry_count: 0 },
    { card_id: "card-dup", last_error: "no_open_pr_found", dispatch_generation: "gen-d" },
    { card_id: "card-aged", last_error: "no_open_pr_found", age_days: 40 }
  ]);
  const { policy, state } = loadPolicy("policies/review-automation.js", {
    cards: { "card-old1": { id: "card-old1", status: "in_progress" }, "card-old2": { id: "card-old2", status: "done" } },
    dbQuery: createSqlRouter([{ match: "FROM pr_tracking", result: (sql) => fake.query(sql) }]),
    dbExecute: (sql, params) => { if (sql.indexOf(MARKER_SQL) >= 0) fake.execute(sql, params); }
  });

  state.kv.set("pr_create_handoff:card-dup:gen-d", "no_open_pr_found");
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
  // #5716 r5: card-dup is a kv dedup hit and card-aged is past the 30-day floor — neither is a handoff.
  assert.equal(state.logs.warn.filter((line) => line.indexOf("Handed off 2 stranded") >= 0).length, 1);
  assert.deepEqual(state.executions.filter((e) => e.sql.indexOf("SET updated_at = datetime('now') WHERE card_id") >= 0)
    .map((e) => e.params.join("/")), ["card-dup/gen-d", "card-dup/gen-d"], "a dedup hit is bumped behind newer candidates");
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

// #5716 slice B: handoffCreatePr / reuse-refresh / reseed each stamp a fresh dispatch_generation AND
// reset retry_count to 0, so every generation's first failure is retry_count=1 — only the generation
// tells them apart. The third call repeats gen-b: deduped, and deliberately left without the durable
// marker so a colliding key self-heals after the kv TTL. r5: calls 4 and 5 (no stamp / record op recorded
// nothing) must not fall back to the row's stale gen-b key, which its own alert already seeded. r6: call 5
// keeps its own stamp inside the record-failed namespace, so consecutive stamped record failures stay apart.
test("review-automation alerts again for a new create-pr failure generation", () => {
  let recorded = true;
  let retryCount = 0;
  const { agentdesk, state } = loadPolicy("policies/review-automation.js", {
    cards: { "card-cp3": { id: "card-cp3", status: "review" } },
    prTracking: { load: () => ({ card_id: "card-cp3", dispatch_generation: "gen-b" }) },
    extraAgentdesk: {
      reviewAutomation: { recordPrCreateFailure: () => (recorded ? { ok: true, retry_count: ++retryCount, escalated: false } : null) }
    }
  });

  agentdesk.reviewAutomation.markPrCreateFailed("card-cp3", "no_open_pr_found", "gen-a");
  retryCount = 0;
  agentdesk.reviewAutomation.markPrCreateFailed("card-cp3", "no_open_pr_found", "gen-b");
  agentdesk.reviewAutomation.markPrCreateFailed("card-cp3", "no_open_pr_found", "gen-b");
  agentdesk.reviewAutomation.markPrCreateFailed("card-cp3", "dispatch_failed: bridge down");
  recorded = false;
  agentdesk.reviewAutomation.markPrCreateFailed("card-cp3", "no_open_pr_found", "gen-b");

  assert.equal(state.deadlockAlerts.length, 4);
  assert.deepEqual([...state.kv.keys()], ["pr_create_handoff:card-cp3:gen-a", "pr_create_handoff:card-cp3:gen-b",
    "pr_create_handoff:card-cp3:pre:dispatch_failed", "pr_create_handoff:card-cp3:pre:record_failed:gen-b"]);
  assert.equal(state.executions.filter((e) => e.sql.indexOf(MARKER_SQL) >= 0).length, 4, "dedup skip writes no marker");
});

// #5716 slice B: the completion failure paths must not commit the cause with a separate upsert — a crash
// between that upsert and the retry_count bump left a recorded failure at retry_count=0, invisible forever.
test("review-automation commits a create-pr completion failure in one op with no pre-count upsert", () => {
  const upserts = [];
  const { policy, state } = loadPolicy("policies/review-automation.js", {
    cards: { "card-cp4": { id: "card-cp4", status: "review", assigned_agent_id: "agent-cp4" } },
    prTracking: {
      load: () => ({ card_id: "card-cp4", repo_id: "o/r", branch: "feat/x", dispatch_generation: "gen-x" }),
      upsert: (...args) => { upserts.push(args); return {}; },
      findOpenPrByBranch: () => null
    },
    dbQuery: createSqlRouter([
      { match: "FROM task_dispatches WHERE id = ?", result: [{ id: "d-cp4", kanban_card_id: "card-cp4", dispatch_type: "create-pr", result: null, context: '{"dispatch_generation":"gen-x"}' }] },
      { match: "SELECT repo_id, github_issue_url", result: [{ repo_id: "o/r", github_issue_url: null }] },
      { match: "AND dispatch_type IN ('implementation', 'rework')", result: [] },
      { match: "SELECT status FROM kanban_cards", result: [{ status: "review" }] }
    ]),
    extraAgentdesk: { reviewAutomation: { recordPrCreateFailure: () => ({ ok: true, retry_count: 1, escalated: false }) } }
  });

  policy.onDispatchCompleted({ dispatch_id: "d-cp4" });

  assert.deepEqual(upserts, [], "the cause must be written by recordPrCreateFailure alone");
  assert.equal(state.deadlockAlerts.length, 1);
  assert.deepEqual([...state.kv.keys()], ["pr_create_handoff:card-cp4:gen-x"]);
});

// #5716 r5: the record-only branch stamps no blocked_reason, so a record op that recorded nothing leaves the failure in neither table AND at retry_count 0 — invisible to the sweep. Hand off immediately.
test("review-automation hands off a create-pr failure the record op did not record", () => {
  const { policy, state } = loadPolicy("policies/review-automation.js", {
    cards: { "card-cp5": { id: "card-cp5", status: "in_progress", assigned_agent_id: "agent-cp5" } },
    prTracking: { load: () => ({ card_id: "card-cp5", repo_id: "o/r", branch: "feat/x", dispatch_generation: "gen-y" }), findOpenPrByBranch: () => null },
    dbQuery: createSqlRouter([
      { match: "FROM task_dispatches WHERE id = ?", result: [{ id: "d-cp5", kanban_card_id: "card-cp5", dispatch_type: "create-pr", result: null, context: '{"dispatch_generation":"gen-y"}' }] },
      { match: "SELECT repo_id, github_issue_url", result: [{ repo_id: "o/r", github_issue_url: null }] },
      { match: "AND dispatch_type IN ('implementation', 'rework')", result: [] },
      { match: "SELECT status FROM kanban_cards", result: [{ status: "in_progress" }] }
    ]),
    extraAgentdesk: { reviewAutomation: { recordPrCreateFailure: () => { throw new Error("tx begin failed"); } } }
  });

  policy.onDispatchCompleted({ dispatch_id: "d-cp5" });

  assert.deepEqual([...state.kv.keys()], ["pr_create_handoff:card-cp5:pre:record_failed:gen-y"]);
  assert.equal(state.statusCalls.length, 0, "the record-only branch must not terminalize");
  assert.equal(state.logs.info.filter((l) => l.indexOf("recorded on pr_tracking") >= 0).length, 0, "no false record log");
});

// #5716 r6 (gpt P1-1): folding every stamped record failure onto the failure class merged G1 and G2 into
// ONE 7-day dedup key, so G2's alert was deduped away — and the sweep cannot recover it either, because the
// rolled-back record leaves retry_count at 0. The stamp inside the namespace is what makes G2 audible.
test("review-automation alerts separately for each stamped generation whose failure record threw", () => {
  const { agentdesk, state } = loadPolicy("policies/review-automation.js", {
    cards: { "card-cp6": { id: "card-cp6", status: "review" } },
    prTracking: { load: () => ({ card_id: "card-cp6", dispatch_generation: "gen-1" }) },
    extraAgentdesk: { reviewAutomation: { recordPrCreateFailure: () => { throw new Error("tx begin failed"); } } }
  });
  agentdesk.reviewAutomation.markPrCreateFailed("card-cp6", "no_open_pr_found", "gen-1");
  agentdesk.reviewAutomation.markPrCreateFailed("card-cp6", "no_open_pr_found", "gen-2");

  assert.equal(state.deadlockAlerts.length, 2, "a second dispatch generation is a second handoff");
  assert.deepEqual([...state.kv.keys()], ["pr_create_handoff:card-cp6:pre:record_failed:gen-1",
    "pr_create_handoff:card-cp6:pre:record_failed:gen-2"]);
});

// #5716 r6 (gpt P1-2): a record op that recorded nothing leaves the handoff alert as the failure's ONLY
// trace (retry_count 0 hides the row from the sweep), so a later mutation throw must not consume it.
test("review-automation still hands off a create-pr failure when the terminal mutations throw", () => {
  const build = (dbExecute, kanban) => loadPolicy("policies/review-automation.js", {
    cards: { "card-cp7": { id: "card-cp7", status: "review" } },
    dbExecute,
    extraAgentdesk: Object.assign(
      { reviewAutomation: { recordPrCreateFailure: () => { throw new Error("tx begin failed"); } } }, kanban || {})
  });
  const onStatusThrow = build(undefined, { kanban: {
    setStatus: () => { throw new Error("setStatus failed"); }, setReviewStatus() {}, getCard: () => null } });
  assert.throws(() => onStatusThrow.agentdesk.reviewAutomation.markPrCreateFailed("card-cp7", "no_open_pr_found", "gen-s"));
  assert.equal(onStatusThrow.state.deadlockAlerts.length, 1, "a setStatus throw must not eat the only signal");

  const onMarkerThrow = build((sql) => {
    if (sql.indexOf("blocked_reason = ?") >= 0) throw new Error("blocked_reason UPDATE failed"); return { changes: 1 }; });
  assert.throws(() => onMarkerThrow.agentdesk.reviewAutomation.markPrCreateFailed("card-cp7", "no_open_pr_found", "gen-m"));
  assert.equal(onMarkerThrow.state.deadlockAlerts.length, 1, "nor may a blocked_reason UPDATE throw");
});

// #5716: notifyDeadlockManager is the durable create-PR handoff. agentdesk.message.queue returns
// {ok:true,id} or {error:"..."} (src/engine/ops/message_ops.rs), so an enqueue failure must surface as
// false instead of a claimed delivery. Loads the real 00-escalation.js — the harness stubs the helper out.
test("00-escalation notifyDeadlockManager reports enqueue failures as undelivered", () => {
  const source = require("fs").readFileSync(__dirname + "/../00-escalation.js", "utf8");
  const build = new Function("require", "module", "agentdesk", source + "; return { notifyDeadlockManager };");
  const withQueueResult = (queueResult) => build(require, {}, {
    registerPolicy() {}, config: { get: () => "chan-1" }, cards: { get: () => null },
    log: { warn() {}, info() {}, error() {}, debug() {} },
    message: { queue: () => queueResult },
    db: { query: () => [], execute() {} },
    kv: { get: () => null, set() {}, delete() {} }
  });

  assert.equal(withQueueResult({ error: "outbox unavailable" }).notifyDeadlockManager("m", "s"), false);
  assert.equal(withQueueResult({ ok: true, id: 7 }).notifyDeadlockManager("m", "s"), true);
});
