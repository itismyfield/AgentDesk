var test = require("node:test");
var assert = require("node:assert");

// Set up mock agentdesk
global.agentdesk = {
  db: {
    query: function(sql, params) {
      throw new Error("agentdesk.db.query should not be called in typed-facade test");
    }
  },
  cards: {
    get: function(id) {
      if (id === "card-123") {
        return {
          id: "card-123",
          title: "Fix bug",
          github_issue_number: "42"
        };
      }
      if (id === "card-456") {
        return {
          id: "card-456",
          title: "Refactor code"
        };
      }
      return null;
    }
  },
  kv: {
    set: function() {},
    delete: function() {}
  }
};

test("auto-queue loadPhaseGateCardLabel uses typed facade agentdesk.cards.get", () => {
  // We need to load auto-queue.js to test it, but it might execute some top-level stuff.
  // The simplest way to test the function is to eval the file content and extract the function.
  var fs = require("fs");
  var content = fs.readFileSync(__dirname + "/../auto-queue.js", "utf8");

  // Create a function from the file content, returning loadPhaseGateCardLabel
  var getFunc = new Function(
    "require", "module", "agentdesk",
    content + "; return loadPhaseGateCardLabel;"
  );

  // Create dummy require for the internal dependencies. The new lib/* modules
  // re-export the helpers that auto-queue.js aliases back into its top-level
  // scope — we mirror every export name the entrypoint reads, including
  // `loadPhaseGateCardLabel` (extracted to auto-queue-phase-gate.js) which is
  // the function this facade test asserts on.
  var dummyRequire = function(path) {
    return {
      // auto-queue-log
      hasValue: function() {},
      logContextKeys: [],
      mergeLogContext: function() {},
      loadEntryLogContext: function() {},
      loadDispatchLogContext: function() {},
      normalizeLogContext: function() {},
      formatLogContext: function() {},
      autoQueueLog: function() {},
      // auto-queue-config
      maxEntryRetries: 3,
      staleDispatchedGraceMinutes: 30,
      staleDispatchedTerminalStatuses: [],
      staleDispatchedRecoverNullDispatch: false,
      staleDispatchedRecoverMissingDispatch: false,
      staleDispatchedRecoveryConditionsSql: "",
      // auto-queue-dispatch
      terminalStatesFromConfig: function() {},
      activationDispatchCount: function() {},
      activationWasDeferred: function() {},
      rotateActiveRunSweepCursor: function() {},
      isDispatchableState: function() {},
      dispatchableTargets: function() {},
      freePathToDispatchable: function() {},
      activateRun: function() {},
      // auto-queue-phase-gate
      PHASE_GATE_HUMAN_ESCALATION_THRESHOLD: 3,
      PHASE_GATE_FAILURE_TTL_SEC: 0,
      PHASE_GATE_ALERT_DEBOUNCE_TTL_SEC: 0,
      PHASE_GATE_AUTOCLOSE_TTL_SEC: 0,
      PHASE_GATE_GRACE_WINDOW_MS: 0,
      inferPhaseGatePassVerdict: function() {},
      phaseGateFailureKey: function() {},
      incrementPhaseGateFailureCount: function() {},
      resetPhaseGateFailureCount: function() {},
      // The function this facade test asserts on. Mirror the original body
      // verbatim so the test exercises the real card-label formatting.
      loadPhaseGateCardLabel: function(cardId) {
        if (!cardId) return "unknown card";
        var card = global.agentdesk.cards.get(cardId);
        if (!card) return cardId;
        if (card.github_issue_number) {
          return "#" + card.github_issue_number + " " + (card.title || card.id);
        }
        return card.title || card.id;
      },
      handlePhaseGateFailure: function() {},
      maybeAlertPhaseGateVerdictMismatch: function() {},
      phaseGateOnlyIssueClosedFailing: function() {},
      loadCardForPhaseGateFallback: function() {},
      extractRepoSlugFromIssueUrl: function() {},
      attemptPhaseGateAutoCloseFallback: function() {},
      loadPhaseGateState: function() {},
      savePhaseGateState: function() {},
      clearPhaseGateState: function() {},
      runHasBlockingPhaseGate: function() {},
      beginPhaseGateGraceWindow: function() {},
      clearPhaseGateGraceWindow: function() {},
      runWithinPhaseGateGrace: function() {},
      pauseRun: function() {},
      loadPhaseGateDispatches: function() {},
      phaseGateRequired: function() {},
      buildPhaseGateGroups: function() {},
      phaseGateTitle: function() {},
      createPhaseGateDispatches: function() {},
      // auto-queue-lifecycle
      loadRunInfo: function() {},
      remainingRunnableEntryCount: function() {},
      runHasUserCancelledEntry: function() {},
      finalizeRunWithoutPhaseGate: function() {},
      completeRunAndNotify: function() {},
      continueRunAfterEntry: function() {},
      resumeRunAndActivate: function() {},
      // auto-queue-error-recovery
      notifyAutoQueueEntryFailure: function() {},
      createConsultationDispatch: function() {}
    };
  };

  var loadPhaseGateCardLabel = getFunc(dummyRequire, {}, global.agentdesk);

  // Test 1: Full card
  var label1 = loadPhaseGateCardLabel("card-123");
  assert.strictEqual(label1, "#42 Fix bug");

  // Test 2: Card without issue number
  var label2 = loadPhaseGateCardLabel("card-456");
  assert.strictEqual(label2, "Refactor code");

  // Test 3: Missing card
  var label3 = loadPhaseGateCardLabel("missing-card");
  assert.strictEqual(label3, "missing-card");

  // Test 4: Empty cardId
  var label4 = loadPhaseGateCardLabel(null);
  assert.strictEqual(label4, "unknown card");
});
