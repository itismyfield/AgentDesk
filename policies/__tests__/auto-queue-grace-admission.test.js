const test = require("node:test");
const assert = require("node:assert/strict");
const { loadPolicy } = require("./support/harness");

const GRACE_WRITE = /SET phase_gate_grace_until = (?!NULL)/;

// Finish phase 0 of run-a with nothing left, so the continuation would open its phase gate.
function continueAfterLastEntry(graceWrite) {
  const { module, agentdesk, state } = loadPolicy("policies/lib/auto-queue-lifecycle.js", {
    dbExecute(sql, params) {
      return GRACE_WRITE.test(sql) ? graceWrite(params) : { changes: 1 };
    },
    globals: { notifyCardOwner() {} }
  });
  agentdesk.db.query = () => [];
  module.continueRunAfterEntry("run-a", "agent-a", 0, 0, null);
  return state;
}

function phaseGateWork(state) {
  return state.autoQueueSavedPhaseGates.length + state.dispatchCreates.length;
}

test("continuation opens the phase gate after the grace write matches its run", () => {
  assert.ok(phaseGateWork(continueAfterLastEntry(() => ({ changes: 1 }))) > 0);
});

test("continuation stops before the phase gate when the grace write matches no run", () => {
  const state = continueAfterLastEntry(() => ({ changes: 0 }));
  assert.equal(phaseGateWork(state), 0);
  assert.equal(state.autoQueueActivations.length, 0);
  assert.equal(state.autoQueueCompletes.length, 0);
  assert.match(state.logs.warn.join("\n"), /run-a/);
});

test("continuation keeps the phase gate when the grace write fails transiently", () => {
  const state = continueAfterLastEntry(() => {
    throw new Error("transient grace failure");
  });
  assert.ok(phaseGateWork(state) > 0);
});
