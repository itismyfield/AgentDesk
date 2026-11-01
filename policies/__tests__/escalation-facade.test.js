var test = require("node:test");
var assert = require("node:assert");

global.agentdesk = {
  // #3335: 00-escalation.js now registers a hook-less helper policy
  registerPolicy: function() {},
  cards: { get: function(id) { return null; } },
  pipeline: { getConfig: function() { return {}; }, isTerminal: function() { return false; } },
  kv: {
    get: function(k) { return null; },
    set: function(k, v, ttl) {},
    delete: function(k) {}
  },
  db: {
    query: function(q, p) { return []; },
    execute: function(q, p) {}
  }
};

test("00-escalation loadCardMetadata handles pre-parsed object metadata", () => {
  var fs = require("fs");
  var content = fs.readFileSync(__dirname + "/../00-escalation.js", "utf8");

  var getFunc = new Function(
    "require", "module", "agentdesk",
    content + "; return { loadCardMetadata, loadLoopGuardJson };"
  );

  var mockAgentdesk = {
    // #3335: 00-escalation.js now registers a hook-less helper policy
    registerPolicy: function() {},
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
    // #3335: 00-escalation.js now registers a hook-less helper policy
    registerPolicy: function() {},
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
    // #3335: 00-escalation.js now registers a hook-less helper policy
    registerPolicy: function() {},
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

test("00-escalation enqueueEscalation delegates to its owner without generic kv/db", () => {
  const content = require("fs").readFileSync(__dirname + "/../00-escalation.js", "utf8");
  const calls = [];
  function forbidden() { throw new Error("escalation must use its Rust owner"); }
  const ad = {
    registerPolicy() {},
    cards: { get() { return { github_issue_number: 42 }; } },
    kv: { get: forbidden, set: forbidden, delete: forbidden },
    db: { query: forbidden, execute: forbidden },
    escalation: { enqueue(...args) { calls.push(args); } }
  };
  const enqueue = new Function("agentdesk", content + "; return enqueueEscalation;")(ad);
  enqueue("card-123", "test_reason");
  enqueue("", "test_reason");
  enqueue("card-123", "");
  assert.deepStrictEqual(calls, [["card-123", "#42 (card-123)", "test_reason"]]);
});

test("00-escalation flushEscalations delegates lifecycle and only stamps successful emits", () => {
  const content = require("fs").readFileSync(__dirname + "/../00-escalation.js", "utf8");
  const prepared = [], sent = [], emitted = [];
  function forbidden() { throw new Error("escalation must use its Rust owner"); }
  const ad = {
    registerPolicy() {},
    cards: { get(id) { return id === "inactive" ? null : { status: "review", review_status: "dilemma_pending" }; } },
    pipeline: { getConfig() { return {}; }, isTerminal() { return false; } },
    config: { get() { return "8080"; } },
    kv: { get: forbidden, set: forbidden, delete: forbidden },
    db: { query: forbidden, execute: forbidden },
    log: { warn() {} },
    http: { post(url, body) { emitted.push(body); return body.card_id === "fail" ? {error: "offline"} : {status: 200}; } },
    escalation: {
      pending() { return ["ready", "cooldown", "inactive", "fail"].map(card_id => ({card_id, value: "opaque"})); },
      prepare(row, fingerprint) {
        prepared.push([row.card_id, fingerprint]);
        return row.card_id === "ready" || row.card_id === "fail" ? {reasons: ["reason"]} : null;
      },
      sent(...args) { sent.push(args); }
    }
  };
  const flush = new Function("agentdesk", content + "; return flushEscalations;")(ad);
  flush();
  assert.deepStrictEqual(prepared, [["ready", "review:dilemma_pending"], ["cooldown", "review:dilemma_pending"], ["inactive", null], ["fail", "review:dilemma_pending"]]);
  assert.deepStrictEqual(emitted, [{card_id: "ready", reasons: ["reason"]}, {card_id: "fail", reasons: ["reason"]}]);
  assert.deepStrictEqual(sent, [["ready", "review:dilemma_pending"]]);
});
