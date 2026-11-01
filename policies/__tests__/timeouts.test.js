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

test("S7T1 helper recognizes external-input TUI-direct synthetic owners", () => {
  const { module } = loadPolicy("policies/timeouts.js");
  for (const owner of [1, "1"]) {
    assert.equal(module.helpers.isExternalInputTuiDirectSyntheticTurn({
      turn_source: "external_input", request_owner_user_id: owner
    }), true);
  }
});

test("S7T2 helper rejects rebind, ordinary, other-owner and missing inflights", () => {
  const { module } = loadPolicy("policies/timeouts.js");
  const synthetic = { turn_source: "external_input", request_owner_user_id: 1 };
  for (const inf of [null, { ...synthetic, rebind_origin: true },
    { ...synthetic, turn_source: "managed" }, { ...synthetic, request_owner_user_id: 2 }]) {
    assert.equal(module.helpers.isExternalInputTuiDirectSyntheticTurn(inf), false);
  }
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

  const progress = module.helpers.findRecentInflightForSession(
    "provider:AgentDesk-codex-project-agentdesk",
    "AgentDesk-codex-project-agentdesk",
  );

  assert.equal(progress, null);
});

test("timeouts card timeout module marks requested dispatches failed before retry", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { requested_timeout_min: 30 },
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

test("timeouts dispatch maintenance retries failed dispatches with an incremented retry count", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
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
});

// Pins the requested-timeout retry budget: retry_count 9 still retries, 10 escalates.
test("timeouts requested sweep retries below the dispatch budget and escalates at it", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { requested_timeout_min: 30 },
    dbQuery: createSqlRouter([
      {
        match: "FROM kanban_cards kc LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id",
        result: [
          {
            id: "card-last-retry",
            assigned_agent_id: "agent-1",
            latest_dispatch_id: "dispatch-last-retry",
            retry_count: 9,
            dispatch_type: "implementation"
          },
          {
            id: "card-exhausted",
            assigned_agent_id: "agent-1",
            latest_dispatch_id: "dispatch-exhausted",
            retry_count: 10,
            dispatch_type: "implementation"
          }
        ]
      }
    ])
  });

  policy._section_A();

  assert.deepEqual(state.dispatchMarkFailedCalls, [
    { dispatchId: "dispatch-last-retry", reason: "Timed out waiting for agent" },
    { dispatchId: "dispatch-exhausted", reason: "Timed out waiting for agent" }
  ]);
  const requestedResets = state.executions.filter((e) => /UPDATE kanban_cards SET requested_at/.test(e.sql));
  assert.deepEqual(requestedResets.map((e) => toPlain(e.params)), [["card-last-retry"]]);
  assert.deepEqual(state.manualInterventions, [
    {
      cardId: "card-exhausted",
      reason: "Timed out waiting for agent (10 retries exhausted)",
      options: null
    }
  ]);
});

test("timeouts requested sweep leaves already-terminal dispatches alone", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    config: { requested_timeout_min: 30 },
    markFailed() {
      return { rows_affected: 0 };
    },
    dbQuery: createSqlRouter([
      {
        match: "FROM kanban_cards kc LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id",
        result: [
          {
            id: "card-terminal",
            assigned_agent_id: "agent-1",
            latest_dispatch_id: "dispatch-terminal",
            retry_count: 10,
            dispatch_type: "implementation"
          }
        ]
      }
    ])
  });

  policy._section_A();

  assert.equal(state.dispatchMarkFailedCalls.length, 1);
  assert.deepEqual(state.executions, []);
  assert.deepEqual(state.manualInterventions, []);
});

// Pins the failed-dispatch auto-retry budget (SQL filter) and the last-retry increment.
test("timeouts failed-dispatch retry keeps the budget filter and retries the last attempt", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches td JOIN kanban_cards kc ON kc.id = td.kanban_card_id",
        result: [
          {
            id: "dispatch-failed-9",
            kanban_card_id: "card-retry-9",
            to_agent_id: "agent-1",
            dispatch_type: null,
            title: "Last retry",
            retry_count: 9,
            github_issue_url: null,
            github_issue_number: null
          }
        ]
      }
    ])
  });

  policy._section_J();

  const retryQuery = state.queries.find((q) => /WHERE td.status = 'failed'/.test(q.sql));
  assert.ok(retryQuery);
  assert.match(retryQuery.sql, /AND COALESCE\(td\.retry_count, 0\) < 10 /);
  assert.deepEqual(toPlain(retryQuery.params), ["requested", "in_progress"]);
  assert.deepEqual(state.dispatchCreates, [
    {
      cardId: "card-retry-9",
      agentId: "agent-1",
      dispatchType: "implementation",
      title: "Last retry",
      context: null
    }
  ]);
  assert.deepEqual(state.dispatchRetryCountCalls, [
    { dispatchId: "dispatch-1", count: 10 }
  ]);
  assert.deepEqual(state.manualInterventions, []);
});

test("timeouts failed-dispatch retry tolerates a dispatch create failure", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    dispatchCreate() {
      throw new Error("create refused");
    },
    dbQuery: createSqlRouter([
      {
        match: "FROM task_dispatches td JOIN kanban_cards kc ON kc.id = td.kanban_card_id",
        result: [
          {
            id: "dispatch-failed-2",
            kanban_card_id: "card-retry-2",
            to_agent_id: "agent-1",
            dispatch_type: "implementation",
            title: "Retry refused",
            retry_count: 2,
            github_issue_url: null,
            github_issue_number: null
          }
        ]
      }
    ])
  });

  assert.doesNotThrow(() => policy._section_J());

  assert.deepEqual(state.dispatchRetryCountCalls, []);
  assert.deepEqual(state.executions, []);
  assert.deepEqual(state.manualInterventions, []);
  assert.equal(state.logs.error.length, 1);
  assert.match(state.logs.error[0], /Failed to create retry dispatch for card card-retry-2/);
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

test("timeouts orphan dispatch delegates from the real scheduler to its Rust owner", () => {
  let queries = 0;
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    dbQuery() { queries++; return []; }
  });
  for (const section of ["I0", "J", "I"]) policy["_section_" + section] = () => {};
  policy.onTick30s({});
  assert.equal(state.orphanMaintenanceCalls, 1);
  assert.equal(queries, 0);
  assert.deepEqual(state.runtimeSignals, []);
});

test("long turns do not register monitoring or page the deadlock manager", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    inflights: [{
      provider: "codex",
      channel_id: "long-running-channel",
      started_at: timestampMinutesAgo(24 * 60)
    }]
  });
  assert.equal(policy._section_L, undefined);
  // Isolate the minute scheduler: an old inflight must not add any hidden
  // watchdog extension, alert, or elapsed-time intervention.
  for (const section of ["A", "C", "D", "E", "N"]) policy["_section_" + section] = () => {};
  policy.onTick1min({});
  assert.equal(state.deadlockAlerts.length, 0);
  assert.equal(state.executions.length, 0);
});

test("workspace branch guard stays out of the 5min tick (startup guard owns it)", () => {
  const { policy, state } = loadPolicy("policies/timeouts.js", {
    exec(cmd) {
      assert.fail("5min tick must not shell out to " + cmd);
    }
  });
  assert.equal(policy._section_M, undefined);
  for (const section of ["B", "G", "O"]) policy["_section_" + section] = () => {};
  policy.onTick5min({});
  assert.equal(state.execCalls.length, 0);
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

test("timeouts idle-kill module does not count provider busy, unknown or host-refused skips toward budget", () => {
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
      if (url.includes("guard-2")) return { ok: false, refused: true, reason: "herdr_unsupported" };
      return url.includes("live-after-guard")
        ? { ok: true, tmux_was_alive: true, tmux_killed: true }
        : { ok: true, tmux_was_alive: true, tmux_killed: false, skipped_provider_activity_guard: true };
    }
  });

  policy._section_O();

  assert.equal(state.httpPosts.length, 4);
  assert.ok(state.httpPosts.some((p) => p.url.includes("live-after-guard")));
  assert.match(state.logs.info.join("\n"), /provider idle state not proven/);
  assert.match(state.logs.warn.join("\n"), /refused by host guard for provider:AgentDesk-claude-guard-2 \(herdr_unsupported/);
  assert.doesNotMatch(state.logs.error.join("\n"), /tmux was alive but kill failed|kill-tmux API failed/);
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


// Host, inflight and recovery decisions are exercised against the Rust owner and real PG.
test("timeouts active monitor only invokes the Rust owner facade", () => {
  const { agentdesk, policy, state } = loadPolicy("policies/timeouts.js", {
    timeouts: {
      maintainStaleSessions() { return { ok: true }; }
    }
  });
  policy._section_I();
  assert.deepEqual(state.timeoutMaintenanceCalls, [[]]);
  assert.equal(policy._sessionHost, undefined);
  assert.equal(state.queries.length, 0);
  assert.equal(state.executions.length, 0);
  assert.equal(state.timeoutHostObservations.length, 0);
  assert.deepEqual(Object.keys(agentdesk.timeouts).sort(), [
    "maintainStaleSessions", "observeSessionHost"
  ]);
});

test("timeouts active monitor leaves owner failure to the policy tick boundary", () => {
  const { policy } = loadPolicy("policies/timeouts.js", {
    timeouts: { maintainStaleSessions() { throw new Error("owner unavailable"); } }
  });
  assert.throws(() => policy._section_I(), /owner unavailable/);
});
