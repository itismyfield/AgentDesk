const test = require("node:test");
const assert = require("node:assert/strict");

const { createExecRouter, createSqlRouter, loadPolicy, toPlain } = require("./support/harness");

function findMetadataExecution(state, extraPredicate) {
  return state.executions.find((execution) => (
    /UPDATE kanban_cards SET metadata = \?/.test(execution.sql) &&
    (!extraPredicate || extraPredicate(execution))
  ));
}

function assertMetadataObjectParam(execution) {
  assert.ok(execution, "expected a metadata UPDATE execution");
  const written = execution.params[0];
  assert.equal(typeof written, "object");
  assert.equal(Array.isArray(written), false);
  return written;
}

test("kanban-rules preflight uses typed facade agentdesk.cards.get", () => {
  const { module, state } = loadPolicy("policies/kanban-rules.js", {
    dbQuery: createSqlRouter([
      {
        match: "SELECT id FROM task_dispatches WHERE kanban_card_id = ? AND dispatch_type = 'implementation' AND status = 'completed'",
        result: []
      }
    ]),
    cards: {
      "card-facade": {
        id: "card-facade",
        description: "A description that is long enough to pass the length check.",
        metadata: "{}",
      }
    }
  });

  const result = toPlain(module.__test.runPreflight("card-facade"));
  assert.equal(result.status, "assumption_ok");
  assert.equal(state.queries.length, 1);
  assert.match(state.queries[0].sql, /FROM task_dispatches/);
  assert.doesNotMatch(state.queries[0].sql, /FROM kanban_cards/);
});

test("kanban-rules preflight missing facade card returns invalid without fallback side effects", () => {
  const { module, state } = loadPolicy("policies/kanban-rules.js", {
    dbQuery: createSqlRouter([]),
    exec: createExecRouter([]),
    cards: {}
  });

  const result = toPlain(module.__test.runPreflight("missing-card"));
  assert.deepEqual(result, { status: "invalid", summary: "Card not found" });
  assert.deepEqual(state.queries, []);
  assert.deepEqual(state.execCalls, []);
});

test("kanban-rules preflight returns already_applied when the linked GitHub issue is closed", () => {
  const { module } = loadPolicy("policies/kanban-rules.js", {
    dbQuery: createSqlRouter([]),
    cards: {
      "card-1": {
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
    },
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

test("kanban-rules writes consultation-clear metadata as a JSON object param", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    cards: {
      "card-consult": {
        id: "card-consult",
        title: "Consulted card",
        status: "in_progress",
        priority: "medium",
        assigned_agent_id: "agent-1",
        deferred_dod_json: null
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches WHERE id = ?",
        result: [
          {
            id: "dispatch-consult",
            kanban_card_id: "card-consult",
            to_agent_id: "agent-1",
            dispatch_type: "consultation",
            chain_depth: 0,
            created_at: "2026-05-06 09:00:00",
            result: JSON.stringify({ verdict: "clear", summary: "ready" }),
            context: "{}",
            status: "completed"
          }
        ]
      },
      {
        match: "SELECT id, title, status, priority, assigned_agent_id, deferred_dod_json FROM kanban_cards WHERE id = ?",
        result: [
          {
            id: "card-consult",
            title: "Consulted card",
            status: "in_progress",
            priority: "medium",
            assigned_agent_id: "agent-1",
            deferred_dod_json: null
          }
        ]
      },
      {
        match: "SELECT metadata FROM kanban_cards WHERE id = ?",
        result: [{ metadata: JSON.stringify({ keep: "yes" }) }]
      },
      { match: "FROM auto_queue_entries e", result: [] }
    ])
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-consult" });

  const written = assertMetadataObjectParam(findMetadataExecution(state));
  assert.equal(written.keep, "yes");
  assert.equal(written.consultation_status, "completed");
  assert.deepEqual(written.consultation_result, { verdict: "clear", summary: "ready" });
  assert.equal(written.preflight_status, "clear");
  assert.equal(written.preflight_summary, "Consultation resolved: ready");
  assert.equal(state.queries.some((query) => /title, status, priority/.test(query.sql)), false);
});

test("kanban-rules writes consultation-escalated metadata as a JSON object param", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    cards: {
      "card-consult-blocked": {
        id: "card-consult-blocked",
        title: "Blocked consultation card",
        status: "in_progress",
        priority: "medium",
        assigned_agent_id: "agent-1",
        deferred_dod_json: null
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches WHERE id = ?",
        result: [
          {
            id: "dispatch-consult-blocked",
            kanban_card_id: "card-consult-blocked",
            to_agent_id: "agent-1",
            dispatch_type: "consultation",
            chain_depth: 0,
            created_at: "2026-05-06 09:00:00",
            result: JSON.stringify({ verdict: "blocked", summary: "ambiguous" }),
            context: "{}",
            status: "completed"
          }
        ]
      },
      {
        match: "SELECT id, title, status, priority, assigned_agent_id, deferred_dod_json FROM kanban_cards WHERE id = ?",
        result: [
          {
            id: "card-consult-blocked",
            title: "Blocked consultation card",
            status: "in_progress",
            priority: "medium",
            assigned_agent_id: "agent-1",
            deferred_dod_json: null
          }
        ]
      },
      {
        match: "SELECT metadata FROM kanban_cards WHERE id = ?",
        result: [{ metadata: JSON.stringify({ keep: "yes" }) }]
      }
    ])
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-consult-blocked" });

  const written = assertMetadataObjectParam(
    findMetadataExecution(state, (execution) => execution.sql.includes("blocked_reason = ?"))
  );
  assert.equal(written.keep, "yes");
  assert.equal(written.consultation_status, "completed");
  assert.deepEqual(written.consultation_result, { verdict: "blocked", summary: "ambiguous" });
  assert.equal(written.preflight_status, "escalated");
  assert.equal(written.preflight_summary, "Consultation did not resolve: ambiguous");
  assert.equal(state.manualInterventions.length, 1);
});

test("kanban-rules writes noop work-resolution metadata as a JSON object param", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    cards: {
      "card-noop": {
        id: "card-noop",
        title: "Noop card",
        status: "in_progress",
        priority: "medium",
        assigned_agent_id: "agent-1",
        deferred_dod_json: null
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches WHERE id = ?",
        result: [
          {
            id: "dispatch-noop",
            kanban_card_id: "card-noop",
            to_agent_id: "agent-1",
            dispatch_type: "implementation",
            chain_depth: 0,
            created_at: "2026-05-06 09:00:00",
            result: JSON.stringify({
              work_outcome: "noop",
              completed_without_changes: true,
              card_status_target: "review"
            }),
            context: "{}",
            status: "completed"
          }
        ]
      },
      {
        match: "SELECT id, title, status, priority, assigned_agent_id, deferred_dod_json FROM kanban_cards WHERE id = ?",
        result: [
          {
            id: "card-noop",
            title: "Noop card",
            status: "in_progress",
            priority: "medium",
            assigned_agent_id: "agent-1",
            deferred_dod_json: null
          }
        ]
      },
      {
        match: "SELECT metadata FROM kanban_cards WHERE id = ?",
        result: [{ metadata: JSON.stringify({ keep: "yes" }) }]
      }
    ])
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-noop" });

  const written = assertMetadataObjectParam(findMetadataExecution(state));
  assert.equal(written.keep, "yes");
  assert.equal(written.work_resolution_status, "noop");
  assert.equal(written.work_resolution_result.work_outcome, "noop");
  assert.equal(written.work_resolution_result.completed_without_changes, true);
  assert.equal(written.preflight_status, null);
  assert.equal(written.consultation_status, null);
});

test("kanban-rules preflight already_applied transitions the card to done and skips pending queue entries", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    cards: {
      "card-2": {
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
    },
    dbQuery: createSqlRouter([
      { match: "SELECT metadata FROM kanban_cards WHERE id = ?", result: [] },
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
    cards: {
      "card-3": {
        id: "card-3",
        title: "Rework card",
        status: "in_progress",
        priority: "medium",
        assigned_agent_id: "agent-3",
        deferred_dod_json: null
      }
    },
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
    cards: {
      "card-4": {
        id: "card-4",
        title: "DoD incomplete card",
        status: "in_progress",
        priority: "high",
        assigned_agent_id: "agent-4",
        deferred_dod_json: {
          items: ["add tests", "update docs"],
          verified: ["add tests"]
        }
      }
    },
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

test("kanban-rules ignores malformed DoD verified shape from facade card", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    cards: {
      "card-dod-malformed": {
        id: "card-dod-malformed",
        title: "Malformed DoD card",
        status: "in_progress",
        priority: "medium",
        assigned_agent_id: "agent-4",
        deferred_dod_json: {
          items: ["add tests"],
          verified: { "add tests": true }
        }
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches WHERE id = ?",
        result: [
          {
            id: "dispatch-dod-malformed",
            kanban_card_id: "card-dod-malformed",
            to_agent_id: "agent-4",
            dispatch_type: "implementation",
            chain_depth: 0,
            created_at: "2026-04-22 16:15:00",
            result: "{}",
            context: "{}",
            status: "completed"
          }
        ]
      }
    ])
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-dod-malformed" });

  assert.deepEqual(state.statusCalls, [{ cardId: "card-dod-malformed", status: "review", force: false }]);
  assert.deepEqual(state.reviewStatusCalls, []);
  assert.deepEqual(state.manualInterventions, []);
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

// ── #3605 (T2): scope-assessment dispatch ───────────────────────

function findScopeAssessmentMeta(state) {
  // The scope-assessment trigger writes a metadata UPDATE carrying
  // scope_assessment_status; preflight also writes one (without it). Pick the
  // execution whose object param actually has the scope marker.
  return state.executions
    .filter((execution) => /UPDATE kanban_cards SET metadata = \?/.test(execution.sql))
    .map((execution) => execution.params[0])
    .find((written) => written && typeof written === "object" && (
      written.scope_assessment_status != null || written.scope_depth != null
    ));
}

test("kanban-rules dispatches scope-assessment once when a card enters requested and preflight clears", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    cards: {
      "card-scope": {
        id: "card-scope",
        title: "Scope card",
        github_issue_number: null,
        github_issue_url: null,
        status: "requested",
        description: "This issue body is comfortably longer than thirty characters of text.",
        assigned_agent_id: "agent-7",
        metadata: "{}",
        blocked_reason: null
      }
    },
    dbQuery: createSqlRouter([
      { match: "SELECT metadata FROM kanban_cards WHERE id = ?", result: [{ metadata: "{}" }] },
      {
        match: "SELECT id FROM task_dispatches WHERE kanban_card_id = ? AND dispatch_type = 'implementation' AND status = 'completed'",
        result: []
      },
      {
        match: "SELECT assigned_agent_id, title FROM kanban_cards WHERE id = ?",
        result: [{ assigned_agent_id: "agent-7", title: "Scope card" }]
      }
    ])
  });

  policy.onCardTransition({ card_id: "card-scope", from: "backlog", to: "requested" });

  assert.equal(state.dispatchCreates.length, 1);
  const created = state.dispatchCreates[0];
  assert.equal(created.dispatchType, "scope-assessment");
  assert.equal(created.agentId, "agent-7");
  assert.match(created.title, /^\[Scope Assessment\]/);

  const scopeMeta = findScopeAssessmentMeta(state);
  assert.ok(scopeMeta, "expected a scope metadata write");
  assert.equal(scopeMeta.scope_assessment_status, "pending");
  assert.ok(scopeMeta.scope_assessment_dispatch_id);
  // T2 is inert: no escalation/manual intervention from the trigger.
  assert.deepEqual(state.manualInterventions, []);
});

test("kanban-rules does not dispatch scope-assessment twice when status already set", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    cards: {
      "card-scope-dup": {
        id: "card-scope-dup",
        title: "Scope dup card",
        github_issue_number: null,
        github_issue_url: null,
        status: "requested",
        description: "This issue body is comfortably longer than thirty characters of text.",
        assigned_agent_id: "agent-7",
        metadata: JSON.stringify({ scope_assessment_status: "pending" }),
        blocked_reason: null
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "SELECT metadata FROM kanban_cards WHERE id = ?",
        result: [{ metadata: JSON.stringify({ scope_assessment_status: "pending" }) }]
      },
      {
        match: "SELECT id FROM task_dispatches WHERE kanban_card_id = ? AND dispatch_type = 'implementation' AND status = 'completed'",
        result: []
      },
      {
        match: "SELECT assigned_agent_id, title FROM kanban_cards WHERE id = ?",
        result: [{ assigned_agent_id: "agent-7", title: "Scope dup card" }]
      }
    ])
  });

  policy.onCardTransition({ card_id: "card-scope-dup", from: "backlog", to: "requested" });

  assert.equal(state.dispatchCreates.length, 0);
});

test("kanban-rules skips scope-assessment when the card has no assigned agent (#3605)", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    cards: {
      "card-scope-noassignee": {
        id: "card-scope-noassignee",
        title: "Unassigned scope card",
        github_issue_number: null,
        github_issue_url: null,
        status: "requested",
        // Long body containing "DoD" → preflight returns "clear" so the
        // scope-assessment trigger is reached (not short-circuited earlier).
        description: "This issue body is comfortably longer than thirty characters and lists a DoD.",
        assigned_agent_id: null,
        metadata: "{}",
        blocked_reason: null
      }
    },
    dbQuery: createSqlRouter([
      { match: "SELECT metadata FROM kanban_cards WHERE id = ?", result: [{ metadata: "{}" }] },
      {
        match: "SELECT id FROM task_dispatches WHERE kanban_card_id = ? AND dispatch_type = 'implementation' AND status = 'completed'",
        result: []
      },
      // _maybeDispatchScopeAssessment's own lookup: assignee is falsy → skip.
      {
        match: "SELECT assigned_agent_id, title FROM kanban_cards WHERE id = ?",
        result: [{ assigned_agent_id: null, title: "Unassigned scope card" }]
      }
    ])
  });

  policy.onCardTransition({ card_id: "card-scope-noassignee", from: "backlog", to: "requested" });

  // No assignee → cannot route to "the assigned agent": log + return, no dispatch.
  assert.equal(state.dispatchCreates.length, 0);
  // And no scope metadata is written (status stays unset so a later requested
  // entry can re-evaluate once an agent is assigned).
  assert.equal(findScopeAssessmentMeta(state), undefined);
  assert.ok(
    state.logs.info.some((line) => /no assigned agent — skipping scope-assessment/.test(line)),
    "expected the no-assignee skip to be logged"
  );
});

test("kanban-rules does not dispatch scope-assessment when preflight is already_applied", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    cards: {
      "card-scope-applied": {
        id: "card-scope-applied",
        title: "Already implemented scope card",
        github_issue_number: null,
        github_issue_url: null,
        status: "requested",
        description: "This body is long enough to pass the description length gate easily.",
        assigned_agent_id: "agent-7",
        metadata: null,
        blocked_reason: null
      }
    },
    dbQuery: createSqlRouter([
      { match: "SELECT metadata FROM kanban_cards WHERE id = ?", result: [] },
      {
        match: "SELECT id FROM task_dispatches WHERE kanban_card_id = ? AND dispatch_type = 'implementation' AND status = 'completed'",
        result: [{ id: "dispatch-old" }]
      },
      {
        match: "SELECT id FROM auto_queue_entries WHERE kanban_card_id = ? AND status = 'pending'",
        result: []
      }
    ])
  });

  policy.onCardTransition({ card_id: "card-scope-applied", from: "backlog", to: "requested" });

  assert.equal(state.dispatchCreates.length, 0);
});

test("kanban-rules does not dispatch scope-assessment when preflight is consult_required (codex R2 #3605)", () => {
  // A short, non-empty body (<30 chars) → preflight returns "consult_required":
  // the issue needs counterpart consultation FIRST. The scope-assessment trigger
  // must NOT fire here — scope is meaningless until the consultation clarifies the
  // issue, and emitting it would add a redundant side-path dispatch. Only
  // "clear"/"assumption_ok" (preflight cleared) warrant a pre-implementation
  // scope read.
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    cards: {
      "card-scope-consult": {
        id: "card-scope-consult",
        title: "Vague scope card",
        github_issue_number: null,
        github_issue_url: null,
        status: "requested",
        // < 30 chars, non-empty → consult_required (kanban-preflight Check 3).
        description: "Too short, needs consult",
        assigned_agent_id: "agent-7",
        metadata: "{}",
        blocked_reason: null
      }
    },
    dbQuery: createSqlRouter([
      { match: "SELECT metadata FROM kanban_cards WHERE id = ?", result: [{ metadata: "{}" }] },
      {
        match: "SELECT id FROM task_dispatches WHERE kanban_card_id = ? AND dispatch_type = 'implementation' AND status = 'completed'",
        result: []
      },
      // If the (buggy) trigger reached _maybeDispatchScopeAssessment, this is the
      // lookup it would run. Provide a valid assignee so the ONLY thing that can
      // stop a dispatch is the consult_required trigger-condition guard itself.
      {
        match: "SELECT assigned_agent_id, title FROM kanban_cards WHERE id = ?",
        result: [{ assigned_agent_id: "agent-7", title: "Vague scope card" }]
      }
    ])
  });

  policy.onCardTransition({ card_id: "card-scope-consult", from: "backlog", to: "requested" });

  // consult_required → no scope-assessment dispatch, no scope metadata written.
  assert.equal(state.dispatchCreates.length, 0);
  assert.equal(findScopeAssessmentMeta(state), undefined);
});

test("kanban-rules records scope-assessment result on the card metadata and stays inert", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    cards: {
      "card-scope-done": {
        id: "card-scope-done",
        title: "Scope done card",
        status: "requested",
        priority: "medium",
        assigned_agent_id: "agent-7",
        deferred_dod_json: null
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches WHERE id = ?",
        result: [
          {
            id: "dispatch-scope",
            kanban_card_id: "card-scope-done",
            to_agent_id: "agent-7",
            dispatch_type: "scope-assessment",
            chain_depth: 0,
            created_at: "2026-06-19 09:00:00",
            result: JSON.stringify({
              scope_depth: "plan_only",
              scope_reason: "medium change touching two modules",
              scope_risk: "could grow if migration needed"
            }),
            context: "{}",
            status: "completed"
          }
        ]
      },
      {
        match: "SELECT metadata FROM kanban_cards WHERE id = ?",
        result: [{ metadata: JSON.stringify({ keep: "yes", scope_assessment_status: "pending" }) }]
      }
    ])
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-scope" });

  const written = assertMetadataObjectParam(findMetadataExecution(state));
  assert.equal(written.keep, "yes");
  assert.equal(written.scope_depth, "plan_only");
  assert.equal(written.scope_reason, "medium change touching two modules");
  assert.equal(written.scope_risk, "could grow if migration needed");
  assert.equal(written.scope_assessment_status, "completed");
  assert.deepEqual(written.scope_assessment_result, {
    scope_depth: "plan_only",
    scope_reason: "medium change touching two modules",
    scope_risk: "could grow if migration needed"
  });
  // Inert: no status change, no review entry, no manual intervention.
  assert.deepEqual(state.statusCalls, []);
  assert.deepEqual(state.manualInterventions, []);
  // Guard: scope-assessment must not flow into the review/create-pr lifecycle,
  // so it never queries the PM-gate card-detail shape.
  assert.equal(state.queries.some((query) => /title, status, priority/.test(query.sql)), false);
});

test("kanban-rules falls back to full when scope-assessment result is unusable", () => {
  const cases = [
    { label: "empty object", result: "{}" },
    { label: "unparsable", result: "not json" },
    { label: "garbage depth", result: JSON.stringify({ scope_depth: "garbage" }) }
  ];

  for (const testCase of cases) {
    const { policy, state } = loadPolicy("policies/kanban-rules.js", {
      cards: {
        "card-scope-fb": {
          id: "card-scope-fb",
          title: "Scope fallback card",
          status: "requested",
          priority: "medium",
          assigned_agent_id: "agent-7",
          deferred_dod_json: null
        }
      },
      dbQuery: createSqlRouter([
        {
          match: "FROM task_dispatches WHERE id = ?",
          result: [
            {
              id: "dispatch-scope-fb",
              kanban_card_id: "card-scope-fb",
              to_agent_id: "agent-7",
              dispatch_type: "scope-assessment",
              chain_depth: 0,
              created_at: "2026-06-19 09:00:00",
              result: testCase.result,
              context: "{}",
              status: "completed"
            }
          ]
        },
        {
          match: "SELECT metadata FROM kanban_cards WHERE id = ?",
          result: [{ metadata: "{}" }]
        }
      ])
    });

    policy.onDispatchCompleted({ dispatch_id: "dispatch-scope-fb" });

    const written = assertMetadataObjectParam(findMetadataExecution(state));
    assert.equal(written.scope_depth, "full", testCase.label + ": depth should fall back to full");
    assert.match(written.scope_reason, /fallback to full/, testCase.label + ": reason diagnostic");
    assert.match(written.scope_risk, /fallback to full/, testCase.label + ": risk diagnostic");
    assert.equal(written.scope_assessment_status, "completed", testCase.label);
  }
});

test("kanban-rules normalizes scope_depth case and dashes", () => {
  const { policy, state } = loadPolicy("policies/kanban-rules.js", {
    cards: {
      "card-scope-norm": {
        id: "card-scope-norm",
        title: "Scope normalize card",
        status: "requested",
        priority: "medium",
        assigned_agent_id: "agent-7",
        deferred_dod_json: null
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches WHERE id = ?",
        result: [
          {
            id: "dispatch-scope-norm",
            kanban_card_id: "card-scope-norm",
            to_agent_id: "agent-7",
            dispatch_type: "scope-assessment",
            chain_depth: 0,
            created_at: "2026-06-19 09:00:00",
            result: JSON.stringify({ scope_depth: " Plan-Only ", scope_reason: "r", scope_risk: "k" }),
            context: "{}",
            status: "completed"
          }
        ]
      },
      {
        match: "SELECT metadata FROM kanban_cards WHERE id = ?",
        result: [{ metadata: "{}" }]
      }
    ])
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-scope-norm" });

  const written = assertMetadataObjectParam(findMetadataExecution(state));
  assert.equal(written.scope_depth, "plan_only");
  assert.equal(written.scope_reason, "r");
  assert.equal(written.scope_risk, "k");
});
