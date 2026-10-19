/** @module policies/lib/auto-queue-lifecycle
 *
 * #1078: Extracted from auto-queue.js as part of the policy modularization pass.
 *
 * Run-lifecycle helpers: continuing the queue after an entry finishes,
 * requesting derived run completion from the canonical Rust writer, and
 * resuming a run after a phase gate passes. The control flow here is driven by Rust
 * lifecycle events (`onCardTerminal`) — the policy's job is to bridge into
 * the correct phase-gate creation, dispatch activation, or run-complete
 * call given the remaining entry topology.
 *
 * Depends on:
 *   - `policies/lib/auto-queue-log` for structured logging
 *   - `policies/lib/auto-queue-dispatch` for `activateRun(...)`
 *   - `policies/lib/auto-queue-phase-gate` for phase-gate state, the
 *     `pauseRun(...)` bridge, grace-window primitives, and gate dispatch
 *     creation.
 */

var _autoQueueLogLib = require("./auto-queue-log");
var _autoQueueDispatchLib = require("./auto-queue-dispatch");
var _autoQueuePhaseGateLib = require("./auto-queue-phase-gate");

var autoQueueLog = _autoQueueLogLib.autoQueueLog;
var activateRun = _autoQueueDispatchLib.activateRun;
var beginPhaseGateGraceWindow = _autoQueuePhaseGateLib.beginPhaseGateGraceWindow;
var clearPhaseGateGraceWindow = _autoQueuePhaseGateLib.clearPhaseGateGraceWindow;
var _createPhaseGateDispatches = _autoQueuePhaseGateLib.createPhaseGateDispatches;
var _phaseGateRequired = _autoQueuePhaseGateLib.phaseGateRequired;

function loadRunInfo(runId) {
  var rows = agentdesk.db.query(
    "SELECT status, repo, unified_thread_id, unified_thread_channel_id, COALESCE(thread_group_count, 1) as group_count " +
    "FROM auto_queue_runs WHERE id = ?",
    [runId]
  );
  return rows.length > 0 ? rows[0] : null;
}

function describeCompletionOutcome(result) {
  if (!result) return "no result";
  if (result.changed) return "completed";
  var outcome = result.outcome || "unchanged";
  return result.blocked_reason ? outcome + ":" + result.blocked_reason : outcome;
}

// Derived completion: the Rust writer re-checks blocking gates, the grace
// window, `user_cancelled` entries and runnable entries under the run token,
// so this helper only reports its typed outcome.
function finalizeRunWithoutPhaseGate(runId) {
  if (!runId) return false;

  var result;
  try {
    result = agentdesk.autoQueue.completeRun(runId, "finalize_without_phase_gate");
  } catch (e) {
    autoQueueLog("warn", "Failed to finalize run " + runId + ": " + e, {
      run_id: runId
    });
    return false;
  }
  if (!result || !result.changed) {
    autoQueueLog("info", "Deferring finalize for run " + runId + " — " + describeCompletionOutcome(result), {
      run_id: runId
    });
    return false;
  }

  autoQueueLog("info", "Finalized non-phase-gate run " + runId + " and released its slots", {
    run_id: runId
  });
  return true;
}

function completeRunAndNotify(runId) {
  if (!runId) return false;
  // The final gate row has been cleared, so the grace window that guarded its
  // creation no longer protects anything and must not defer completion.
  clearPhaseGateGraceWindow(runId);
  try {
    var completed = agentdesk.autoQueue.completeRun(
      runId,
      "phase_gate_complete"
    );
    if (completed && completed.changed) return true;
    if (completed && completed.outcome === "already_terminal") {
      autoQueueLog("info", "Phase-gate completion found run " + runId + " already terminal", {
        run_id: runId
      });
      return true;
    }
    autoQueueLog("warn", "Phase-gate completion did not mark run " + runId + " completed (" + describeCompletionOutcome(completed) + "); falling back to resume", {
      run_id: runId
    });
  } catch (e) {
    autoQueueLog("warn", "Failed to complete final phase-gate run " + runId + ": " + e, {
      run_id: runId
    });
  }
  try {
    agentdesk.autoQueue.resumeRun(runId, "phase_gate_complete_resume_fallback");
  } catch (e) {
    autoQueueLog("warn", "Failed to resume final phase-gate run " + runId + ": " + e, {
      run_id: runId
    });
  }
  activateRun(runId, null);
  return false;
}

function continueRunAfterEntry(runId, agentId, doneGroup, donePhase, anchorCardId) {
  if (!runId || !agentId) return;

  // #747 round-2: Open the phase-gate grace window BEFORE we do any work so
  // an overlapping `onTick1min.finalizeRunWithoutPhaseGate` on the tick
  // engine cannot steal-complete the run. Cleared on all non-phase-gate
  // exits below; `_createPhaseGateDispatches` leaves the gate in place (the
  // pending phase-gate row itself now guards the run via
  // `runHasBlockingPhaseGate`).
  if (!beginPhaseGateGraceWindow(runId)) {
    // The run completed before this continuation: no phase gate or next group may follow.
    autoQueueLog("warn", "Run " + runId + " is completed or gone; skipping its phase gate and continuation", {
      run_id: runId
    });
    return;
  }

  var effectiveDonePhase = (donePhase !== null && donePhase !== undefined) ? donePhase : -1;

  var remainingStatsRows = agentdesk.db.query(
    "SELECT " +
    "  COUNT(*) as total_remaining, " +
    "  SUM(CASE WHEN COALESCE(batch_phase, 0) = ? THEN 1 ELSE 0 END) as phase_remaining, " +
    "  SUM(CASE WHEN COALESCE(thread_group, 0) = ? THEN 1 ELSE 0 END) as group_remaining " +
    "FROM auto_queue_entries " +
    "WHERE run_id = ? AND status IN ('pending', 'dispatched')",
    [effectiveDonePhase, doneGroup || 0, runId]
  );

  var remainingCount = remainingStatsRows.length > 0 ? (remainingStatsRows[0].total_remaining || 0) : 0;

  if (effectiveDonePhase >= 0) {
    var currentPhaseDone = remainingStatsRows.length > 0 ? (remainingStatsRows[0].phase_remaining || 0) === 0 : true;
    if (currentPhaseDone) {
      var nextPhaseRows = agentdesk.db.query(
        "SELECT MIN(batch_phase) as next_phase FROM auto_queue_entries " +
        "WHERE run_id = ? AND status IN ('pending', 'dispatched') AND COALESCE(batch_phase, 0) > ?",
        [runId, donePhase]
      );
      var nextPhase = (nextPhaseRows.length > 0) ? nextPhaseRows[0].next_phase : null;
      if (_phaseGateRequired(runId, donePhase)) {
        var finalPhase = remainingCount === 0;
        _createPhaseGateDispatches(runId, donePhase, nextPhase, finalPhase, anchorCardId);
        // Intentionally leave grace window in place: the phase-gate row
        // created above now guards the run; grace naturally expires.
        return;
      }
      if (nextPhase !== null && nextPhase !== undefined) {
        var nextPhaseCountRows = agentdesk.db.query(
          "SELECT COUNT(*) as cnt FROM auto_queue_entries " +
          "WHERE run_id = ? AND status IN ('pending', 'dispatched') AND COALESCE(batch_phase, 0) = ?",
          [runId, nextPhase]
        );
        var nextPhaseCount = (nextPhaseCountRows.length > 0) ? nextPhaseCountRows[0].cnt : 0;
        agentdesk.log.info("[auto-queue] Phase " + donePhase + " 완료, Phase " + nextPhase + " 시작 (" + nextPhaseCount + " entries)");
        clearPhaseGateGraceWindow(runId);
        activateRun(runId, null);
        return;
      }
    }
  }

  if (remainingCount === 0) {
    // No more work AND no phase gate required → grace window no longer
    // needed. Clear it so finalization can proceed immediately.
    clearPhaseGateGraceWindow(runId);
    if (!finalizeRunWithoutPhaseGate(runId)) {
      completeRunAndNotify(runId);
    }
    return;
  }

  var groupDone = remainingStatsRows.length > 0 ? (remainingStatsRows[0].group_remaining || 0) === 0 : true;

  var tCfg = agentdesk.pipeline.getConfig();
  var tKickoff = agentdesk.pipeline.kickoffState(tCfg);
  var tInProgress = agentdesk.pipeline.nextGatedTarget(tKickoff, tCfg);
  var tReview = agentdesk.pipeline.nextGatedTarget(tInProgress, tCfg);
  var activeStates = [tKickoff, tInProgress, tReview].filter(function(s) { return s; });
  var agentBusy = false;
  if (activeStates.length > 0) {
    var placeholders = activeStates.map(function() { return "?"; }).join(",");
    var active = agentdesk.db.query(
      "SELECT id FROM kanban_cards WHERE assigned_agent_id = ? AND status IN (" + placeholders + ") LIMIT 1",
      [agentId].concat(activeStates)
    );
    agentBusy = active.length > 0;
  }

  // Grace window no longer needed past this point — either we keep
  // dispatching in the same group (no phase transition happened) or we
  // move to the next group. Either way, there is no phase-gate dispatch
  // window to protect.
  clearPhaseGateGraceWindow(runId);

  if (!groupDone) {
    if (!agentBusy) {
      activateRun(runId, doneGroup || 0, agentId);
    } else {
      agentdesk.log.info("[auto-queue] Agent " + agentId + " still busy, deferring group " + (doneGroup || 0) + " next dispatch");
    }
    return;
  }

  activateRun(runId, null, agentId);
}

function resumeRunAndActivate(runId, nextPhase) {
  try {
    agentdesk.autoQueue.resumeRun(runId, "phase_gate_resume");
  } catch (e) {
    autoQueueLog("warn", "Failed to resume run " + runId + ": " + e, {
      run_id: runId,
      batch_phase: nextPhase !== undefined ? nextPhase : null
    });
  }
  if (nextPhase !== null && nextPhase !== undefined) {
    autoQueueLog("info", "Resuming run " + runId + " for phase " + nextPhase, {
      run_id: runId,
      batch_phase: nextPhase
    });
  }
  activateRun(runId, null);
}

module.exports = {
  loadRunInfo: loadRunInfo,
  finalizeRunWithoutPhaseGate: finalizeRunWithoutPhaseGate,
  completeRunAndNotify: completeRunAndNotify,
  continueRunAfterEntry: continueRunAfterEntry,
  resumeRunAndActivate: resumeRunAndActivate
};
