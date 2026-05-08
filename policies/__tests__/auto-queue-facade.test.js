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

  // Create dummy require for the internal dependencies
  var dummyRequire = function(path) {
    return {
      hasValue: function() {},
      logContextKeys: [],
      mergeLogContext: function() {},
      loadEntryLogContext: function() {},
      loadDispatchLogContext: function() {},
      normalizeLogContext: function() {},
      formatLogContext: function() {},
      autoQueueLog: function() {},
      maxEntryRetries: 3,
      staleDispatchedGraceMinutes: 30,
      staleDispatchedTerminalStatuses: [],
      staleDispatchedRecoverNullDispatch: false,
      staleDispatchedRecoverMissingDispatch: false,
      staleDispatchedRecoveryConditionsSql: ""
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
