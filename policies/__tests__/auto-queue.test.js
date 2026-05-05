const test = require("node:test");
const assert = require("node:assert/strict");

const { createSqlRouter, defaultPipelineConfig, loadPolicy, toPlain } = require("./support/harness");

test("auto-queue infers phase_gate_passed when every declared check passes", () => {
  const { module } = loadPolicy("policies/auto-queue.js");

  const verdict = module.__test.inferPhaseGatePassVerdict(
    {
      phase_gate: {
        pass_verdict: "phase_gate_passed",
        checks: ["lint", "tests"]
      }
    },
    {
      checks: {
        lint: { status: "pass" },
        tests: { result: "passed" }
      }
    }
  );

  assert.equal(verdict, "phase_gate_passed");
});

test("auto-queue does not infer a phase gate verdict when the result already carries an explicit verdict", () => {
  const { module } = loadPolicy("policies/auto-queue.js");

  const verdict = module.__test.inferPhaseGatePassVerdict(
    {
      phase_gate: {
        pass_verdict: "phase_gate_passed",
        checks: ["lint"]
      }
    },
    {
      verdict: "manual_override",
      checks: {
        lint: { status: "pass" }
      }
    }
  );

  assert.equal(verdict, null);
});

test("auto-queue dispatchable targets prioritize requested and keep unique dispatch anchors", () => {
  const pipelineConfig = defaultPipelineConfig();
  const { module } = loadPolicy("policies/auto-queue.js");

  const targets = module.__test.dispatchableTargets(pipelineConfig);

  assert.deepEqual(toPlain(targets), ["requested"]);
});

test("auto-queue finds a free path from backlog to the nearest dispatchable state", () => {
  const { module } = loadPolicy("policies/auto-queue.js");

  const path = module.__test.freePathToDispatchable("backlog", defaultPipelineConfig());

  assert.deepEqual(toPlain(path), ["requested"]);
});

test("auto-queue onTick1min honors stale dispatched runtime config", () => {
  const recordedFailures = [];
  const { policy } = loadPolicy("policies/auto-queue.js", {
    config: {
      maxEntryRetries: 7,
      staleDispatchedGraceMin: 5,
      staleDispatchedTerminalStatuses: "failed,expired",
      staleDispatchedRecoverNullDispatch: false,
      staleDispatchedRecoverMissingDispatch: true
    },
    recordDispatchFailure(entryId, retryLimit, source) {
      recordedFailures.push({ entryId, retryLimit, source });
      return { retryCount: 2, retryLimit, to: "pending", changed: true };
    },
    dbQuery: createSqlRouter([
      {
        match: "FROM auto_queue_entries e JOIN auto_queue_runs r ON e.run_id = r.id JOIN kanban_cards kc ON kc.id = e.kanban_card_id",
        result: []
      },
      {
        match: "FROM auto_queue_runs r WHERE r.status IN ('active', 'paused')",
        result: []
      },
      {
        match: "FROM auto_queue_runs r JOIN auto_queue_entries e ON e.run_id = r.id",
        result: []
      },
      {
        match(sql) {
          return sql.includes("FROM auto_queue_entries e") &&
            sql.includes("e.status = 'dispatched'") &&
            sql.includes("td.status IN ('failed', 'expired')");
        },
        result(sql) {
          assert.match(sql, /datetime\('now', '-5 minutes'\)/);
          assert.doesNotMatch(sql, /e\.dispatch_id IS NULL/);
          assert.match(
            sql,
            /\(e\.dispatch_id IS NOT NULL AND NOT EXISTS \(SELECT 1 FROM task_dispatches td WHERE td\.id = e\.dispatch_id\)\)/
          );
          return [{
            id: "entry-stale-1",
            agent_id: "agent-1",
            dispatch_id: "dispatch-stale-1",
            kanban_card_id: "card-stale-1"
          }];
        }
      },
      {
        match: "SELECT run_id, id as entry_id, kanban_card_id as card_id, dispatch_id, agent_id,",
        result: [{
          run_id: "run-stale-1",
          entry_id: "entry-stale-1",
          card_id: "card-stale-1",
          dispatch_id: "dispatch-stale-1",
          agent_id: "agent-1",
          thread_group: 0,
          batch_phase: 0,
          slot_index: null
        }]
      },
      {
        match: "SELECT COALESCE(e.run_id, json_extract(COALESCE(td.context, '{}'), '$.run_id')",
        result: [{
          run_id: "run-stale-1",
          entry_id: "entry-stale-1",
          card_id: "card-stale-1",
          dispatch_id: "dispatch-stale-1",
          thread_group: 0,
          batch_phase: 0,
          slot_index: null,
          agent_id: "agent-1"
        }]
      }
    ])
  });

  policy.onTick1min();

  assert.deepEqual(recordedFailures, [
    { entryId: "entry-stale-1", retryLimit: 7, source: "tick_recovery" }
  ]);
});

test("auto-queue terminal cleanup uses pipeline terminal states", () => {
  const { policy, state } = loadPolicy("policies/auto-queue.js", {
    pipelineConfig: {
      states: [
        { id: "backlog" },
        { id: "requested" },
        { id: "shipped", terminal: true }
      ],
      transitions: [
        { from: "backlog", to: "requested", type: "free" },
        { from: "requested", to: "shipped", type: "gated" }
      ]
    },
    dbQuery: createSqlRouter([
      {
        match: "JOIN kanban_cards kc ON kc.id = e.kanban_card_id",
        result: [{ id: "entry-terminal", kanban_card_id: "card-terminal", status: "shipped", run_id: "run-terminal" }]
      },
      {
        match: "SELECT run_id, id as entry_id, kanban_card_id as card_id",
        result: [{
          run_id: "run-terminal",
          entry_id: "entry-terminal",
          card_id: "card-terminal",
          dispatch_id: null,
          agent_id: "agent-terminal",
          thread_group: 0,
          batch_phase: 0,
          slot_index: null
        }]
      },
      {
        match: "SELECT r.id FROM auto_queue_runs r",
        result: []
      },
      {
        match: "SELECT DISTINCT r.id",
        result: []
      },
      {
        match: "e.status = 'dispatched'",
        result: []
      }
    ])
  });

  policy.onTick1min();

  assert.deepEqual(Array.from(state.queries[0].params), ["shipped"]);
  assert.deepEqual(state.autoQueueStatusUpdates, [
    {
      entryId: "entry-terminal",
      status: "skipped",
      reason: "tick_terminal_cleanup",
      extra: null
    }
  ]);
});

test("auto-queue rotates saturated active runs in bounded tick sweep", () => {
  const { policy, state } = loadPolicy("policies/auto-queue.js", {
    dbQuery: createSqlRouter([
      {
        match: "JOIN kanban_cards kc ON kc.id = e.kanban_card_id",
        result: []
      },
      {
        match: "SELECT r.id FROM auto_queue_runs r",
        result: []
      },
      {
        match: "SELECT DISTINCT r.id",
        result: [{ id: "run-saturated" }]
      },
      {
        match: "e.status = 'dispatched'",
        result: []
      }
    ]),
    autoQueueActivate: () => ({ count: 0 })
  });

  policy.onTick1min();

  assert.deepEqual(state.autoQueueActivations, [{ runId: "run-saturated", threadGroup: null }]);
  assert.equal(state.executions.length, 1);
  assert.equal(
    state.executions[0].sql,
    "UPDATE auto_queue_runs SET updated_at = datetime('now') WHERE id = ? AND status = 'active'"
  );
  assert.deepEqual(Array.from(state.executions[0].params), ["run-saturated"]);
});

test("auto-queue marks pending entries skipped when a card progresses externally into a dispatchable state", () => {
  const { policy, state } = loadPolicy("policies/auto-queue.js", {
    dbQuery: createSqlRouter([
      {
        match: "SELECT e.id FROM auto_queue_entries e",
        result: [{ id: "entry-10" }, { id: "entry-11" }]
      },
      {
        match: "SELECT run_id, id as entry_id, kanban_card_id as card_id, dispatch_id, agent_id,",
        result: (_sql, params) => [
          {
            run_id: "run-1",
            entry_id: params[0],
            card_id: "card-10",
            dispatch_id: null,
            agent_id: "agent-10",
            thread_group: 0,
            batch_phase: 0,
            slot_index: 0
          }
        ]
      }
    ])
  });

  policy.onCardTransition({
    card_id: "card-10",
    source: "manual_transition",
    to: "requested"
  });

  assert.deepEqual(state.autoQueueStatusUpdates, [
    {
      entryId: "entry-10",
      status: "skipped",
      reason: "external_progress",
      extra: null
    },
    {
      entryId: "entry-11",
      status: "skipped",
      reason: "external_progress",
      extra: null
    }
  ]);
});
