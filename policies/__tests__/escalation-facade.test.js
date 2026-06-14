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

test("00-escalation enqueueEscalation uses typed kv facade instead of raw db", () => {
  var fs = require("fs");
  var content = fs.readFileSync(__dirname + "/../00-escalation.js", "utf8");

  var getFunc = new Function(
    "require", "module", "agentdesk",
    content + "; return { enqueueEscalation };"
  );

  var setCalls = [];
  var getCalls = [];
  var dbCalls = [];

  var mockAgentdesk = {
    // #3335: 00-escalation.js now registers a hook-less helper policy
    registerPolicy: function() {},
    cards: { get: function(id) { return { id: id, title: "Test Card" }; } },
    db: {
      query: function(sql, params) { dbCalls.push({ sql: sql, params: params }); return []; },
      execute: function(sql, params) { dbCalls.push({ sql: sql, params: params }); }
    },
    kv: {
      get: function(key) { getCalls.push(key); return null; },
      set: function(key, value, ttl) { setCalls.push({ key, value, ttl }); },
      delete: function(key) {}
    }
  };

  var funcs = getFunc(require, {}, mockAgentdesk);
  funcs.enqueueEscalation("card-123", "test_reason");

  assert.strictEqual(dbCalls.length, 0, "Should not call agentdesk.db");
  assert.strictEqual(getCalls.length, 1);
  assert.strictEqual(getCalls[0], "pm_pending:card-123");
  assert.strictEqual(setCalls.length, 1);
  assert.strictEqual(setCalls[0].key, "pm_pending:card-123");
  assert.strictEqual(setCalls[0].ttl, 600); // ESCALATION_PENDING_TTL_SEC is usually 600
  var parsed = JSON.parse(setCalls[0].value);
  assert.deepStrictEqual(parsed.reasons, ["test_reason"]);
});

test("00-escalation flushEscalations uses typed kv facade for writes/deletes", () => {
  var fs = require("fs");
  var content = fs.readFileSync(__dirname + "/../00-escalation.js", "utf8");

  var getFunc = new Function(
    "require", "module", "agentdesk",
    content + "; return { flushEscalations };"
  );

  var setCalls = [];
  var getCalls = [];
  var delCalls = [];
  var dbCalls = [];

  var mockAgentdesk = {
    // #3335: 00-escalation.js now registers a hook-less helper policy
    registerPolicy: function() {},
    cards: { get: function(id) { return { id: id, title: "Test Card", status: "review", review_status: "dilemma_pending" }; } },
    pipeline: {
      getConfig: function() { return {}; },
      isTerminal: function() { return false; }
    },
    config: {
      get: function(key) { if (key === "server_port") return "8080"; return null; }
    },
    http: {
      post: function(url, body) { return { status: 200 }; }
    },
    log: { info: function() {}, warn: function() {} },
    db: {
      query: function(sql, params) {
        if (sql.includes("pm_pending:%")) {
          return [
            { key: "pm_pending:card-123", value: JSON.stringify({ reasons: ["test"] }) },
            { key: "pm_pending:invalid", value: "invalid-json" }
          ];
        }
        dbCalls.push({ sql: sql, params: params });
        return [];
      },
      execute: function(sql, params) { dbCalls.push({ sql: sql, params: params }); }
    },
    kv: {
      get: function(key) { getCalls.push(key); return null; },
      set: function(key, value, ttl) { setCalls.push({ key, value, ttl }); },
      delete: function(key) { delCalls.push(key); }
    }
  };

  var funcs = getFunc(require, {}, mockAgentdesk);
  funcs.flushEscalations();

  assert.strictEqual(dbCalls.length, 0, "Should not call agentdesk.db except for prefix scan");
  assert.ok(getCalls.includes("pm_decision_sent:card-123"));
  assert.ok(delCalls.includes("pm_pending:invalid"));
  assert.ok(delCalls.includes("pm_pending:card-123"));
  assert.strictEqual(setCalls.length, 1);
  assert.strictEqual(setCalls[0].key, "pm_decision_sent:card-123");
});
