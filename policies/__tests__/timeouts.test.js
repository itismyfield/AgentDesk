const test = require("node:test");
const assert = require("node:assert/strict");

const { createSqlRouter, loadPolicy, toPlain } = require("./support/harness");

function timestampMinutesAgo(minutes) {
  const d = new Date(Date.now() - minutes * 60 * 1000);
  const pad = (value) => String(value).padStart(2, "0");
  return [
    d.getFullYear(),
    pad(d.getMonth() + 1),
    pad(d.getDate())
  ].join("-") + " " + [
    pad(d.getHours()),
    pad(d.getMinutes()),
    pad(d.getSeconds())
  ].join(":");
}

test("timeouts helper module parses session channel names", () => {
  const { module } = loadPolicy("policies/timeouts.js");

  assert.equal(
    module.helpers.parseSessionChannelName("provider:AgentDesk-codex-project-agentdesk-dev", "codex"),
    "project-agentdesk"
  );
});

test("timeouts helper module identifies synthetic missing-inflight reattach placeholders", () => {
  const { module } = loadPolicy("policies/timeouts.js");

  const synthetic = {
    session_id: null,
    request_owner_user_id: 0,
    user_msg_id: 0,
    any_tool_used: false,
    has_post_tool_text: false,
    rebind_origin: true
  };

  assert.equal(module.helpers.isSyntheticMissingInflightReattachPlaceholder(synthetic), true);
  assert.equal(
    module.helpers.isSyntheticMissingInflightReattachPlaceholder({
      ...synthetic,
      request_owner_user_id: 123
    }),
    false
  );
  assert.equal(
    module.helpers.isSyntheticMissingInflightReattachPlaceholder({
      ...synthetic,
      any_tool_used: true
    }),
    false
  );
});

test("timeouts helper module ignores synthetic reattach placeholders for inflight progress", () => {
  const { module } = loadPolicy("policies/timeouts.js", {
    inflights: [
      {
        provider: "codex",
        channel_id: "channel-1",
        channel_name: "project-agentdesk",
        session_key: "provider:AgentDesk-codex-project-agentdesk",
        tmux_session_name: "AgentDesk-codex-project-agentdesk",
        session_id: null,
        request_owner_user_id: 0,
        user_msg_id: 0,
        any_tool_used: false,
        has_post_tool_text: false,
        rebind_origin: true,
        started_at: timestampMinutesAgo(95),
        updated_at: timestampMinutesAgo(95)
      }
    ]
  });

  const progress = module.helpers.inspectInflightProgress(
    "provider:AgentDesk-codex-project-agentdesk",
    "AgentDesk-codex-project-agentdesk",
    30,
    180
  );

  assert.equal(progress.inflight, null);
  assert.equal(progress.channel_id, null);
  assert.equal(progress.recent, false);
});

test("timeouts reconciliation module scans pending fallback dispatch keys", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    dbQuery: createSqlRouter([
      { match: "SELECT key, value FROM kv_meta WHERE key LIKE 'reconcile_dispatch:%'", result: [] }
    ])
  });

  policy._section_R();

  assert.match(state.queries[0].sql, /reconcile_dispatch:%/);
});

test("timeouts harness previewTimeoutDecision handles null payload without throwing", () => {
  const { agentdesk, state } = loadPolicy("policies/timeouts.js");
  let result = null;

  assert.doesNotThrow(() => {
    result = agentdesk.timeouts.previewTimeoutDecision(null);
  });

  assert.equal(state.timeoutPreviewCalls.length, 1);
  assert.deepEqual(state.timeoutPreviewCalls[0], {});
  assert.equal(result && typeof result, "object");
  assert.ok(
    Object.prototype.hasOwnProperty.call(result, "resolution") ||
      Object.prototype.hasOwnProperty.call(result, "error")
  );
});

test("timeouts card timeout module marks requested dispatches failed before retry", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { requested_timeout_min: 30 },
    timeouts: {
      previewTimeoutDecision(payload) {
        return {
          would_retry: true,
          would_exhaust: false,
          resolution: "retry",
          attempt: payload.attempt,
          delay: 300
        };
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "FROM kanban_cards kc LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id",
        result: [
          {
            id: "card-requested-1",
            assigned_agent_id: "agent-1",
            latest_dispatch_id: "dispatch-requested-1",
            retry_count: 2,
            dispatch_type: "implementation"
          }
        ]
      }
    ])
  });

  policy._section_A();

  assert.deepEqual(state.dispatchMarkFailedCalls, [
    { dispatchId: "dispatch-requested-1", reason: "Timed out waiting for agent" }
  ]);
  assert.equal(state.timeoutPreviewCalls.length, 1);
  assert.deepEqual(
    {
      card_id: state.timeoutPreviewCalls[0].card_id,
      status: state.timeoutPreviewCalls[0].status,
      state: state.timeoutPreviewCalls[0].state,
      latest_dispatch_id: state.timeoutPreviewCalls[0].latest_dispatch_id,
      attempt: state.timeoutPreviewCalls[0].attempt
    },
    {
      card_id: "card-requested-1",
      status: "requested",
      state: "requested",
      latest_dispatch_id: "dispatch-requested-1",
      attempt: 2
    }
  );
  const shadowLine = state.logs.info.find((line) => line.startsWith("[timeout_shadow] "));
  assert.ok(shadowLine);
  const shadow = JSON.parse(shadowLine.slice("[timeout_shadow] ".length));
  assert.equal(shadow.target, "agentdesk::timeout_shadow");
  assert.equal(shadow.card_id, "card-requested-1");
  assert.equal(shadow.section, "_section_A");
  assert.equal(shadow.js_decision, "retry");
  assert.equal(shadow.reducer_decision, "retry");
  assert.equal(shadow.agree, true);
  assert.match(state.executions[0].sql, /UPDATE kanban_cards SET requested_at/);
  assert.deepEqual(toPlain(state.executions[0].params), ["card-requested-1"]);
});

test("timeouts requested sweep skips consultation side-path dispatches (#256)", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { requested_timeout_min: 30 },
    dbQuery: createSqlRouter([
      {
        match: "FROM kanban_cards kc LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id",
        result: [
          {
            id: "card-consult-1",
            assigned_agent_id: "agent-1",
            latest_dispatch_id: "dispatch-consult-1",
            retry_count: 0,
            dispatch_type: "consultation"
          }
        ]
      }
    ])
  });

  policy._section_A();

  // Consultation side-path: never marked failed, never retried/escalated.
  assert.deepEqual(state.dispatchMarkFailedCalls, []);
  assert.deepEqual(state.manualInterventions, []);
  assert.equal(
    state.executions.filter((e) => /UPDATE kanban_cards SET requested_at/.test(e.sql)).length,
    0
  );
});

test("timeouts requested sweep skips scope-assessment side-path even when overdue (#3605)", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { requested_timeout_min: 30 },
    dbQuery: createSqlRouter([
      {
        match: "FROM kanban_cards kc LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id",
        result: [
          {
            id: "card-scope-1",
            assigned_agent_id: "agent-1",
            latest_dispatch_id: "dispatch-scope-1",
            retry_count: 0,
            dispatch_type: "scope-assessment"
          }
        ]
      }
    ])
  });

  policy._section_A();

  // #3605 (T2) inert side-path: the requested-timeout sweep must NOT mark the
  // scope-assessment dispatch failed nor retry/escalate the card, exactly like
  // consultation. A scope-assessment that lingers in `requested` is harmless.
  assert.deepEqual(state.dispatchMarkFailedCalls, []);
  assert.deepEqual(state.manualInterventions, []);
  assert.equal(
    state.executions.filter((e) => /UPDATE kanban_cards SET requested_at/.test(e.sql)).length,
    0
  );
});

test("timeouts dispatch maintenance shadows failed-dispatch retries without changing retry mutations", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    timeouts: {
      previewTimeoutDecision(payload) {
        return {
          would_retry: false,
          would_exhaust: false,
          resolution: "incomparable",
          attempt: payload.attempt,
          delay: null,
          incomparable: true,
          reason: "state missing"
        };
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches td JOIN kanban_cards kc ON kc.id = td.kanban_card_id",
        result: [
          {
            id: "dispatch-failed-1",
            kanban_card_id: "card-retry-1",
            to_agent_id: "agent-1",
            dispatch_type: "implementation",
            title: "Retry implementation",
            retry_count: 3,
            github_issue_url: null,
            github_issue_number: null
          }
        ]
      }
    ])
  });

  policy._section_J();

  assert.equal(state.timeoutPreviewCalls.length, 1);
  assert.deepEqual(
    {
      card_id: state.timeoutPreviewCalls[0].card_id,
      status: state.timeoutPreviewCalls[0].status,
      state: state.timeoutPreviewCalls[0].state,
      latest_dispatch_id: state.timeoutPreviewCalls[0].latest_dispatch_id,
      attempt: state.timeoutPreviewCalls[0].attempt
    },
    {
      card_id: "card-retry-1",
      status: null,
      state: null,
      latest_dispatch_id: "dispatch-failed-1",
      attempt: 3
    }
  );
  assert.deepEqual(state.dispatchCreates, [
    {
      cardId: "card-retry-1",
      agentId: "agent-1",
      dispatchType: "implementation",
      title: "Retry implementation",
      context: null
    }
  ]);
  assert.deepEqual(state.dispatchRetryCountCalls, [
    { dispatchId: "dispatch-1", count: 4 }
  ]);
  const shadowLine = state.logs.info.find((line) => line.startsWith("[timeout_shadow] "));
  assert.ok(shadowLine);
  const shadow = JSON.parse(shadowLine.slice("[timeout_shadow] ".length));
  assert.equal(shadow.target, "agentdesk::timeout_shadow");
  assert.equal(shadow.card_id, "card-retry-1");
  assert.equal(shadow.section, "_section_J");
  assert.equal(shadow.js_decision, "retry");
  assert.equal(shadow.reducer_decision, "incomparable");
  assert.equal(shadow.agree, false);
  assert.equal(shadow.incomparable, true);
});

test("timeouts reconcile fallback does not advance a completed scope-assessment (#3605)", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { pm_decision_gate_enabled: true },
    cards: {
      "card-scope-r": {
        id: "card-scope-r",
        status: "requested",
        priority: "medium",
        assigned_agent_id: "agent-1",
        deferred_dod_json: null
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "SELECT key, value FROM kv_meta WHERE key LIKE 'reconcile_dispatch:%'",
        result: [{ key: "reconcile_dispatch:dispatch-scope-r", value: "dispatch-scope-r" }]
      },
      {
        match: "SELECT id, kanban_card_id, to_agent_id, dispatch_type, chain_depth, status, result, context FROM task_dispatches WHERE id = ?",
        result: [
          {
            id: "dispatch-scope-r",
            kanban_card_id: "card-scope-r",
            to_agent_id: "agent-1",
            dispatch_type: "scope-assessment",
            chain_depth: 0,
            status: "completed",
            result: JSON.stringify({ scope_depth: "direct" }),
            context: "{}"
          }
        ]
      },

      {
        // #3605 (T2): the fallback now records scope_depth via the shared
        // recorder, which reads the card metadata first.
        match: "SELECT metadata FROM kanban_cards WHERE id = ?",
        result: [{ metadata: JSON.stringify({ scope_depth: "direct", scope_assessment_status: "completed" }) }]
      },
      // #3594 (T3): the fallback now also GATES the depth flow (direct → impl).
      // With no linked auto-queue entry it defers to the activate path (no
      // dispatch created), so the no-review-advance assertions below still hold.
      { match: "FROM auto_queue_entries e", result: [] }
    ])
  });

  policy._section_R();

  // Missed-hook fallback must mirror kanban-rules.js onDispatchCompleted: a
  // completed scope-assessment never advances the card to REVIEW (no setStatus
  // to review), grants no XP, and runs no PM-gate-driven status change. (T3 may
  // create a depth-gated NEXT dispatch, but only when an auto-queue entry is
  // linked — none here, so it defers.)
  assert.deepEqual(state.statusCalls, []);
  assert.deepEqual(state.reviewStatusCalls, []);
  assert.deepEqual(state.reviewStateSyncs, []);
  // XP UPDATE (agents SET xp = xp + ?) must not fire for a side-path.
  assert.equal(
    state.executions.filter((e) => /UPDATE agents SET xp = xp/.test(e.sql)).length,
    0
  );
  // #3605 (T2): but the fallback MUST still record scope_depth (parity with the
  // live hook) — previously it only `continue`d and lost the result entirely.
  const metaWrite = state.executions.find((e) =>
    /UPDATE kanban_cards SET metadata = \?/.test(e.sql)
  );
  assert.ok(metaWrite, "fallback must persist scope-assessment metadata");
  const meta = metaWrite.params[0];
  assert.equal(meta.scope_depth, "direct");
  assert.equal(meta.scope_assessment_status, "completed");
});

test("timeouts reconcile fallback applies full fallback for an unparsable scope-assessment (#3605)", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { pm_decision_gate_enabled: true },
    cards: {
      "card-scope-fb": {
        id: "card-scope-fb",
        status: "requested",
        priority: "medium",
        assigned_agent_id: "agent-1",
        deferred_dod_json: null
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "SELECT key, value FROM kv_meta WHERE key LIKE 'reconcile_dispatch:%'",
        result: [{ key: "reconcile_dispatch:dispatch-scope-fb", value: "dispatch-scope-fb" }]
      },
      {
        match: "SELECT id, kanban_card_id, to_agent_id, dispatch_type, chain_depth, status, result, context FROM task_dispatches WHERE id = ?",
        result: [
          {
            id: "dispatch-scope-fb",
            kanban_card_id: "card-scope-fb",
            to_agent_id: "agent-1",
            dispatch_type: "scope-assessment",
            chain_depth: 0,
            status: "completed",
            result: "not json at all",
            context: "{}"
          }
        ]
      },

      {
        match: "SELECT metadata FROM kanban_cards WHERE id = ?",
        result: [{ metadata: "{}" }]
      },
      // #3594 (T3): full-fallback depth gates to a plan dispatch; no linked
      // auto-queue entry → defers (no dispatch created), so the card stays inert.
      { match: "FROM auto_queue_entries e", result: [] }
    ])
  });

  policy._section_R();

  // Unparsable result → cautious "full" fallback, recorded even on the
  // missed-hook path; card never advances to review (no setStatus).
  assert.deepEqual(state.statusCalls, []);
  const metaWrite = state.executions.find((e) =>
    /UPDATE kanban_cards SET metadata = \?/.test(e.sql)
  );
  assert.ok(metaWrite, "fallback must persist scope metadata even when unparsable");
  assert.equal(metaWrite.params[0].scope_depth, "full");
  assert.match(metaWrite.params[0].scope_reason, /fallback to full/);
});

test("timeouts reconcile fallback gates depth flow when an auto-queue entry is linked (#3594 T3)", () => {
  // Parity with the live hook: a missed scope-assessment completion whose card
  // has a linked auto-queue entry must create the depth-gated next dispatch
  // (direct → implementation) so the run does not stall waiting for a hook that
  // was dropped.
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { pm_decision_gate_enabled: true },
    cards: {
      "card-scope-g": {
        id: "card-scope-g",
        status: "requested",
        priority: "medium",
        assigned_agent_id: "agent-1",
        deferred_dod_json: null
      }
    },
    dbQuery: createSqlRouter([
      {
        match: "SELECT key, value FROM kv_meta WHERE key LIKE 'reconcile_dispatch:%'",
        result: [{ key: "reconcile_dispatch:dispatch-scope-g", value: "dispatch-scope-g" }]
      },
      {
        match: "SELECT id, kanban_card_id, to_agent_id, dispatch_type, chain_depth, status, result, context FROM task_dispatches WHERE id = ?",
        result: [
          {
            id: "dispatch-scope-g",
            kanban_card_id: "card-scope-g",
            to_agent_id: "agent-1",
            dispatch_type: "scope-assessment",
            chain_depth: 0,
            status: "completed",
            result: JSON.stringify({ scope_depth: "direct" }),
            context: "{}"
          }
        ]
      },

      {
        match: "SELECT metadata FROM kanban_cards WHERE id = ?",
        result: [{ metadata: JSON.stringify({ scope_depth: "direct", scope_assessment_status: "completed" }) }]
      },
      { match: "FROM auto_queue_entries e", result: [{ id: "entry-g", agent_id: "agent-1" }] }
    ])
  });

  policy._section_R();

  // No review advance, but the gated implementation dispatch IS created.
  assert.deepEqual(state.statusCalls, []);
  assert.equal(state.dispatchCreates.length, 1);
  assert.equal(state.dispatchCreates[0].dispatchType, "implementation");
  assert.equal(state.autoQueueStatusUpdates[0].status, "dispatched");
  assert.equal(state.autoQueueStatusUpdates[0].reason, "scope_gate_direct_reconcile");
});

test("timeouts review timeout module escalates overdue DoD waits", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    dbQuery: createSqlRouter([
      {
        match: "WHERE status = ? AND review_status = 'awaiting_dod'",
        result: [{ id: "card-dod-1" }]
      }
    ])
  });

  policy._section_D();

  assert.deepEqual(state.manualInterventions, [
    {
      cardId: "card-dod-1",
      reason: "DoD 대기 15분 초과",
      options: { review: true }
    }
  ]);
});

test("timeouts review auto-accept module creates rework dispatches before transitioning", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    dbQuery: createSqlRouter([
      {
        match: "WHERE status = ? AND review_status = 'suggestion_pending'",
        result: [
          {
            id: "card-review-1",
            assigned_agent_id: "agent-review-1",
            title: "Review card"
          }
        ]
      },
      { match: "SELECT review_round, last_verdict FROM card_review_state WHERE card_id = ?", result: [] }
    ])
  });

  policy._section_E();

  const staleSuggestionQuery = state.queries.find((query) =>
    query.sql.includes("review_status = 'suggestion_pending'")
  );
  assert.match(staleSuggestionQuery.sql, /assigned_agent_id IS NOT NULL/);

  assert.deepEqual(state.dispatchCreates, [
    {
      cardId: "card-review-1",
      agentId: "agent-review-1",
      dispatchType: "rework",
      title: "[Rework] Review card",
      context: null
    }
  ]);
  assert.deepEqual(state.statusCalls, [
    { cardId: "card-review-1", status: "in_progress", force: false }
  ]);
  assert.deepEqual(state.reviewStatusCalls, [
    {
      cardId: "card-review-1",
      reviewStatus: "rework_pending",
      options: { suggestion_pending_at: null }
    }
  ]);
});

test("timeouts review auto-accept triggers tuning aggregate once after batch inserts", () => {
  const staleCards = [
    { id: "card-review-1", assigned_agent_id: "agent-review-1", title: "Review card 1" },
    { id: "card-review-2", assigned_agent_id: "agent-review-2", title: "Review card 2" }
  ];

  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery(sql) {
      if (sql.includes("review_status = 'suggestion_pending'")) return staleCards;
      if (sql.includes("SELECT review_round, last_verdict FROM card_review_state")) {
        return [{ review_round: 2, last_verdict: "changes_requested" }];
      }
      if (sql.includes("FROM task_dispatches")) return [];
      return [];
    },
    httpPost(url, body, currentState) {
      const outcomeInserts = currentState.executions.filter((e) =>
        e.sql.includes("INSERT INTO review_tuning_outcomes")
      );
      assert.equal(outcomeInserts.length, 2);
      return { ok: true };
    }
  });

  policy._section_E();

  const outcomeInserts = state.executions.filter((execution) =>
    execution.sql.includes("INSERT INTO review_tuning_outcomes")
  );
  assert.equal(outcomeInserts.length, 2);
  assert.deepEqual(state.httpPosts, [
    { url: "http://127.0.0.1:8791/api/reviews/tuning/aggregate", body: {} }
  ]);
  assert.equal(state.kv.has("review_tuning:auto_accept_aggregate_retry"), false);
});

test("timeouts review auto-accept retries pending tuning aggregate on later tick", () => {
  const retryKey = "review_tuning:auto_accept_aggregate_retry";
  let staleQueryCount = 0;
  let postCount = 0;

  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery(sql) {
      if (sql.includes("review_status = 'suggestion_pending'")) {
        staleQueryCount += 1;
        return staleQueryCount === 1
          ? [{ id: "card-review-1", assigned_agent_id: "agent-review-1", title: "Review card 1" }]
          : [];
      }
      if (sql.includes("SELECT review_round, last_verdict FROM card_review_state")) {
        return [{ review_round: 2, last_verdict: "changes_requested" }];
      }
      if (sql.includes("FROM task_dispatches")) return [];
      return [];
    },
    httpPost() {
      postCount += 1;
      if (postCount === 1) throw new Error("temporary API failure");
      return { ok: true };
    }
  });

  policy._section_E();

  assert.equal(state.kv.get(retryKey), "pending");
  assert.equal(state.httpPosts.length, 1);

  policy._section_E();

  assert.equal(state.kv.has(retryKey), false);
  assert.equal(state.httpPosts.length, 2);
});

test("timeouts review auto-accept keeps aggregate retry when POST returns error payload", () => {
  const retryKey = "review_tuning:auto_accept_aggregate_retry";
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery: createSqlRouter([
      {
        match: "WHERE status = ? AND review_status = 'suggestion_pending'",
        result: []
      }
    ]),
    httpPost() {
      return { error: "temporary API failure" };
    }
  });
  state.kv.set(retryKey, "pending");

  policy._section_E();

  assert.equal(state.kv.get(retryKey), "pending");
  assert.equal(state.httpPosts.length, 1);
  assert.match(state.logs.warn.at(-1), /aggregate trigger returned error/);
});

test("timeouts dispatch maintenance module re-enqueues unnotified pending dispatches", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches td JOIN kanban_cards kc ON td.kanban_card_id = kc.id",
        result: [
          {
            id: "dispatch-unnotified-1",
            dispatch_type: "implementation",
            to_agent_id: "agent-1",
            title: "Needs notify",
            github_issue_url: null,
            github_issue_number: null,
            kanban_card_id: "card-1"
          }
        ]
      }
    ])
  });

  policy._section_I0();

  assert.match(state.executions[0].sql, /INSERT INTO dispatch_outbox/);
  assert.deepEqual(toPlain(state.executions[0].params), [
    "dispatch-unnotified-1",
    "agent-1",
    "card-1",
    "Needs notify"
  ]);
});

test("timeouts active monitor module checks tmux live panes exactly", () => {
  const { policy } = loadPolicy("policies/timeouts.js", {
    exec() {
      return "1\n0\n";
    }
  });

  assert.equal(policy._tmuxHasLivePane("AgentDesk-codex-project-agentdesk"), true);
});

test("timeouts active monitor deadlock section uses typed timeout facade", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    timeouts: {
      staleWorkingSessions: [],
      deadlockCandidates: []
    }
  });

  policy._section_I();

  assert.equal(state.queries.length, 0);
  assert.equal(state.executions.length, 0);
  assert.deepEqual(toPlain(state.timeoutClearFreshCounterCalls), [{ staleScanMinutes: 30 }]);
  assert.deepEqual(toPlain(state.timeoutStaleWorkingScans), [{ graceMinutes: 10 }]);
  assert.deepEqual(toPlain(state.timeoutDeadlockCandidateScans), [{ staleScanMinutes: 30, limit: 50 }]);
  assert.equal(state.timeoutInactiveCounterCleanups, 1);
  assert.equal(state.timeoutHistoryCleanupCalls.length, 1);
});

test("timeouts active monitor module treats synthetic reattach placeholders as absent", () => {
  const sessionKey = "provider:AgentDesk-codex-project-agentdesk";
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    inflights: [
      {
        provider: "codex",
        channel_id: "channel-1",
        channel_name: "project-agentdesk",
        session_key: sessionKey,
        tmux_session_name: "AgentDesk-codex-project-agentdesk",
        session_id: null,
        request_owner_user_id: 0,
        user_msg_id: 0,
        any_tool_used: false,
        has_post_tool_text: false,
        rebind_origin: true,
        started_at: timestampMinutesAgo(95),
        updated_at: timestampMinutesAgo(95)
      }
    ],
    timeouts: {
      deadlockCandidates: [
        {
          session_key: sessionKey,
          agent_id: "agent-1",
          active_dispatch_id: "dispatch-1",
          last_heartbeat: "2026-04-29 10:00:00"
        }
      ]
    },
    exec() {
      return "0\n";
    }
  });

  policy._section_I();

  assert.equal(state.deadlockAlerts.length, 0);
  assert.equal(state.httpPosts.length, 0);
  assert.deepEqual(toPlain(state.timeoutMarkSessionIdleCalls), [
    { sessionKey, options: { clear_active_dispatch_id: false } }
  ]);
});

test("timeouts active monitor opt-in review hang recovery retries stale review dispatches", () => {
  const sessionKey = "provider:AgentDesk-claude-review-session";
  const previousCheck = Date.now() - 16 * 60 * 1000;
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: {
      server_port: 8791,
      review_hang_auto_recovery_enabled: true,
      review_hang_auto_recovery_stale_min: 15,
      review_hang_auto_recovery_max_extensions: 1
    },
    inflights: [
      {
        provider: "claude",
        channel_id: "review-channel",
        channel_name: "project-agentdesk-review",
        session_key: sessionKey,
        tmux_session_name: "AgentDesk-claude-review-session",
        started_at: timestampMinutesAgo(20),
        updated_at: timestampMinutesAgo(16),
        dispatch_id: "dispatch-review-1"
      }
    ],
    timeouts: {
      deadlockCandidates: [
        {
          session_key: sessionKey,
          agent_id: "agent-review",
          active_dispatch_id: "dispatch-review-1",
          last_heartbeat: timestampMinutesAgo(16)
        }
      ],
      dispatchTypes: {
        "dispatch-review-1": "review"
      }
    },
    exec() {
      return "0\n";
    },
    httpPost() {
      return {
        ok: true,
        tmux_killed: true,
        inflight_cleared: true,
        retry_dispatch_id: "retry-review-1"
      };
    }
  });
  state.kv.set("deadlock_check:" + sessionKey, JSON.stringify({ count: 1, ts: previousCheck }));

  policy._section_I();

  assert.equal(state.httpPosts.length, 1);
  assert.equal(
    state.httpPosts[0].url,
    "http://127.0.0.1:8791/api/sessions/" + encodeURIComponent(sessionKey) + "/force-kill"
  );
  assert.equal(state.httpPosts[0].body.retry, true);
  assert.match(state.httpPosts[0].body.reason, /review hang timeout/);
  assert.equal(state.deadlockAlerts.length, 1);
  assert.match(state.deadlockAlerts[0].message, /재디스패치 완료/);
  assert.equal(state.timeoutTerminationRecords.length, 1);
  assert.equal(state.timeoutTerminationRecords[0].session_key, sessionKey);
});

test("timeouts active monitor review fast path leaves non-review sessions on the normal threshold", () => {
  const sessionKey = "provider:AgentDesk-codex-implementation-session";
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: {
      server_port: 8791,
      review_hang_auto_recovery_enabled: true,
      review_hang_auto_recovery_stale_min: 15
    },
    timeouts: {
      deadlockCandidates: [
        {
          session_key: sessionKey,
          agent_id: "agent-impl",
          active_dispatch_id: "dispatch-impl-1",
          last_heartbeat: timestampMinutesAgo(16)
        }
      ],
      dispatchTypes: {
        "dispatch-impl-1": "implementation"
      }
    },
    exec() {
      throw new Error("non-review session under 30 minutes should not probe tmux");
    }
  });

  policy._section_I();

  assert.equal(state.httpPosts.length, 0);
  assert.equal(state.deadlockAlerts.length, 0);
});

test("timeouts orphan dispatch module emits orphan recovery signals", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    dbQuery: createSqlRouter([
      { match: "SELECT value FROM kv_meta WHERE key = 'server_boot_at'", result: [] },
      {
        match: "FROM task_dispatches td JOIN kanban_cards kc ON kc.id = td.kanban_card_id",
        result: [
          {
            dispatch_id: "dispatch-orphan-1",
            kanban_card_id: "card-orphan-1",
            dispatch_type: "implementation"
          }
        ]
      }
    ]),
    emitSignal() {
      return { executed: true };
    }
  });

  policy._section_K();

  assert.deepEqual(state.runtimeSignals, [
    {
      signalName: "OrphanCandidate",
      evidence: {
        dispatch_id: "dispatch-orphan-1",
        card_id: "card-orphan-1",
        dispatch_type: "implementation",
        detected_from: "timeouts._section_K"
      }
    }
  ]);
});

test("timeouts long turn monitor module alerts every 30-minute threshold", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    inflights: [
      {
        provider: "codex",
        channel_id: "channel-1",
        channel_name: "project-agentdesk",
        session_key: "provider:AgentDesk-codex-project-agentdesk",
        tmux_session_name: "AgentDesk-codex-project-agentdesk",
        started_at: timestampMinutesAgo(91),
        dispatch_id: null
      }
    ],
    dbQuery: createSqlRouter([
      { match: "SELECT value FROM kv_meta WHERE key = ?", result: [] },
      {
        match: "SELECT id FROM agents WHERE discord_channel_id = ? OR discord_channel_alt = ? OR discord_channel_cc = ? OR discord_channel_cdx = ? LIMIT 1",
        result: []
      },
      { match: "SELECT key FROM kv_meta WHERE key LIKE 'long_turn_tier:%'", result: [] },
    ])
  });

  policy._section_L();

  assert.equal(state.deadlockAlerts.length, 1);
  assert.match(state.deadlockAlerts[0].message, /장시간 턴/);
  assert.match(state.deadlockAlerts[0].message, /90분 단계/);
  assert.match(state.executions[0].sql, /INSERT OR REPLACE INTO kv_meta/);
  assert.deepEqual(toPlain(state.executions[0].params), ["long_turn_tier:codex:channel-1", "90"]);
});

test("timeouts long turn monitor module skips persistent routine keep-alive sessions", () => {
  const tmuxSession = "AgentDesk-claude-routine-warmup-obiseo-session---personal-obi";
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    inflights: [
      {
        provider: "claude",
        channel_id: "routine-thread-1",
        channel_name: "routine warmup-obiseo-session - personal-obi",
        session_key: "provider:" + tmuxSession,
        tmux_session_name: tmuxSession,
        started_at: timestampMinutesAgo(91),
        dispatch_id: null
      }
    ],
    dbQuery: createSqlRouter([
      {
        match: "SELECT execution_strategy FROM routines WHERE discord_thread_id = ? LIMIT 1",
        result(sql, params) {
          assert.deepEqual(toPlain(params), ["routine-thread-1"]);
          return [{ execution_strategy: "persistent" }];
        }
      },
      { match: "SELECT value FROM kv_meta WHERE key = ?", result: [] },
      {
        match: "SELECT id FROM agents WHERE discord_channel_id = ? OR discord_channel_alt = ? OR discord_channel_cc = ? OR discord_channel_cdx = ? LIMIT 1",
        result: []
      },
      { match: "SELECT key FROM kv_meta WHERE key LIKE 'long_turn_tier:%'", result: [] },
      { match: "SELECT key FROM kv_meta WHERE key LIKE 'long_turn_watchdog_extension:%'", result: [] }
    ])
  });

  policy._section_L();

  assert.equal(state.deadlockAlerts.length, 0);
  assert.equal(
    state.executions.filter((execution) => /INSERT OR REPLACE INTO kv_meta/.test(execution.sql)).length,
    0
  );
  assert.equal(
    state.logs.warn.filter((line) => line.includes("inflight scan error")).length,
    0,
    state.logs.warn.join("\n")
  );
});

test("timeouts long turn monitor module still alerts fresh routine sessions", () => {
  const tmuxSession = "AgentDesk-claude-routine-once-only---personal-obi";
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    inflights: [
      {
        provider: "claude",
        channel_id: "routine-thread-2",
        channel_name: "routine once-only - personal-obi",
        session_key: "provider:" + tmuxSession,
        tmux_session_name: tmuxSession,
        started_at: timestampMinutesAgo(91),
        dispatch_id: null
      }
    ],
    dbQuery: createSqlRouter([
      {
        match: "SELECT execution_strategy FROM routines WHERE discord_thread_id = ? LIMIT 1",
        result: [{ execution_strategy: "fresh" }]
      },
      { match: "SELECT value FROM kv_meta WHERE key = ?", result: [] },
      {
        match: "SELECT id FROM agents WHERE discord_channel_id = ? OR discord_channel_alt = ? OR discord_channel_cc = ? OR discord_channel_cdx = ? LIMIT 1",
        result: []
      },
      { match: "SELECT key FROM kv_meta WHERE key LIKE 'long_turn_tier:%'", result: [] }
    ])
  });

  policy._section_L();

  assert.equal(state.deadlockAlerts.length, 1);
  assert.match(state.deadlockAlerts[0].message, /장시간 턴/);
  assert.match(state.deadlockAlerts[0].message, /90분 단계/);
});

test("timeouts long turn monitor module skips synthetic reattach placeholders", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    inflights: [
      {
        provider: "codex",
        channel_id: "channel-1",
        channel_name: "project-agentdesk",
        session_key: "provider:AgentDesk-codex-project-agentdesk",
        tmux_session_name: "AgentDesk-codex-project-agentdesk",
        session_id: null,
        request_owner_user_id: 0,
        user_msg_id: 0,
        any_tool_used: false,
        has_post_tool_text: false,
        rebind_origin: true,
        started_at: timestampMinutesAgo(95),
        updated_at: timestampMinutesAgo(95),
        dispatch_id: null
      }
    ],
    dbQuery: createSqlRouter([
      { match: "SELECT key FROM kv_meta WHERE key LIKE 'long_turn_tier:%'", result: [] },
      { match: "SELECT key FROM kv_meta WHERE key LIKE 'long_turn_watchdog_extension:%'", result: [] }
    ])
  });

  policy._section_L();

  // Synthetic placeholders never trigger alerts or tier writes…
  assert.equal(state.deadlockAlerts.length, 0);
  // …but the cleanup pass still runs and the bulk alert-key DELETE must execute.
  const bulkAlertDeletes = state.executions.filter((execution) =>
    /DELETE FROM kv_meta WHERE key LIKE 'long_turn_alert:%'/.test(execution.sql)
  );
  assert.equal(bulkAlertDeletes.length, 1);
});

test("timeouts long turn monitor module skips repeated 30-minute threshold", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    inflights: [
      {
        provider: "codex",
        channel_id: "channel-1",
        channel_name: "project-agentdesk",
        session_key: "provider:AgentDesk-codex-project-agentdesk",
        tmux_session_name: "AgentDesk-codex-project-agentdesk",
        started_at: timestampMinutesAgo(95),
        dispatch_id: null
      }
    ],
    dbQuery: createSqlRouter([
      {
        match: (sql, params) => sql.includes("SELECT value FROM kv_meta WHERE key = ?") && params[0] === "long_turn_tier:codex:channel-1",
        result: [{ value: "90" }]
      },
      { match: "SELECT value FROM kv_meta WHERE key = ?", result: [] },
      { match: "SELECT key FROM kv_meta WHERE key LIKE 'long_turn_tier:%'", result: [] },
      { match: "SELECT key FROM kv_meta WHERE key LIKE 'long_turn_alert:%'", result: [] }
    ])
  });

  policy._section_L();

  assert.equal(state.deadlockAlerts.length, 0);
});

test("timeouts long turn monitor module uses configured alert interval", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { long_turn_alert_interval_min: 40 },
    inflights: [
      {
        provider: "codex",
        channel_id: "channel-1",
        channel_name: "project-agentdesk",
        session_key: "provider:AgentDesk-codex-project-agentdesk",
        tmux_session_name: "AgentDesk-codex-project-agentdesk",
        started_at: timestampMinutesAgo(91),
        dispatch_id: null
      }
    ],
    dbQuery: createSqlRouter([
      { match: "SELECT value FROM kv_meta WHERE key = ?", result: [] },
      {
        match: "SELECT id FROM agents WHERE discord_channel_id = ? OR discord_channel_alt = ? OR discord_channel_cc = ? OR discord_channel_cdx = ? LIMIT 1",
        result: []
      },
      { match: "SELECT key FROM kv_meta WHERE key LIKE 'long_turn_tier:%'", result: [] },
      { match: "SELECT key FROM kv_meta WHERE key LIKE 'long_turn_alert:%'", result: [] }
    ])
  });

  policy._section_L();

  assert.equal(state.deadlockAlerts.length, 1);
  assert.match(state.deadlockAlerts[0].message, /80분 단계/);
  assert.deepEqual(toPlain(state.executions[0].params), ["long_turn_tier:codex:channel-1", "80"]);
});

test("timeouts workspace branch guard module recovers wt branches", () => {
  const workspace = "/tmp/agentdesk-main";
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    dbQuery: createSqlRouter([
      {
        match: "SELECT DISTINCT json_extract(metadata, '$.workspace') as ws FROM sessions",
        result: [{ ws: workspace }]
      },
      { match: "SELECT DISTINCT workspace FROM agents WHERE workspace IS NOT NULL AND workspace != ''", result: [] }
    ]),
    exec(cmd, args) {
      assert.equal(cmd, "git");
      const parsed = JSON.parse(args);
      if (parsed.includes("branch")) return "wt/feature-1\n";
      return "";
    }
  });

  policy._section_M();

  assert.deepEqual(
    state.execCalls.map((call) => JSON.parse(call.args)[2]),
    ["branch", "stash", "checkout", "pull", "worktree"]
  );
  assert.equal(state.deadlockAlerts.length, 1);
});

test("timeouts idle-kill module calls kill-tmux API for expired idle sessions", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery: createSqlRouter([
      {
        match: "SELECT id, name, name_ko, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx FROM agents",
        result: [
          {
            id: "agent-idle-1",
            name: "Idle Agent",
            name_ko: null,
            discord_channel_id: "channel-1",
            discord_channel_alt: null,
            discord_channel_cc: null,
            discord_channel_cdx: null
          }
        ]
      },
      {
        match: "FROM sessions WHERE provider IN ('claude', 'codex', 'qwen') AND (agent_id IS NULL OR TRIM(agent_id) = '')",
        result: []
      },
      {
        match(sql) {
          return sql.includes("WHERE status = 'idle'") &&
            sql.includes("active_dispatch_id IS NULL") &&
            sql.includes("INTERVAL '6 hours'");
        },
        result: [
          {
            session_key: "provider:AgentDesk-codex-project-agentdesk",
            agent_id: "agent-idle-1",
            provider: "codex",
            active_dispatch_id: null,
            thread_channel_id: null,
            last_seen_at: timestampMinutesAgo(370)
          }
        ]
      },
      {
        match(sql) {
          return sql.includes("WHERE status = 'idle'") &&
            sql.includes("active_dispatch_id IS NOT NULL") &&
            sql.includes("INTERVAL '24 hours'");
        },
        result: []
      }
    ]),
    httpPost() {
      return { ok: true, tmux_killed: true };
    }
  });

  policy._section_O();

  assert.equal(state.httpPosts.length, 1);
  assert.match(state.httpPosts[0].url, /\/api\/sessions\/provider%3AAgentDesk-codex-project-agentdesk\/kill-tmux$/);
  assert.equal(state.httpPosts[0].body.retry, undefined);
  assert.match(state.httpPosts[0].body.reason, /idle 6시간 초과/);
  assert.equal(state.httpPosts[0].body.minimum_idle_minutes, 360);
  const idleSql = state.queries.map((q) => q.sql).join("\n");
  assert.match(idleSql, /COALESCE\(s\.last_heartbeat, s\.created_at\) AS last_seen_at/);
  assert.doesNotMatch(idleSql, /turn_lifecycle_events/);
  assert.doesNotMatch(idleSql, /active_dispatch_id IS NOT NULL/);
  assert.match(state.logs.info.join("\n"), /idle kill: agent-idle-1/);
});

test("timeouts idle-kill module leaves active-dispatch sessions to dispatch cleanup", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery: createSqlRouter([
      { match: "SELECT id, name, name_ko, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx FROM agents", result: [] },
      { match: "FROM sessions WHERE provider IN ('claude', 'codex', 'qwen') AND (agent_id IS NULL OR TRIM(agent_id) = '')", result: [] },
      {
        match(sql) {
          return sql.includes("WHERE status = 'idle'") &&
            sql.includes("active_dispatch_id IS NULL") &&
            sql.includes("INTERVAL '6 hours'");
        },
        result: []
      }
    ]),
    httpPost() {
      throw new Error("active-dispatch idle rows must not call kill-tmux");
    }
  });

  policy._section_O();

  assert.equal(state.httpPosts.length, 0);
  assert.doesNotMatch(
    state.queries.map((q) => q.sql).join("\n"),
    /active_dispatch_id IS NOT NULL/
  );
});

test("timeouts idle-kill module does not let zombie (already-gone tmux) rows starve live idle sessions (#2861)", () => {
  // Main idle batch has maxKills=3. Put 3 zombie rows (tmux already gone →
  // tmux_killed:false) ahead of a genuinely-alive idle row. A no-op kill must
  // NOT consume the budget, so the live row at position 4 still gets reaped.
  const zombieKeys = [
    "provider:AgentDesk-claude-zombie-1",
    "provider:AgentDesk-claude-zombie-2",
    "provider:AgentDesk-claude-zombie-3"
  ];
  const liveKey = "provider:AgentDesk-claude-live-4";
  function row(session_key) {
    return {
      session_key,
      agent_id: null,
      provider: "claude",
      active_dispatch_id: null,
      thread_channel_id: null,
      last_seen_at: "2000-01-01 00:00:00"
    };
  }

  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery: createSqlRouter([
      {
        match: "SELECT id, name, name_ko, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx FROM agents",
        result: []
      },
      {
        match: "FROM sessions WHERE provider IN ('claude', 'codex', 'qwen') AND (agent_id IS NULL OR TRIM(agent_id) = '')",
        result: []
      },
      {
        match(sql) {
          return sql.includes("WHERE status = 'idle'") &&
            sql.includes("active_dispatch_id IS NULL") &&
            sql.includes("INTERVAL '6 hours'");
        },
        result: [...zombieKeys.map(row), row(liveKey)]
      },
      {
        match(sql) {
          return sql.includes("WHERE status = 'idle'") &&
            sql.includes("active_dispatch_id IS NOT NULL") &&
            sql.includes("INTERVAL '24 hours'");
        },
        result: []
      }
    ]),
    httpPost(url) {
      // Zombie rows report tmux_was_alive:false (handler reconciled the stale
      // row to disconnected); the live row is actually killed.
      const tmuxGone = url.includes("live-4") === false;
      return tmuxGone
        ? { ok: true, tmux_was_alive: false, tmux_killed: false, session_row_disconnected: true }
        : { ok: true, tmux_was_alive: true, tmux_killed: true };
    }
  });

  policy._section_O();

  // All 4 rows get a kill-tmux call (zombies no longer break the budget early).
  assert.equal(state.httpPosts.length, 4);
  // The live row was reached and actually killed.
  assert.ok(state.httpPosts.some((p) => p.url.includes("live-4")));
  assert.match(state.logs.info.join("\n"), /Killed idle tmux .*live-4/);
});

test("timeouts idle-kill module does not count live-activity guard skips toward budget", () => {
  const guardKeys = [
    "provider:AgentDesk-claude-guard-1",
    "provider:AgentDesk-claude-guard-2",
    "provider:AgentDesk-claude-guard-3"
  ];
  const liveKey = "provider:AgentDesk-claude-live-after-guard";
  function row(session_key) {
    return {
      session_key,
      agent_id: null,
      provider: "claude",
      active_dispatch_id: null,
      thread_channel_id: null,
      last_seen_at: "2000-01-01 00:00:00"
    };
  }

  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery: createSqlRouter([
      { match: "SELECT id, name, name_ko, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx FROM agents", result: [] },
      { match: "FROM sessions WHERE provider IN ('claude', 'codex', 'qwen') AND (agent_id IS NULL OR TRIM(agent_id) = '')", result: [] },
      { match: (sql) => sql.includes("WHERE status = 'idle'") && sql.includes("active_dispatch_id IS NULL") && sql.includes("INTERVAL '6 hours'"), result: [...guardKeys.map(row), row(liveKey)] },
      { match: (sql) => sql.includes("WHERE status = 'idle'") && sql.includes("active_dispatch_id IS NOT NULL") && sql.includes("INTERVAL '24 hours'"), result: [] }
    ]),
    httpPost(url) {
      return url.includes("live-after-guard")
        ? { ok: true, tmux_was_alive: true, tmux_killed: true }
        : { ok: true, tmux_was_alive: true, tmux_killed: false, skipped_live_activity_guard: true };
    }
  });

  policy._section_O();

  assert.equal(state.httpPosts.length, 4);
  assert.ok(state.httpPosts.some((p) => p.url.includes("live-after-guard")));
  assert.match(state.logs.info.join("\n"), /skipped live activity guard/);
  assert.doesNotMatch(state.logs.error.join("\n"), /tmux was alive but kill failed/);
});

test("timeouts idle-kill module counts genuine kill failures (tmux alive but kill failed) toward budget", () => {
  // A live session whose `tmux kill-session` fails (tmux_was_alive:true,
  // tmux_killed:false) is NOT a zombie — it must consume the budget so a stuck
  // session is not retried unbounded every tick, and must be logged as an error.
  const failKeys = [
    "provider:AgentDesk-claude-stuck-1",
    "provider:AgentDesk-claude-stuck-2",
    "provider:AgentDesk-claude-stuck-3",
    "provider:AgentDesk-claude-stuck-4"
  ];
  function row(session_key) {
    return {
      session_key, agent_id: null, provider: "claude",
      active_dispatch_id: null, thread_channel_id: null,
      last_seen_at: "2000-01-01 00:00:00"
    };
  }
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery: createSqlRouter([
      { match: "SELECT id, name, name_ko, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx FROM agents", result: [] },
      { match: "FROM sessions WHERE provider IN ('claude', 'codex', 'qwen') AND (agent_id IS NULL OR TRIM(agent_id) = '')", result: [] },
      { match: (sql) => sql.includes("WHERE status = 'idle'") && sql.includes("active_dispatch_id IS NULL") && sql.includes("INTERVAL '6 hours'"), result: failKeys.map(row) },
      { match: (sql) => sql.includes("WHERE status = 'idle'") && sql.includes("active_dispatch_id IS NOT NULL") && sql.includes("INTERVAL '24 hours'"), result: [] }
    ]),
    httpPost() {
      return { ok: true, tmux_was_alive: true, tmux_killed: false };
    }
  });

  policy._section_O();

  // maxKills=3 for the main idle batch; failures count, so only 3 attempts.
  assert.equal(state.httpPosts.length, 3);
  assert.match(state.logs.error.join("\n"), /tmux was alive but kill failed/);
});

test("timeouts idle-kill module excludes thread idle rows from the main batch", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { server_port: 8791 },
    dbQuery: createSqlRouter([
      {
        match: "SELECT id, name, name_ko, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx FROM agents",
        result: [
          {
            id: "agent-main",
            name: "Main Agent",
            name_ko: null,
            discord_channel_id: "channel-main",
            discord_channel_alt: null,
            discord_channel_cc: null,
            discord_channel_cdx: null
          }
        ]
      },
      {
        match: "FROM sessions WHERE provider IN ('claude', 'codex', 'qwen') AND (agent_id IS NULL OR TRIM(agent_id) = '')",
        result: []
      },
      {
        // Main-channel 6h batch.
        match(sql) {
          return sql.includes("INTERVAL '6 hours'") &&
            sql.includes("thread_channel_id IS NULL");
        },
        result: [
          {
            session_key: "claude/discord_x/host:AgentDesk-claude-adk-cc",
            agent_id: "agent-main",
            provider: "claude",
            active_dispatch_id: null,
            thread_channel_id: null,
            last_seen_at: "2000-01-01 00:00:00"
          }
        ]
      },
      {
        match(sql) {
          return sql.includes("active_dispatch_id IS NOT NULL") &&
            sql.includes("INTERVAL '24 hours'");
        },
        result: []
      },
    ]),
    httpPost() {
      return { ok: true, tmux_killed: true };
    }
  });

  policy._section_O();

  const idleSql = state.queries.map((q) => q.sql).join("\n");
  assert.match(idleSql, /thread_channel_id IS NULL/);
  assert.match(idleSql, /session_key !~ '-t\[0-9\]\{15,\}\(-dev\)\?\$'/);
  assert.equal(state.httpPosts.length, 1);
  const urls = state.httpPosts.map((p) => p.url).join("\n");
  assert.match(urls, /AgentDesk-claude-adk-cc\/kill-tmux$/m);
  // Reason text uses the human-readable formatter (hours, not minutes).
  state.httpPosts.forEach((p) => {
    assert.match(p.body.reason, /idle \d+(시간|일) 초과/);
    assert.equal(p.body.minimum_idle_minutes, 360);
  });
});
