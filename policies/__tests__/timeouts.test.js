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
      assert.equal(currentState.executions.length, 2);
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
    dbQuery: createSqlRouter([
      {
        match: "AND last_heartbeat >= datetime('now', '-30 minutes')",
        result: []
      },
      {
        match: "SELECT session_key FROM sessions WHERE status IN ('turn_active', 'working') AND last_heartbeat < datetime('now', '-10 minutes')",
        result: []
      },
      {
        match: "SELECT session_key, agent_id, active_dispatch_id, last_heartbeat FROM sessions WHERE status IN ('turn_active', 'working')",
        result: [
          {
            session_key: sessionKey,
            agent_id: "agent-1",
            active_dispatch_id: "dispatch-1",
            last_heartbeat: "2026-04-29 10:00:00"
          }
        ]
      },
      {
        match: "SELECT key FROM kv_meta WHERE key LIKE 'deadlock_check:%'",
        result: []
      },
      {
        match: "SELECT key FROM kv_meta WHERE key LIKE 'deadlock_history:%'",
        result: []
      }
    ]),
    exec() {
      return "0\n";
    }
  });

  policy._section_I();

  assert.equal(state.deadlockAlerts.length, 0);
  assert.equal(state.httpPosts.length, 0);
  assert.match(state.executions[0].sql, /SET status = 'idle'/);
  assert.deepEqual(toPlain(state.executions[0].params), [sessionKey]);
  assert.deepEqual(toPlain(state.executions[1].params), ["deadlock_check:" + sessionKey]);
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
      { match: "SELECT key FROM kv_meta WHERE key LIKE 'long_turn_alert:%'", result: [] }
    ])
  });

  policy._section_L();

  assert.equal(state.deadlockAlerts.length, 1);
  assert.match(state.deadlockAlerts[0].message, /장시간 턴/);
  assert.match(state.deadlockAlerts[0].message, /90분 단계/);
  assert.match(state.executions[0].sql, /INSERT OR REPLACE INTO kv_meta/);
  assert.deepEqual(toPlain(state.executions[0].params), ["long_turn_tier:codex:channel-1", "90"]);
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
      { match: "SELECT key FROM kv_meta WHERE key LIKE 'long_turn_alert:%'", result: [] },
      { match: "SELECT key FROM kv_meta WHERE key LIKE 'long_turn_watchdog_extension:%'", result: [] }
    ])
  });

  policy._section_L();

  assert.equal(state.deadlockAlerts.length, 0);
  assert.equal(state.executions.length, 0);
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

test("timeouts idle-kill module calls force-kill API for expired idle sessions", () => {
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
            sql.includes("-60 minutes");
        },
        result: [
          {
            session_key: "provider:AgentDesk-codex-project-agentdesk",
            agent_id: "agent-idle-1",
            provider: "codex",
            active_dispatch_id: null,
            thread_channel_id: null,
            last_seen_at: "2000-01-01 00:00:00"
          }
        ]
      },
      {
        match(sql) {
          return sql.includes("WHERE status = 'idle'") &&
            sql.includes("active_dispatch_id IS NOT NULL") &&
            sql.includes("-180 minutes");
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
  assert.match(state.httpPosts[0].url, /\/api\/sessions\/provider%3AAgentDesk-codex-project-agentdesk\/force-kill$/);
  assert.equal(state.httpPosts[0].body.retry, false);
  assert.match(state.logs.info.join("\n"), /idle kill: agent-idle-1/);
});
