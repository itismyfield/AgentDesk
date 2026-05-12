/** @module policies/lib/auto-queue-dispatch
 *
 * #1078: Extracted from auto-queue.js as part of the policy modularization pass.
 *
 * Dispatch-activation helpers for the auto-queue policy. Covers:
 *   - pipeline-state classification (terminal/dispatchable/free-path)
 *   - activation result interpretation (count/deferred shape)
 *   - the `activateRun(...)` bridge into the Rust auto-queue surface
 *   - the periodic "rotate active run sweep cursor" recovery primitive
 *
 * Depends on the global `agentdesk` surface and the global `autoQueueLog`
 * binding (wired up by auto-queue.js after requiring auto-queue-log.js).
 */

var _autoQueueLogLib = require("./auto-queue-log");
var autoQueueLog = _autoQueueLogLib.autoQueueLog;

function terminalStatesFromConfig(cfg) {
  var terminalStates = [];
  if (cfg && cfg.states) {
    for (var i = 0; i < cfg.states.length; i++) {
      var state = cfg.states[i];
      if (state && state.terminal && state.id) {
        terminalStates.push(state.id);
      }
    }
  }
  if (terminalStates.length === 0) {
    terminalStates.push("done");
  }
  return terminalStates;
}

function activationDispatchCount(result) {
  if (!result) return null;
  if (typeof result.count === "number") return result.count;
  if (typeof result.dispatched_count === "number") return result.dispatched_count;
  if (typeof result.dispatchedCount === "number") return result.dispatchedCount;
  if (typeof result.activated_count === "number") return result.activated_count;
  if (typeof result.activatedCount === "number") return result.activatedCount;
  return null;
}

function activationWasDeferred(result) {
  return result && result.deferred === true;
}

function rotateActiveRunSweepCursor(runId) {
  if (!runId) return;
  try {
    agentdesk.db.execute(
      "UPDATE auto_queue_entries SET updated_at = datetime('now') WHERE run_id = ? AND status = 'pending'",
      [runId]
    );
  } catch (e) {
    autoQueueLog("warn", "failed to rotate active run sweep cursor for " + runId + ": " + e, {
      run_id: runId
    });
  }
}

function _isDispatchableState(state, cfg) {
  if (!cfg || !cfg.transitions) return false;
  var hasGatedOut = false;
  var hasGatedIn = false;
  for (var i = 0; i < cfg.transitions.length; i++) {
    var t = cfg.transitions[i];
    if (t.from === state && t.type === "gated") hasGatedOut = true;
    if (t.to === state && t.type === "gated") hasGatedIn = true;
  }
  return hasGatedOut && !hasGatedIn;
}

function _dispatchableTargets(cfg) {
  if (!cfg || !cfg.states) return [];
  var targets = [];

  // #255: requested is the canonical preflight anchor when present.
  if (agentdesk.pipeline.hasState("requested", cfg)) {
    targets.push("requested");
  }

  for (var i = 0; i < cfg.states.length; i++) {
    var s = cfg.states[i];
    if (s.terminal) continue;
    if (!_isDispatchableState(s.id, cfg)) continue;
    if (targets.indexOf(s.id) === -1) targets.push(s.id);
  }
  return targets;
}

function _freePathToDispatchable(from, cfg) {
  var targets = _dispatchableTargets(cfg);
  if (targets.length === 0) return null;
  if (targets.indexOf(from) >= 0) return [];
  if (!cfg || !cfg.transitions) return null;

  var queue = [from];
  var visited = {};
  var parent = {};
  visited[from] = true;

  while (queue.length > 0) {
    var cur = queue.shift();
    for (var i = 0; i < cfg.transitions.length; i++) {
      var t = cfg.transitions[i];
      if (t.from !== cur || t.type !== "free" || visited[t.to]) continue;
      parent[t.to] = cur;
      if (targets.indexOf(t.to) >= 0) {
        var path = [t.to];
        var p = cur;
        while (p && p !== from) {
          path.unshift(p);
          p = parent[p];
        }
        return path;
      }
      visited[t.to] = true;
      queue.push(t.to);
    }
  }

  return null;
}

function activateRun(runId, threadGroup, agentId) {
  if (!runId) return null;
  try {
    if (agentId !== null && agentId !== undefined) {
      var body = {
        run_id: runId,
        active_only: true,
        agent_id: agentId
      };
      if (threadGroup !== null && threadGroup !== undefined) {
        body.thread_group = threadGroup;
      }
      return agentdesk.autoQueue.activate(body);
    }
    return agentdesk.autoQueue.activate(runId, threadGroup);
  } catch (e) {
    autoQueueLog("warn", "activate bridge failed for run " + runId + ": " + e, {
      run_id: runId,
      thread_group: threadGroup,
      agent_id: agentId || null
    });
    return null;
  }
}

module.exports = {
  terminalStatesFromConfig: terminalStatesFromConfig,
  activationDispatchCount: activationDispatchCount,
  activationWasDeferred: activationWasDeferred,
  rotateActiveRunSweepCursor: rotateActiveRunSweepCursor,
  isDispatchableState: _isDispatchableState,
  dispatchableTargets: _dispatchableTargets,
  freePathToDispatchable: _freePathToDispatchable,
  activateRun: activateRun
};
