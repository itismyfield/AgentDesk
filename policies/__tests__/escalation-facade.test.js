var test = require("node:test");
var assert = require("node:assert");

global.agentdesk = {
  cards: { get: function(id) { return null; } },
  pipeline: { getConfig: function() { return {}; }, isTerminal: function() { return false; } }
};

test("00-escalation loadCardMetadata handles pre-parsed object metadata", () => {
  var fs = require("fs");
  var content = fs.readFileSync(__dirname + "/../00-escalation.js", "utf8");

  var getFunc = new Function(
    "require", "module", "agentdesk",
    content + "; return { loadCardMetadata, loadLoopGuardJson };"
  );

  var mockAgentdesk = {
    cards: {
      get: function(id) {
        if (id === "parsed") {
          return { id: "parsed", metadata: { key: "value" } };
        }
        if (id === "string") {
          return { id: "string", metadata: '{"key":"value"}' };
        }
        if (id === "invalid") {
          return { id: "invalid", metadata: '{key:value}' };
        }
        if (id === "empty") {
          return { id: "empty" };
        }
        return null;
      }
    }
  };

  var funcs = getFunc(require, {}, mockAgentdesk);

  // Test parsed object
  assert.deepStrictEqual(funcs.loadCardMetadata("parsed"), { key: "value" });

  // Test JSON string
  assert.deepStrictEqual(funcs.loadCardMetadata("string"), { key: "value" });

  // Test invalid JSON string
  assert.deepStrictEqual(funcs.loadCardMetadata("invalid"), {});

  // Test empty card
  assert.deepStrictEqual(funcs.loadCardMetadata("empty"), {});

  // Test null
  assert.deepStrictEqual(funcs.loadCardMetadata("missing"), {});
});

test("00-escalation loadManualInterventionState handles missing cards and parses states", () => {
  var fs = require("fs");
  var content = fs.readFileSync(__dirname + "/../00-escalation.js", "utf8");

  var getFunc = new Function(
    "require", "module", "agentdesk",
    content + "; return { loadManualInterventionState };"
  );

  var mockAgentdesk = {
    cards: {
      get: function(id) {
        if (id === "valid") {
          return { id: "valid", status: "review", review_status: "dilemma_pending", blocked_reason: null };
        }
        if (id === "terminal") {
          return { id: "terminal", status: "terminal", review_status: null, blocked_reason: null };
        }
        return null;
      }
    },
    pipeline: {
      getConfig: function() { return {}; },
      isTerminal: function(status, cfg) { return status === "terminal"; }
    }
  };

  var funcs = getFunc(require, {}, mockAgentdesk);

  // Test valid card
  assert.deepStrictEqual(funcs.loadManualInterventionState("valid"), {
    status: "review",
    review_status: "dilemma_pending",
    blocked_reason: null,
    fingerprint: "review:dilemma_pending",
    active: true
  });

  // Test terminal card
  assert.deepStrictEqual(funcs.loadManualInterventionState("terminal"), {
    status: "terminal",
    review_status: null,
    blocked_reason: null,
    fingerprint: null,
    active: false
  });

  // Test missing card
  assert.strictEqual(funcs.loadManualInterventionState("missing"), null);
});

test("00-escalation escalationCardTitle uses github issue number", () => {
  var fs = require("fs");
  var content = fs.readFileSync(__dirname + "/../00-escalation.js", "utf8");

  var getFunc = new Function(
    "require", "module", "agentdesk",
    content + "; return { escalationCardTitle };"
  );

  var mockAgentdesk = {
    cards: {
      get: function(id) {
        if (id === "with_issue") {
          return { id: "with_issue", github_issue_number: "42" };
        }
        if (id === "without_issue") {
          return { id: "without_issue" };
        }
        return null;
      }
    }
  };

  var funcs = getFunc(require, {}, mockAgentdesk);

  // Test with issue
  assert.strictEqual(funcs.escalationCardTitle("with_issue"), "#42 (with_issue)");

  // Test without issue
  assert.strictEqual(funcs.escalationCardTitle("without_issue"), "without_issue");

  // Test missing card
  assert.strictEqual(funcs.escalationCardTitle("missing"), "missing");
});
