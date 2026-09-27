/**
 * phase_gate.js — Phase Gate check registry (#1082).
 *
 * Previously, phase_gate checks (merge_verified / issue_closed / build_passed)
 * were hard-coded by the Rust runtime. This registry pattern lets JS policies
 * (including repo- and agent-level overrides) register additional named gates
 * without rebuilding the binary.
 *
 * YAML still declares the *names* of the checks to run in
 * `phase_gate.checks` (see policies/default-pipeline.yaml). The JS side
 * supplies the implementation for each name.
 *
 * Usage:
 *   agentdesk.phaseGate.register("qa_passed", function(ctx) {
 *     // ctx: { card_id, repo_id, agent_id, dispatch_id }
 *     // Return: true/false, or { passed: bool, reason?: string }
 *     var rows = queryGateState(
 *       "task_dispatches", ctx.dispatch_id, "succeeded");
 *     return rows.length > 0;
 *   });
 *
 * Evaluation (driven from auto-queue / phase-gate dispatch handling):
 *   var result = agentdesk.phaseGate.evaluate(name, ctx);
 *
 * If a gate name appears in phase_gate.checks but is NOT registered, the
 * evaluator returns `{ passed: false, reason: "phase_gate_unregistered:<name>" }`
 * so the card is held in review rather than silently passing.
 */

(function installPhaseGateRegistry() {
  if (typeof agentdesk === "undefined" || !agentdesk) {
    return;
  }
  if (agentdesk.phaseGate && agentdesk.phaseGate._installed) {
    return;
  }

  var registry = {};

  function register(name, fn) {
    if (typeof name !== "string" || !name) {
      throw new Error("phaseGate.register: name must be a non-empty string");
    }
    if (typeof fn !== "function") {
      throw new Error("phaseGate.register: fn must be a function");
    }
    if (registry[name]) {
      agentdesk.log.warn("[phase_gate] overwriting existing gate: " + name);
    }
    registry[name] = fn;
    agentdesk.log.debug("[phase_gate] registered: " + name);
  }

  function has(name) {
    return Object.prototype.hasOwnProperty.call(registry, name);
  }

  function names() {
    return Object.keys(registry);
  }

  function evaluate(name, ctx) {
    if (!has(name)) {
      return { passed: false, reason: "phase_gate_unregistered:" + name };
    }
    try {
      var raw = registry[name](ctx || {});
      if (raw === true) return { passed: true };
      if (raw === false) return { passed: false, reason: "phase_gate_failed:" + name };
      if (raw && typeof raw === "object") {
        return {
          passed: !!raw.passed,
          reason: raw.reason || (raw.passed ? undefined : "phase_gate_failed:" + name)
        };
      }
      return { passed: false, reason: "phase_gate_invalid_return:" + name };
    } catch (e) {
      agentdesk.log.warn("[phase_gate] evaluator threw for " + name + ": " + e);
      return { passed: false, reason: "phase_gate_error:" + name + ":" + e };
    }
  }

  agentdesk.phaseGate = {
    _installed: true,
    register: register,
    has: has,
    names: names,
    evaluate: evaluate
  };

  // ── Built-in gates (parity with previous hard-coded behavior) ──────

  register("merge_verified", function(ctx) {
    if (!ctx || !ctx.card_id) return false;
    /* legacy-raw-db: policy=phase_gate capability=merge_verified source_event=phase_gate.evaluate */
    var rows = agentdesk.db.query(
      "SELECT pr_merge_verified_at FROM kanban_cards WHERE id = ?",
      [ctx.card_id]
    );
    return rows.length > 0 && !!rows[0].pr_merge_verified_at;
  });

  register("issue_closed", function(ctx) {
    if (!ctx || !ctx.card_id) return false;
    /* legacy-raw-db: policy=phase_gate capability=issue_closed source_event=phase_gate.evaluate */
    var rows = agentdesk.db.query(
      "SELECT issue_closed_at FROM kanban_cards WHERE id = ?",
      [ctx.card_id]
    );
    return rows.length > 0 && !!rows[0].issue_closed_at;
  });

  register("build_passed", function(ctx) {
    if (!ctx || !ctx.card_id) return false;
    /* legacy-raw-db: policy=phase_gate capability=build_passed source_event=phase_gate.evaluate */
    var rows = agentdesk.db.query(
      "SELECT last_build_status FROM kanban_cards WHERE id = ?",
      [ctx.card_id]
    );
    return rows.length > 0 &&
      (rows[0].last_build_status === "passed" ||
       rows[0].last_build_status === "success");
  });
})();

// Register a named policy so the loader accepts this file. It has no
// hooks — the registry lives on `agentdesk.phaseGate` and is consulted by
// phase-gate dispatch evaluation elsewhere in the code base.
if (typeof agentdesk !== "undefined" &&
    agentdesk &&
    typeof agentdesk.registerPolicy === "function") {
  agentdesk.registerPolicy({
    name: "phase_gate",
    priority: 150
  });
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = {
    // Re-export for tests in Node-style runtimes.
    getRegistry: function() {
      return (typeof agentdesk !== "undefined" && agentdesk.phaseGate) || null;
    }
  };
}
