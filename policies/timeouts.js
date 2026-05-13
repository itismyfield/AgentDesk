/**
 * timeouts.js — ADK Policy: Timeout & Stale Detection
 * priority: 100
 *
 * Root policy entrypoint. Domain logic lives in policies/timeouts/*.js and
 * is attached here so the JS engine still loads policies/timeouts.js.
 */

var helpers = require("./lib/timeouts-helpers");
var _flushPMDecisions = helpers._flushPMDecisions;

var timeouts = {
  name: "timeouts",
  priority: 100
};

require("./timeouts/reconciliation")(timeouts, helpers);
require("./timeouts/card-timeouts")(timeouts, helpers);
require("./timeouts/review-timeouts")(timeouts, helpers);
require("./timeouts/review-auto-accept")(timeouts, helpers);
require("./timeouts/dispatch-maintenance")(timeouts, helpers);
require("./timeouts/active-monitor")(timeouts, helpers);
require("./timeouts/orphan-dispatch")(timeouts, helpers);
require("./timeouts/long-turn-monitor")(timeouts, helpers);
require("./timeouts/workspace-branch-guard")(timeouts, helpers);
require("./timeouts/idle-kill")(timeouts, helpers);
require("./timeouts/idle-recap")(timeouts, helpers);

// ── Tiered tick handlers (#127) ──────────────────────────────────
// Sections are grouped by criticality and cadence.
// onTick (legacy, 5min) is kept as no-op for backward compat.

// 30s tier: [J] retry, [I-0] unsent notification recovery, [I] deadlock, [K] orphan
// Critical-path sections [I] and [K] run here to avoid non-critical section delays (#127)
timeouts.onTick30s = function(ev) {
  var start = Date.now();
  var t;
  t = Date.now(); try { timeouts._section_I0(); } catch(e) { agentdesk.log.warn("[tick30s] I-0 error: " + e); }
  agentdesk.log.debug("[tick30s][I-0] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_J(); } catch(e) { agentdesk.log.warn("[tick30s] J error: " + e); }
  agentdesk.log.debug("[tick30s][J] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_I(); } catch(e) { agentdesk.log.warn("[tick30s] I error: " + e); }
  agentdesk.log.debug("[tick30s][I] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_K(); } catch(e) { agentdesk.log.warn("[tick30s] K error: " + e); }
  agentdesk.log.debug("[tick30s][K] " + (Date.now() - t) + "ms");
  agentdesk.log.debug("[tick30s] total " + (Date.now() - start) + "ms");
};

// 1min tier: [A] [C] [D] [E] [L] (non-critical timeouts)
// [K] moved to 30s tier for critical-path isolation (#127)
timeouts.onTick1min = function(ev) {
  var start = Date.now();
  var t;
  t = Date.now(); try { timeouts._section_A(); } catch(e) { agentdesk.log.warn("[tick1min] A error: " + e); }
  agentdesk.log.debug("[tick1min][A] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_C(); } catch(e) { agentdesk.log.warn("[tick1min] C error: " + e); }
  agentdesk.log.debug("[tick1min][C] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_D(); } catch(e) { agentdesk.log.warn("[tick1min] D error: " + e); }
  agentdesk.log.debug("[tick1min][D] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_E(); } catch(e) { agentdesk.log.warn("[tick1min] E error: " + e); }
  agentdesk.log.debug("[tick1min][E] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_L(); } catch(e) { agentdesk.log.warn("[tick1min] L error: " + e); }
  agentdesk.log.debug("[tick1min][L] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_N(); } catch(e) { agentdesk.log.warn("[tick1min] N error: " + e); }
  agentdesk.log.debug("[tick1min][N] " + (Date.now() - t) + "ms");
  agentdesk.log.debug("[tick1min] total " + (Date.now() - start) + "ms");
};

// 5min tier: [R] [B] [F] [G] [H] [M] [O] + TTL cleanup (non-critical reconciliation)
// [I] moved to 30s tier for critical-path isolation (#127)
timeouts.onTick5min = function(ev) {
  var start = Date.now();
  var t;
  // #126: Purge expired kv_meta keys
  t = Date.now();
  try {
    agentdesk.db.execute("DELETE FROM kv_meta WHERE expires_at IS NOT NULL AND expires_at < datetime('now')");
  } catch(e) { agentdesk.log.warn("[tick5min] kv_ttl error: " + e); }
  agentdesk.log.debug("[tick5min][kv_ttl] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_R(); } catch(e) { agentdesk.log.warn("[tick5min] R error: " + e); }
  agentdesk.log.debug("[tick5min][R] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_B(); } catch(e) { agentdesk.log.warn("[tick5min] B error: " + e); }
  agentdesk.log.debug("[tick5min][B] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_F(); } catch(e) { agentdesk.log.warn("[tick5min] F error: " + e); }
  agentdesk.log.debug("[tick5min][F] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_G(); } catch(e) { agentdesk.log.warn("[tick5min] G error: " + e); }
  agentdesk.log.debug("[tick5min][G] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_H(); } catch(e) { agentdesk.log.warn("[tick5min] H error: " + e); }
  agentdesk.log.debug("[tick5min][H] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_M(); } catch(e) { agentdesk.log.warn("[tick5min] M error: " + e); }
  agentdesk.log.debug("[tick5min][M] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_O(); } catch(e) { agentdesk.log.warn("[tick5min] O error: " + e); }
  agentdesk.log.debug("[tick5min][O] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_R_idle_recap(); } catch(e) { agentdesk.log.warn("[tick5min] R_idle_recap error: " + e); }
  agentdesk.log.debug("[tick5min][R_idle_recap] " + (Date.now() - t) + "ms");
  agentdesk.log.debug("[tick5min] total " + (Date.now() - start) + "ms");
};

// Legacy onTick: flush PM decision buffer after all tiered handlers (#231)
timeouts.onTick = function() {
  flushEscalations();
};

if (typeof agentdesk !== "undefined" && agentdesk && typeof agentdesk.registerPolicy === "function") {
  agentdesk.registerPolicy(timeouts);
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = {
    policy: timeouts,
    helpers: helpers
  };
}
