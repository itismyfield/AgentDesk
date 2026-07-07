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
            deferred_dod_json: {
              items: ["add tests", "update docs"],
              verified: ["add tests"]
            }
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
      },
      {
        // #3594 (T3, codex Finding 2): scope-assessment claims the card's pending
        // auto-queue entry onto its dispatch (consultation pattern).
        match: "FROM auto_queue_entries e JOIN auto_queue_runs r",
        result: [{ id: "entry-scope-1", agent_id: "agent-7" }]
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

  // #3594 (T3, codex Finding 2): the pending entry must be CLAIMED onto the
  // scope-assessment dispatch — marked `dispatched` + linked (via the Rust
  // updateEntryStatus path that writes auto_queue_entry_dispatch_history) — so
  // scope-completion's resume finds it and the depth gate (plan/plan-review) is
  // not bypassed by the activate fallback creating a plain implementation.
  const claim = state.autoQueueStatusUpdates.find(
    (u) => u.entryId === "entry-scope-1" && u.status === "dispatched"
  );
  assert.ok(claim, "scope-assessment must claim the pending entry as dispatched");
  assert.equal(claim.extra.dispatchId, scopeMeta.scope_assessment_dispatch_id);
  assert.equal(claim.reason, "scope_assessment_claim");

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
      },
      // #3594 (T3): no auto-queue entry exists for this card (manual/API path),
      // so the plan stage finds nothing to claim and creates no entry-bound
      // dispatch — the inert assertions below stay valid while still exercising
      // the plan_only depth branch. (The full chain with a real entry is covered
      // by the depth-gated-chain integration tests below.)
      { match: "FROM auto_queue_entries e", result: [] }
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
        },
        // #3594 (T3): no auto-queue entry for this card → full-fallback plan
        // stage creates no entry-bound dispatch (manual/API path).
        { match: "FROM auto_queue_entries e", result: [] }
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
      },
      // #3594 (T3): normalized plan_only depth triggers a plan dispatch; no
      // linked auto-queue entry → defers (no dispatch created).
      { match: "FROM auto_queue_entries e", result: [] }
    ])
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-scope-norm" });

  const written = assertMetadataObjectParam(findMetadataExecution(state));
  assert.equal(written.scope_depth, "plan_only");
  assert.equal(written.scope_reason, "r");
  assert.equal(written.scope_risk, "k");
});

// ── #3594 (T3): depth-gated flow ─────────────────────────────────────────────

// Build an onDispatchCompleted scenario for a scope/plan/plan-review dispatch
// with a single linked auto-queue entry so the next-dispatch is actually
// created (not deferred). `dispatchType` is the COMPLETED dispatch type;
// `result`/`context` are its JSON strings; `metadata` is the card's current
// metadata object (post-record for scope-assessment).
function loadFlowScenario(opts) {
  const cardId = opts.cardId || "card-flow";
  const dispatchId = opts.dispatchId || "dispatch-flow";
  const entries = Object.prototype.hasOwnProperty.call(opts, "entries")
    ? opts.entries
    : [{ id: "entry-flow", agent_id: "agent-flow" }];
  return loadPolicy("policies/kanban-rules.js", {
    cards: {
      [cardId]: {
        id: cardId,
        title: opts.title || "Flow card",
        status: opts.cardStatus || "requested",
        priority: "medium",
        assigned_agent_id: "agent-flow",
        deferred_dod_json: null
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches WHERE id = ?",
        result: [
          {
            id: dispatchId,
            kanban_card_id: cardId,
            to_agent_id: "agent-flow",
            dispatch_type: opts.dispatchType,
            chain_depth: 0,
            created_at: "2026-06-19 09:00:00",
            result: opts.result || "{}",
            context: opts.context || "{}",
            status: "completed"
          }
        ]
      },
      {
        match: "SELECT metadata FROM kanban_cards WHERE id = ?",
        result: [{ metadata: JSON.stringify(opts.metadata || {}) }]
      },
      { match: "FROM auto_queue_entries e", result: entries }
    ])
  });
}

test("T3: scope-assessment depth=direct creates an implementation dispatch directly", () => {
  const { policy, state } = loadFlowScenario({
    cardId: "card-direct",
    dispatchId: "dispatch-scope-direct",
    dispatchType: "scope-assessment",
    result: JSON.stringify({ scope_depth: "direct", scope_reason: "tiny", scope_risk: "low" }),
    // metadata AFTER _recordScopeAssessment writes scope_depth=direct.
    metadata: { scope_assessment_status: "pending", scope_depth: "direct" }
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-scope-direct" });

  assert.equal(state.dispatchCreates.length, 1, "direct should create exactly one dispatch");
  assert.equal(state.dispatchCreates[0].dispatchType, "implementation");
  assert.equal(state.dispatchCreates[0].cardId, "card-direct");
  // entry resumed as dispatched, not plan/plan-review.
  assert.equal(state.autoQueueStatusUpdates.length, 1);
  assert.equal(state.autoQueueStatusUpdates[0].status, "dispatched");
  assert.equal(state.autoQueueStatusUpdates[0].reason, "scope_gate_direct");
  // no plan-review channel, no review/PM flow.
  assert.deepEqual(state.statusCalls, []);
  assert.deepEqual(state.manualInterventions, []);
});

test("T3: scope-assessment depth=plan_only creates a plan dispatch carrying depth", () => {
  const { policy, state } = loadFlowScenario({
    cardId: "card-plan-only",
    dispatchId: "dispatch-scope-po",
    dispatchType: "scope-assessment",
    result: JSON.stringify({ scope_depth: "plan_only", scope_reason: "med", scope_risk: "mid" }),
    metadata: { scope_assessment_status: "pending", scope_depth: "plan_only" }
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-scope-po" });

  assert.equal(state.dispatchCreates.length, 1);
  assert.equal(state.dispatchCreates[0].dispatchType, "plan");
  // depth rides in the plan dispatch context so the plan-completion arm branches
  // without re-reading metadata.
  assert.equal(state.dispatchCreates[0].context.scope_depth, "plan_only");
  assert.equal(state.dispatchCreates[0].context.auto_queue, true);
  assert.equal(state.autoQueueStatusUpdates[0].status, "dispatched");
  assert.equal(state.autoQueueStatusUpdates[0].reason, "scope_gate_plan");
});

test("T3: scope-assessment depth=full creates a plan dispatch carrying full", () => {
  const { policy, state } = loadFlowScenario({
    cardId: "card-full",
    dispatchId: "dispatch-scope-full",
    dispatchType: "scope-assessment",
    result: JSON.stringify({ scope_depth: "full", scope_reason: "big", scope_risk: "high" }),
    metadata: { scope_assessment_status: "pending", scope_depth: "full" }
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-scope-full" });

  assert.equal(state.dispatchCreates.length, 1);
  assert.equal(state.dispatchCreates[0].dispatchType, "plan");
  assert.equal(state.dispatchCreates[0].context.scope_depth, "full");
});

test("T3: fail-safe — unknown/missing scope_depth metadata is treated as full (plan)", () => {
  // _recordScopeAssessment already normalizes to "full", but guard the gate too:
  // metadata with NO scope_depth must still resolve to the full flow → plan.
  const { policy, state } = loadFlowScenario({
    cardId: "card-unknown",
    dispatchId: "dispatch-scope-unk",
    dispatchType: "scope-assessment",
    result: "{}",
    metadata: { scope_assessment_status: "pending" } // no scope_depth at all
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-scope-unk" });

  assert.equal(state.dispatchCreates.length, 1);
  assert.equal(state.dispatchCreates[0].dispatchType, "plan", "unknown depth must take the cautious full flow → plan");
});

test("T3: plan completion (depth=plan_only in context) creates the implementation dispatch", () => {
  const { policy, state } = loadFlowScenario({
    cardId: "card-plan-done-po",
    dispatchId: "dispatch-plan-po",
    dispatchType: "plan",
    cardStatus: "in_progress",
    context: JSON.stringify({ scope_depth: "plan_only", auto_queue: true }),
    result: JSON.stringify({ plan: "design...", summary: "done" })
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-plan-po" });

  assert.equal(state.dispatchCreates.length, 1);
  assert.equal(state.dispatchCreates[0].dispatchType, "implementation", "plan_only plan completion → impl, no plan-review");
  assert.equal(state.autoQueueStatusUpdates[0].reason, "scope_gate_plan_done");
});

test("T3: plan completion (depth=full in context) publishes a plan-review dispatch", () => {
  const { policy, state } = loadFlowScenario({
    cardId: "card-plan-done-full",
    dispatchId: "dispatch-plan-full",
    dispatchType: "plan",
    cardStatus: "in_progress",
    context: JSON.stringify({ scope_depth: "full", auto_queue: true }),
    result: JSON.stringify({ plan: "big design...", summary: "done" })
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-plan-full" });

  assert.equal(state.dispatchCreates.length, 1);
  assert.equal(state.dispatchCreates[0].dispatchType, "plan-review", "full plan completion → plan-review");
  assert.equal(state.dispatchCreates[0].context.scope_depth, "full");
  // #3594 (T3, codex Finding 3): the plan body must be forwarded to the reviewer
  // so it actually reviews the plan (not just {scope_depth}).
  assert.equal(
    state.dispatchCreates[0].context.parent_plan,
    "big design...",
    "plan-review dispatch must carry the plan body as parent_plan"
  );
  assert.equal(state.autoQueueStatusUpdates[0].reason, "scope_gate_plan_review");
});

test("T3: plan-review pass → implementation dispatch", () => {
  const { policy, state } = loadFlowScenario({
    cardId: "card-pr-pass",
    dispatchId: "dispatch-pr-pass",
    dispatchType: "plan-review",
    cardStatus: "in_progress",
    context: JSON.stringify({ scope_depth: "full", auto_queue: true, parent_plan: "approved design body" }),
    result: JSON.stringify({ verdict: "pass", summary: "plan ok" })
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-pr-pass" });

  assert.equal(state.dispatchCreates.length, 1);
  assert.equal(state.dispatchCreates[0].dispatchType, "implementation", "plan-review pass → impl");
  // #3594 (T3, codex Finding 3): the approved plan (carried in the plan-review
  // context) must flow into the impl dispatch so the implementer sees it.
  assert.equal(
    state.dispatchCreates[0].context.parent_plan,
    "approved design body",
    "impl dispatch must carry the approved plan body forward"
  );
  assert.equal(state.autoQueueStatusUpdates[0].reason, "scope_gate_plan_review_pass");
});

test("T3: plan-review rework → re-plan dispatch", () => {
  const { policy, state } = loadFlowScenario({
    cardId: "card-pr-rework",
    dispatchId: "dispatch-pr-rework",
    dispatchType: "plan-review",
    cardStatus: "in_progress",
    context: JSON.stringify({ scope_depth: "full", auto_queue: true }),
    result: JSON.stringify({ verdict: "rework", notes: "missing migration step" })
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-pr-rework" });

  assert.equal(state.dispatchCreates.length, 1);
  assert.equal(state.dispatchCreates[0].dispatchType, "plan", "plan-review rework → re-plan");
  assert.equal(state.dispatchCreates[0].context.scope_depth, "full");
});

test("T3: plan-review with missing/ambiguous verdict re-plans (cautious default)", () => {
  const { policy, state } = loadFlowScenario({
    cardId: "card-pr-amb",
    dispatchId: "dispatch-pr-amb",
    dispatchType: "plan-review",
    cardStatus: "in_progress",
    context: JSON.stringify({ scope_depth: "full", auto_queue: true }),
    result: JSON.stringify({ summary: "forgot the verdict" })
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-pr-amb" });

  assert.equal(state.dispatchCreates.length, 1);
  assert.equal(state.dispatchCreates[0].dispatchType, "plan", "ambiguous verdict must NOT advance to impl — re-plan");
});

test("T3: scope/plan dispatches never enter the PM-gate / review lifecycle", () => {
  // A plan completion must early-return before the PM gate, so it never queries
  // the PM-gate card-detail shape nor sets review status.
  const { policy, state } = loadFlowScenario({
    cardId: "card-no-pmgate",
    dispatchId: "dispatch-plan-nopm",
    dispatchType: "plan",
    cardStatus: "in_progress",
    context: JSON.stringify({ scope_depth: "plan_only", auto_queue: true }),
    result: JSON.stringify({ plan: "x" })
  });

  policy.onDispatchCompleted({ dispatch_id: "dispatch-plan-nopm" });

  assert.deepEqual(state.statusCalls, [], "plan completion must not setStatus (no review advance)");
  assert.deepEqual(state.reviewStatusCalls, []);
  assert.deepEqual(state.manualInterventions, []);
});

// ── #3594 (T3) ★ depth-gated-chain integration tests ──────────────
//
// These drive the FULL multi-stage chain through the real kanban-rules
// onDispatchCompleted arms with a STATEFUL auto-queue entry, asserting the
// codex-required invariants end-to-end:
//   - full:      scope → plan (NOT impl) → plan-review → impl. Each stage keeps
//                the SAME entry alive (dispatched), only impl is the terminal
//                work dispatch. The plan body flows plan → plan-review → impl.
//   - plan_only: scope → plan → impl.
//   - direct:    scope → impl (no plan).
// The harness simulates the consultation-resume contract: a dispatch "claims" the
// entry by linking it (auto_queue_entry_dispatch_history) + marking it dispatched,
// and _findAutoQueueEntriesByDispatch returns the entry only while it is linked to
// the queried dispatch AND still `dispatched`. That is exactly the lifecycle the
// Rust updateEntryStatus path implements, so the JS chain is exercised faithfully.
function makeChainHarness(initialScopeMeta) {
  // Mutable simulation state.
  const entry = { id: "entry-chain", agent_id: "agent-chain", status: "pending", linkedDispatchId: null };
  let cardMeta = Object.assign({}, initialScopeMeta || {});
  const dispatches = {}; // id → { dispatch_type, context, result, status }
  let dispatchSeq = 0;

  function registerDispatch(dispatchType, context, result) {
    dispatchSeq += 1;
    const id = "dispatch-chain-" + dispatchSeq;
    dispatches[id] = {
      id,
      kanban_card_id: "card-chain",
      to_agent_id: "agent-chain",
      dispatch_type: dispatchType,
      chain_depth: 0,
      created_at: "2026-06-19 09:0" + dispatchSeq + ":00",
      context: context || "{}",
      result: result || "{}",
      status: "completed"
    };
    return id;
  }

  const harness = loadPolicy("policies/kanban-rules.js", {
    cards: {
      "card-chain": {
        id: "card-chain",
        title: "Chain card",
        status: "requested",
        priority: "medium",
        assigned_agent_id: "agent-chain",
        deferred_dod_json: null
      }
    },
    // dispatch.create registers a new (already-"completed") dispatch row so the
    // next onDispatchCompleted can read its context. Returns the new id.
    dispatchCreate(cardId, agentId, dispatchType, title, context) {
      const ctxJson = context ? JSON.stringify(context) : "{}";
      const id = registerDispatch(dispatchType, ctxJson, "{}");
      harness.state.dispatchCreates.push({ cardId, agentId, dispatchType, title, context: context || null, id });
      return id;
    },
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches WHERE id = ?",
        result: (sql, params) => {
          const row = dispatches[params[0]];
          return row ? [row] : [];
        }
      },
      {
        match: "SELECT metadata FROM kanban_cards WHERE id = ?",
        result: () => [{ metadata: JSON.stringify(cardMeta) }]
      },
      {
        // _findAutoQueueEntriesByDispatch(dispatchId, false): entry is returned
        // only while linked to the queried dispatch AND status 'dispatched'.
        match: (sql) => /LEFT JOIN auto_queue_entry_dispatch_history/.test(sql),
        result: (sql, params) => {
          const dispatchId = params[0];
          if (entry.status === "dispatched" && entry.linkedDispatchId === dispatchId) {
            return [{ id: entry.id, agent_id: entry.agent_id }];
          }
          return [];
        }
      },
      {
        // _findPendingEntryForCard: returns the pending entry by card.
        match: (sql) => /FROM auto_queue_entries e JOIN auto_queue_runs r/.test(sql),
        result: () => (entry.status === "pending" ? [{ id: entry.id, agent_id: entry.agent_id }] : [])
      }
    ]),
    dbExecute(sql, params) {
      // Capture card metadata writes so scope_assessment_status flips persist.
      if (/UPDATE kanban_cards SET metadata = \?/.test(sql) && params && typeof params[0] === "object") {
        cardMeta = Object.assign({}, params[0]);
      }
      return { changes: 1 };
    }
  });

  // updateEntryStatus mutates the simulated entry: 'dispatched' (re)links to the
  // given dispatchId; terminal statuses settle it.
  harness.agentdesk.autoQueue.updateEntryStatus = function (entryId, status, reason, opts) {
    harness.state.autoQueueStatusUpdates.push({ entryId, status, reason, extra: opts || null });
    if (entryId !== entry.id) return;
    entry.status = status;
    if (status === "dispatched" && opts && opts.dispatchId) {
      entry.linkedDispatchId = opts.dispatchId;
    }
  };

  return {
    policy: harness.module.policy,
    state: harness.state,
    entry,
    registerDispatch,
    patchDispatchResult(dispatchId, result) {
      if (dispatches[dispatchId]) {
        dispatches[dispatchId].result = typeof result === "string" ? result : JSON.stringify(result);
      }
    },
    setMeta(patch) { cardMeta = Object.assign({}, cardMeta, patch); },
    getMeta() { return cardMeta; }
  };
}

function lastCreate(state) {
  return state.dispatchCreates[state.dispatchCreates.length - 1];
}

test("T3 ★ full chain: scope→plan→plan-review→impl keeps one entry alive, only impl is terminal work, plan body flows through", () => {
  const h = makeChainHarness({ scope_assessment_status: "pending" });
  // Stage 0: scope-assessment dispatch already exists + claimed the entry
  // (as _maybeDispatchScopeAssessment does). Seed that linkage.
  const scopeId = h.registerDispatch("scope-assessment", "{}",
    JSON.stringify({ scope_depth: "full", scope_reason: "big", scope_risk: "high" }));
  h.entry.status = "dispatched";
  h.entry.linkedDispatchId = scopeId;

  // Stage 1: scope-assessment completes (depth=full) → plan (NOT impl).
  h.policy.onDispatchCompleted({ dispatch_id: scopeId });
  let c = lastCreate(h.state);
  assert.equal(c.dispatchType, "plan", "full scope completion must create a PLAN, not implementation");
  const planId = c.id;
  assert.equal(h.entry.status, "dispatched", "entry must stay alive (dispatched) after scope→plan");
  assert.equal(h.entry.linkedDispatchId, planId, "entry must re-link to the plan dispatch");

  // Stage 2: the SAME plan dispatch the chain created completes with a plan body
  // → plan-review carrying that body. Patch the created plan dispatch's result
  // (the agent's PATCH would set result.plan) and complete it by its id.
  h.patchDispatchResult(planId, { plan: "PLAN-BODY-XYZ", summary: "planned" });
  h.policy.onDispatchCompleted({ dispatch_id: planId });
  c = lastCreate(h.state);
  assert.equal(c.dispatchType, "plan-review", "full plan completion must create a plan-review");
  assert.equal(c.context.parent_plan, "PLAN-BODY-XYZ", "plan-review must carry the plan body forward");
  const reviewId = c.id;
  assert.equal(h.entry.status, "dispatched", "entry stays alive after plan→plan-review");
  assert.equal(h.entry.linkedDispatchId, reviewId, "entry re-links to plan-review dispatch");

  // Stage 3: the SAME plan-review dispatch passes → impl carrying the approved
  // plan body (which rode in the plan-review context, then forwarded to impl).
  h.patchDispatchResult(reviewId, { verdict: "pass", summary: "ok" });
  h.policy.onDispatchCompleted({ dispatch_id: reviewId });
  c = lastCreate(h.state);
  assert.equal(c.dispatchType, "implementation", "plan-review pass must create the implementation dispatch");
  assert.equal(c.context.parent_plan, "PLAN-BODY-XYZ", "impl must carry the approved plan body");
  const implId = c.id;
  assert.equal(h.entry.linkedDispatchId, implId, "entry re-links to the impl dispatch");
  assert.equal(h.entry.status, "dispatched", "entry bound to impl (review-enabled impl holds dispatched until card terminal)");

  // Exactly 3 staged dispatches were created across the whole chain: plan,
  // plan-review, implementation — no extra/duplicate dispatches.
  assert.deepEqual(
    h.state.dispatchCreates.map((d) => d.dispatchType),
    ["plan", "plan-review", "implementation"],
    "the full chain creates exactly plan → plan-review → implementation"
  );
  // Never escalated / never advanced via PM gate during the staged chain.
  assert.deepEqual(h.state.manualInterventions, []);
});

test("T3 ★ plan_only chain: scope→plan→impl (no plan-review)", () => {
  const h = makeChainHarness({ scope_assessment_status: "pending" });
  const scopeId = h.registerDispatch("scope-assessment", "{}",
    JSON.stringify({ scope_depth: "plan_only", scope_reason: "mid", scope_risk: "low" }));
  h.entry.status = "dispatched";
  h.entry.linkedDispatchId = scopeId;

  h.policy.onDispatchCompleted({ dispatch_id: scopeId });
  let c = lastCreate(h.state);
  assert.equal(c.dispatchType, "plan", "plan_only scope completion → plan");
  const planId = c.id;
  assert.equal(h.entry.linkedDispatchId, planId);

  // Complete the SAME plan dispatch (with a body) → impl, no plan-review.
  h.patchDispatchResult(planId, { plan: "PLAN-PO", summary: "planned" });
  h.policy.onDispatchCompleted({ dispatch_id: planId });
  c = lastCreate(h.state);
  assert.equal(c.dispatchType, "implementation", "plan_only plan completion → impl (no plan-review)");
  assert.equal(c.context.parent_plan, "PLAN-PO", "impl carries plan body on plan_only path too");
  assert.equal(h.entry.linkedDispatchId, c.id, "entry re-links to impl on plan_only path");
  assert.deepEqual(
    h.state.dispatchCreates.map((d) => d.dispatchType),
    ["plan", "implementation"],
    "plan_only chain creates exactly plan → implementation (no plan-review)"
  );
});

test("T3 ★ direct chain: scope→impl (no plan)", () => {
  const h = makeChainHarness({ scope_assessment_status: "pending" });
  const scopeId = h.registerDispatch("scope-assessment", "{}",
    JSON.stringify({ scope_depth: "direct", scope_reason: "tiny", scope_risk: "none" }));
  h.entry.status = "dispatched";
  h.entry.linkedDispatchId = scopeId;

  h.policy.onDispatchCompleted({ dispatch_id: scopeId });
  const c = lastCreate(h.state);
  assert.equal(c.dispatchType, "implementation", "direct scope completion → impl immediately (no plan)");
  assert.equal(h.entry.linkedDispatchId, c.id, "entry binds straight to the impl dispatch");
});
