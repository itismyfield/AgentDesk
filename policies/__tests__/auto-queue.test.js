const test = require("node:test");
const assert = require("node:assert/strict");

const { createSqlRouter, defaultPipelineConfig, loadPolicy, toPlain } = require("./support/harness");

function prConfirmDeclaration() {
  return {
    kind: "pr-confirm",
    declaration_version: 1,
    pass_verdict: "phase_gate_passed",
    evidence_requirement: "dispatch_result_checks",
    required_checks: [
      { check: "merge_verified", authority: "dispatch_result" },
      { check: "issue_closed", authority: "dispatch_result" },
      { check: "build_passed", authority: "dispatch_result" }
    ],
    available: true,
    unavailable_reason: null
  };
}

function deployGateDeclaration() {
  return {
    kind: "deploy-gate",
    declaration_version: 1,
    pass_verdict: "phase_gate_passed",
    evidence_requirement: "trusted_deployment_evidence",
    required_checks: [
      { check: "build_passed", authority: "dispatch_result" },
      { check: "deploy_verified", authority: "trusted_deployment_evidence" }
    ],
    available: false,
    unavailable_reason: "deploy-gate unavailable: trusted deployment evidence capability is not configured"
  };
}

function phaseGateDeclaration(kind) {
  if (!kind || kind === "pr-confirm") return prConfirmDeclaration();
  if (kind === "deploy-gate") return deployGateDeclaration();
  return null;
}

function passingPrChecks() {
  return {
    merge_verified: { status: "pass" },
    issue_closed: { result: "passed" },
    build_passed: "pass"
  };
}

test("auto-queue infers phase_gate_passed when every registry check passes", () => {
  const { module } = loadPolicy("policies/auto-queue.js", { phaseGateDeclaration });
  const verdict = module.__test.inferPhaseGatePassVerdict(
    { phase_gate: prConfirmDeclaration() },
    { checks: passingPrChecks() }
  );
  assert.equal(verdict, "phase_gate_passed");
});

test("auto-queue does not infer a phase gate verdict when result carries explicit failure", () => {
  const { module } = loadPolicy("policies/auto-queue.js", { phaseGateDeclaration });
  const verdict = module.__test.inferPhaseGatePassVerdict(
    { phase_gate: prConfirmDeclaration() },
    { verdict: "manual_override", checks: passingPrChecks() }
  );
  assert.equal(verdict, null);
});

test("auto-queue treats phase_gate_verdict as explicit phase gate verdict", () => {
  const { module } = loadPolicy("policies/auto-queue.js", { phaseGateDeclaration });
  const verdict = module.__test.inferPhaseGatePassVerdict(
    { phase_gate: prConfirmDeclaration() },
    { phase_gate_verdict: "manual_hold", checks: passingPrChecks() }
  );
  assert.equal(verdict, null);
});

test("auto-queue accepts pass alias only when registry checks pass", () => {
  const { module } = loadPolicy("policies/auto-queue.js", { phaseGateDeclaration });
  const matches = module.__test.phaseGateVerdictMatches(
    "pass",
    "phase_gate_passed",
    { phase_gate: prConfirmDeclaration() },
    { verdict: "pass", checks: passingPrChecks() }
  );
  assert.equal(matches, true);
});

test("auto-queue explicit phase_gate_passed cannot bypass registry checks", () => {
  const { module } = loadPolicy("policies/auto-queue.js", { phaseGateDeclaration });
  assert.equal(module.__test.phaseGateVerdictMatches(
    "phase_gate_passed",
    "phase_gate_passed",
    { phase_gate: prConfirmDeclaration() },
    { verdict: "phase_gate_passed", checks: {} }
  ), false);
});

test("auto-queue agent-supplied deploy_verified cannot satisfy authoritative evidence", () => {
  const { module } = loadPolicy("policies/auto-queue.js", { phaseGateDeclaration });
  assert.equal(module.__test.phaseGateVerdictMatches(
    "phase_gate_passed",
    "phase_gate_passed",
    { phase_gate: deployGateDeclaration() },
    { verdict: "phase_gate_passed", checks: { build_passed: "pass", deploy_verified: "pass" } }
  ), false);
});

test("auto-queue malformed and stale declaration snapshots fail closed", () => {
  const { module } = loadPolicy("policies/auto-queue.js", { phaseGateDeclaration });
  const stale = prConfirmDeclaration();
  stale.declaration_version = 99;
  assert.equal(module.__test.inferPhaseGatePassVerdict(
    { phase_gate: stale },
    { checks: passingPrChecks() }
  ), null);
});

test("auto-queue production completion persists only closed non-string verdict labels", () => {
  const context = {
    phase_gate: Object.assign(prConfirmDeclaration(), {
      run_id: "run-private",
      batch_phase: 0,
      card_ids: ["card-private"]
    })
  };
  const rawResult = {
    verdict: { authorization: "Bearer secret" },
    checks: { build_passed: "fail" }
  };
  const { policy, state } = loadPolicy("policies/auto-queue.js", {
    phaseGateDeclaration,
    dbQuery: createSqlRouter([
      {
        match: "SELECT id, kanban_card_id, dispatch_type, result, context FROM task_dispatches",
        result: [{
          id: "dsp-private",
          kanban_card_id: "card-private",
          dispatch_type: "phase-gate",
          context: JSON.stringify(context),
          result: JSON.stringify(rawResult)
        }]
      },
      {
        match: "FROM auto_queue_phase_gates",
        result: [{
          dispatch_id: "dsp-private",
          status: "pending",
          verdict: null,
          pass_verdict: "phase_gate_passed",
          next_phase: 1,
          final_phase: false,
          anchor_card_id: "card-private",
          failure_reason: null,
          created_at: "2026-07-25T00:00:00Z"
        }]
      },
      { match: "FROM kanban_cards WHERE id = ?", result: [] },
      { match: "FROM task_dispatches td LEFT JOIN auto_queue_entries", result: [] }
    ]),
    globals: { notifyCardOwner() {} }
  });

  policy.onDispatchCompleted({ dispatch_id: "dsp-private" });

  assert.equal(state.autoQueueSavedPhaseGates.length, 1);
  const saved = state.autoQueueSavedPhaseGates[0].state;
  assert.equal(saved.verdict, "<non-string:object>");
  assert.equal(saved.failure_reason, "expected phase_gate_passed, got <non-string:object>");
  assert.doesNotMatch(JSON.stringify(saved), /authorization|Bearer secret/);
});

test("auto-queue completes pre-snapshot legacy gate only from persisted NULL/blank provenance", () => {
  for (const provenance of ["NULL", "blank"]) {
    const legacyContext = {
      phase_gate: {
        run_id: `run-legacy-${provenance}`,
        batch_phase: 0,
        next_phase: 1,
        final_phase: false,
        pass_verdict: "attacker_override",
        checks: ["attacker_override"]
      }
    };
    const result = {
      verdict: "phase_gate_passed",
      checks: passingPrChecks()
    };
    const dispatch = {
      id: `dsp-legacy-${provenance}`,
      kanban_card_id: `card-legacy-${provenance}`,
      dispatch_type: "phase-gate",
      context: JSON.stringify(legacyContext),
      result: JSON.stringify(result),
      status: "completed"
    };
    const { policy, state } = loadPolicy("policies/auto-queue.js", {
      phaseGateDeclaration,
      dbQuery: createSqlRouter([
        {
          match: "SELECT id, kanban_card_id, dispatch_type, result, context FROM task_dispatches",
          result: [dispatch]
        },
        {
          match: "FROM auto_queue_phase_gates",
          result: [{
            dispatch_id: dispatch.id,
            status: "pending",
            verdict: null,
            pass_verdict: "attacker_override",
            next_phase: 1,
            final_phase: false,
            anchor_card_id: dispatch.kanban_card_id,
            failure_reason: null,
            created_at: "2026-07-25T00:00:00Z"
          }]
        },
        {
          match: "legacy_default_count",
          result: [{ entry_count: 1, legacy_default_count: 1 }]
        },
        {
          match: "SELECT id, status, result, context FROM task_dispatches WHERE id IN",
          result: [dispatch]
        },
        { match: "FROM kanban_cards WHERE id = ?", result: [] },
        { match: "FROM task_dispatches td LEFT JOIN auto_queue_entries", result: [] }
      ]),
      globals: { notifyCardOwner() {} }
    });

    policy.onDispatchCompleted({ dispatch_id: dispatch.id });

    assert.equal(state.autoQueueSavedPhaseGates.length, 0);
    assert.equal(state.autoQueueClearedPhaseGates.length, 1);
    assert.equal(state.autoQueueResumes.length, 1);
  }
});

test("auto-queue legacy context remains fail-closed for nonblank persisted kind", () => {
  const context = {
    phase_gate: {
      run_id: "run-explicit-deploy",
      batch_phase: 0,
      pass_verdict: "phase_gate_passed"
    }
  };
  const result = {
    verdict: "phase_gate_passed",
    checks: passingPrChecks()
  };
  const dispatch = {
    id: "dsp-explicit-deploy",
    kanban_card_id: "card-explicit-deploy",
    dispatch_type: "phase-gate",
    context: JSON.stringify(context),
    result: JSON.stringify(result),
    status: "completed"
  };
  const { policy, state } = loadPolicy("policies/auto-queue.js", {
    phaseGateDeclaration,
    dbQuery: createSqlRouter([
      {
        match: "SELECT id, kanban_card_id, dispatch_type, result, context FROM task_dispatches",
        result: [dispatch]
      },
      {
        match: "FROM auto_queue_phase_gates",
        result: [{
          dispatch_id: dispatch.id,
          status: "pending",
          pass_verdict: "phase_gate_passed",
          next_phase: 1,
          final_phase: false,
          anchor_card_id: dispatch.kanban_card_id,
          created_at: "2026-07-25T00:00:00Z"
        }]
      },
      {
        match: "legacy_default_count",
        result: [{ entry_count: 1, legacy_default_count: 0 }]
      },
      { match: "FROM kanban_cards WHERE id = ?", result: [] },
      { match: "FROM task_dispatches td LEFT JOIN auto_queue_entries", result: [] }
    ]),
    globals: { notifyCardOwner() {} }
  });

  policy.onDispatchCompleted({ dispatch_id: dispatch.id });

  assert.equal(state.autoQueueSavedPhaseGates.length, 1);
  assert.equal(state.autoQueueSavedPhaseGates[0].state.status, "failed");
  assert.equal(state.autoQueueClearedPhaseGates.length, 0);
});

test("auto-queue legacy context remains fail-closed for mixed NULL and nonblank persisted kinds", () => {
  const context = {
    phase_gate: {
      run_id: "run-mixed-provenance",
      batch_phase: 0
    }
  };
  const result = {
    verdict: "phase_gate_passed",
    checks: passingPrChecks()
  };
  const dispatch = {
    id: "dsp-mixed-provenance",
    kanban_card_id: "card-mixed-provenance",
    dispatch_type: "phase-gate",
    context: JSON.stringify(context),
    result: JSON.stringify(result),
    status: "completed"
  };
  let provenanceSql = "";
  const { policy, state } = loadPolicy("policies/auto-queue.js", {
    phaseGateDeclaration,
    dbQuery(sql) {
      if (sql.includes("SELECT id, kanban_card_id, dispatch_type, result, context FROM task_dispatches")) {
        return [dispatch];
      }
      if (sql.includes("FROM auto_queue_phase_gates")) {
        return [{
          dispatch_id: dispatch.id,
          status: "pending",
          pass_verdict: "phase_gate_passed",
          next_phase: 1,
          final_phase: false,
          anchor_card_id: dispatch.kanban_card_id,
          created_at: "2026-07-25T00:00:00Z"
        }];
      }
      if (sql.includes("legacy_default_count")) {
        provenanceSql = sql;
        return [{ entry_count: 2, legacy_default_count: 1 }];
      }
      return [];
    },
    globals: { notifyCardOwner() {} }
  });

  policy.onDispatchCompleted({ dispatch_id: dispatch.id });

  assert.equal(state.autoQueueSavedPhaseGates.length, 1);
  assert.equal(state.autoQueueSavedPhaseGates[0].state.status, "failed");
  assert.equal(state.autoQueueClearedPhaseGates.length, 0);
  assert.match(provenanceSql, /BTRIM\(phase_gate_kind, E' \\t\\n\\r\\f\\v'\)/);
});

test("auto-queue group keys split distinct kind and declaration snapshots", () => {
  const { module } = loadPolicy("policies/auto-queue.js", { phaseGateDeclaration });
  const pr = prConfirmDeclaration();
  const stalePr = prConfirmDeclaration();
  stalePr.declaration_version = 2;
  const deploy = deployGateDeclaration();
  const key = (declaration) => module.__test.phaseGateGroupKey(
    "agent-a",
    "agent-a",
    "phase-gate",
    declaration
  );
  assert.notEqual(key(pr), key(stalePr));
  assert.notEqual(key(pr), key(deploy));
});

test("auto-queue groups persisted legacy defaults and explicit kinds by canonical declaration", () => {
  const rows = [
    {
      entry_id: "entry-legacy",
      kanban_card_id: "card-legacy",
      agent_id: "agent-a",
      status: "done",
      priority_rank: 0,
      phase_gate_kind: null,
      title: "Legacy",
      github_issue_number: 1,
      repo_id: "repo",
      latest_result: "{}"
    },
    {
      entry_id: "entry-pr",
      kanban_card_id: "card-pr",
      agent_id: "agent-a",
      status: "done",
      priority_rank: 1,
      phase_gate_kind: "pr-confirm",
      title: "PR",
      github_issue_number: 2,
      repo_id: "repo",
      latest_result: "{}"
    }
  ];
  const { module, state } = loadPolicy("policies/auto-queue.js", {
    phaseGateDeclaration,
    dbQuery: createSqlRouter([{ match: "e.phase_gate_kind", result: rows }])
  });
  const groups = module.__test.buildPhaseGateGroups("run-1", 0);
  assert.equal(groups.length, 1);
  assert.equal(groups[0].declaration.kind, "pr-confirm");
  assert.deepEqual(toPlain(groups[0].card_ids), ["card-legacy", "card-pr"]);
  assert.ok(state.queries[0].sql.includes("e.phase_gate_kind"));
});

test("auto-queue pipeline checks cannot override canonical declaration", () => {
  const pipelineConfig = defaultPipelineConfig();
  pipelineConfig.phase_gate = {
    checks: ["attacker_override"],
    pass_verdict: "attacker_pass"
  };
  const { module } = loadPolicy("policies/auto-queue.js", {
    pipelineConfig,
    phaseGateDeclaration,
    dbQuery: createSqlRouter([{
      match: "e.phase_gate_kind",
      result: [{
        entry_id: "entry-pr",
        kanban_card_id: "card-pr",
        agent_id: "agent-a",
        status: "done",
        priority_rank: 0,
        phase_gate_kind: "pr-confirm",
        title: "PR",
        github_issue_number: 2,
        repo_id: "repo",
        latest_result: "{}"
      }]
    }])
  });
  const groups = module.__test.buildPhaseGateGroups("run-1", 0);
  assert.deepEqual(toPlain(groups[0].declaration.required_checks), prConfirmDeclaration().required_checks);
  assert.equal(groups[0].declaration.pass_verdict, "phase_gate_passed");
});

test("auto-queue unknown persisted phase-gate kind fails closed", () => {
  const { module } = loadPolicy("policies/auto-queue.js", {
    phaseGateDeclaration,
    dbQuery: createSqlRouter([{
      match: "e.phase_gate_kind",
      result: [{
        entry_id: "entry-unknown", kanban_card_id: "card-unknown", agent_id: "agent-a",
        status: "done", priority_rank: 0, phase_gate_kind: "ship-it", title: "Unknown",
        github_issue_number: 3, repo_id: "repo", latest_result: "{}"
      }]
    }])
  });
  const groups = module.__test.buildPhaseGateGroups("run-1", 0);
  assert.equal(groups.length, 0);
  assert.match(groups.error, /requires reconciliation/);
});

test("auto-queue unavailable deploy gate creates no runnable dispatch", () => {
  const row = {
    entry_id: "entry-deploy", kanban_card_id: "card-deploy", agent_id: "agent-a",
    status: "done", priority_rank: 0, phase_gate_kind: "deploy-gate", title: "Deploy",
    github_issue_number: 4, repo_id: "repo", latest_result: "{}"
  };
  const { module, state } = loadPolicy("policies/auto-queue.js", {
    phaseGateDeclaration,
    dbQuery: createSqlRouter([
      { match: "FROM auto_queue_phase_gates", result: [] },
      { match: "e.phase_gate_kind", result: [row] }
    ]),
    globals: { notifyCardOwner() {} }
  });
  const result = module.__test.createPhaseGateDispatches("run-deploy", 0, 1, false, "card-deploy");
  assert.equal(result.status, "failed");
  assert.equal(result.failed_reason, deployGateDeclaration().unavailable_reason);
  assert.equal(state.dispatchCreates.length, 0);
  assert.equal(state.autoQueueSavedPhaseGates.length, 1);
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

test("auto-queue stale terminal statuses accept normalized strings and arrays", () => {
  const stringConfig = loadPolicy("policies/lib/auto-queue-config.js", {
    config: {
      staleDispatchedTerminalStatuses: " failed,expired,FAILED,bad-status "
    }
  }).module;
  const arrayConfig = loadPolicy("policies/lib/auto-queue-config.js", {
    config: {
      staleDispatchedTerminalStatuses: ["FAILED", "expired", "failed", "bad-status"]
    }
  }).module;

  assert.deepEqual(toPlain(stringConfig.staleDispatchedTerminalStatuses()), ["failed", "expired"]);
  assert.deepEqual(toPlain(arrayConfig.staleDispatchedTerminalStatuses()), ["failed", "expired"]);
});

test("auto-queue stale recovery switches accept only explicit booleans", () => {
  const validValues = [
    [true, true],
    ["true", true],
    [false, false],
    ["false", false]
  ];
  const readers = [
    ["staleDispatchedRecoverNullDispatch", "staleDispatchedRecoverNullDispatch"],
    ["staleDispatchedRecoverMissingDispatch", "staleDispatchedRecoverMissingDispatch"]
  ];

  for (const [key, reader] of readers) {
    for (const [configured, expected] of validValues) {
      const module = loadPolicy("policies/lib/auto-queue-config.js", {
        config: { [key]: configured }
      }).module;
      assert.equal(module[reader](), expected, `${key}=${JSON.stringify(configured)}`);
    }
  }
});

test("auto-queue stale recovery switches fail safely on malformed values", () => {
  const malformedValues = [null, undefined, [], {}, 0, 1, "TRUE", "FALSE", "yes", ""];
  const readers = [
    ["staleDispatchedRecoverNullDispatch", "staleDispatchedRecoverNullDispatch"],
    ["staleDispatchedRecoverMissingDispatch", "staleDispatchedRecoverMissingDispatch"]
  ];

  for (const [key, reader] of readers) {
    for (const configured of malformedValues) {
      const module = loadPolicy("policies/lib/auto-queue-config.js", {
        config: { [key]: configured }
      }).module;
      assert.equal(module[reader](), true, `${key}=${JSON.stringify(configured)}`);
    }
  }
});

test("auto-queue onTick1min honors stale dispatched runtime config", () => {
  const recordedFailures = [];
  const { policy } = loadPolicy("policies/auto-queue.js", {
    config: {
      maxEntryRetries: 7,
      staleDispatchedGraceMin: 5,
      staleDispatchedTerminalStatuses: ["failed", "expired", "FAILED", "bad-status"],
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
        match(sql) {
          return sql.includes("SELECT r.id FROM auto_queue_runs r") &&
            sql.includes("JOIN LATERAL (") &&
            sql.includes("WHERE e.run_id = r.id AND e.status = 'pending'") &&
            sql.includes("ORDER BY e.updated_at ASC LIMIT 1") &&
            sql.includes("ORDER BY oldest_pending.updated_at ASC LIMIT 50");
        },
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
        match(sql) {
          return sql.includes("SELECT r.id FROM auto_queue_runs r") &&
            sql.includes("user_cancelled");
        },
        result: []
      },
      {
        match(sql) {
          return sql.includes("SELECT r.id FROM auto_queue_runs r") &&
            sql.includes("JOIN LATERAL (") &&
            sql.includes("WHERE e.run_id = r.id AND e.status = 'pending'") &&
            sql.includes("ORDER BY e.updated_at ASC LIMIT 1") &&
            sql.includes("ORDER BY oldest_pending.updated_at ASC LIMIT 50");
        },
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

test("auto-queue finalization sweep filters blocked runs before LIMIT", () => {
  const { policy, state } = loadPolicy("policies/auto-queue.js", {
    dbQuery: createSqlRouter([
      {
        match: "JOIN kanban_cards kc ON kc.id = e.kanban_card_id",
        result: []
      },
      {
        match(sql) {
          return sql.includes("SELECT r.id FROM auto_queue_runs r") &&
            sql.includes("auto_queue_phase_gates") &&
            sql.includes("phase_gate_grace_until") &&
            sql.includes("ORDER BY r.id ASC LIMIT 50");
        },
        result: [{ id: "run-eligible" }]
      },
      {
        match: "SELECT COUNT(*) as cnt FROM auto_queue_phase_gates",
        result: [{ cnt: 0 }]
      },
      {
        match: "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND status IN ('pending', 'dispatched')",
        result: [{ cnt: 0 }]
      },
      {
        match: "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND status = 'user_cancelled'",
        result: [{ cnt: 0 }]
      },
      {
        match: "SELECT phase_gate_grace_until FROM auto_queue_runs WHERE id = ?",
        result: [{ phase_gate_grace_until: null }]
      },
      {
        match(sql) {
          return sql.includes("SELECT r.id FROM auto_queue_runs r") &&
            sql.includes("JOIN LATERAL (") &&
            sql.includes("WHERE e.run_id = r.id AND e.status = 'pending'") &&
            sql.includes("ORDER BY e.updated_at ASC LIMIT 1") &&
            sql.includes("ORDER BY oldest_pending.updated_at ASC LIMIT 50");
        },
        result: []
      },
      {
        match: "e.status = 'dispatched'",
        result: []
      }
    ])
  });

  policy.onTick1min();

  const finishedRunQuery = state.queries.find((query) =>
    query.sql.includes("SELECT r.id FROM auto_queue_runs r") &&
    query.sql.includes("auto_queue_phase_gates")
  );
  assert.match(finishedRunQuery.sql, /NOT EXISTS \(  SELECT 1 FROM auto_queue_phase_gates pg/);
  assert.match(finishedRunQuery.sql, /datetime\(r\.phase_gate_grace_until\) <= datetime\('now'\)/);
  assert.deepEqual(state.autoQueueCompletes, [
    { runId: "run-eligible", reason: "finalize_without_phase_gate", options: { releaseSlots: true } }
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
        match(sql) {
          return sql.includes("SELECT r.id FROM auto_queue_runs r") &&
            sql.includes("user_cancelled");
        },
        result: []
      },
      {
        match(sql) {
          return sql.includes("SELECT r.id FROM auto_queue_runs r") &&
            sql.includes("JOIN LATERAL (") &&
            sql.includes("WHERE e.run_id = r.id AND e.status = 'pending'") &&
            sql.includes("ORDER BY e.updated_at ASC LIMIT 1") &&
            sql.includes("ORDER BY oldest_pending.updated_at ASC LIMIT 50");
        },
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
    "UPDATE auto_queue_entries SET updated_at = datetime('now') WHERE run_id = ? AND status = 'pending'"
  );
  assert.deepEqual(Array.from(state.executions[0].params), ["run-saturated"]);
});

test("auto-queue does not rotate deferred active run activations", () => {
  const { policy, state } = loadPolicy("policies/auto-queue.js", {
    dbQuery: createSqlRouter([
      {
        match: "JOIN kanban_cards kc ON kc.id = e.kanban_card_id",
        result: []
      },
      {
        match(sql) {
          return sql.includes("SELECT r.id FROM auto_queue_runs r") &&
            sql.includes("user_cancelled");
        },
        result: []
      },
      {
        match(sql) {
          return sql.includes("SELECT r.id FROM auto_queue_runs r") &&
            sql.includes("JOIN LATERAL (") &&
            sql.includes("WHERE e.run_id = r.id AND e.status = 'pending'") &&
            sql.includes("ORDER BY e.updated_at ASC LIMIT 1") &&
            sql.includes("ORDER BY oldest_pending.updated_at ASC LIMIT 50");
        },
        result: [{ id: "run-deferred" }]
      },
      {
        match: "e.status = 'dispatched'",
        result: []
      }
    ]),
    autoQueueActivate: () => ({ ok: true, deferred: true, count: 0, dispatched: [] })
  });

  policy.onTick1min();

  assert.deepEqual(state.autoQueueActivations, [{ runId: "run-deferred", threadGroup: null }]);
  assert.equal(state.executions.length, 0);
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
