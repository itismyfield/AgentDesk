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
