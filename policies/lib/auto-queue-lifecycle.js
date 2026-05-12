/** @module policies/lib/auto-queue-lifecycle
 *
 * #1078: Extracted from auto-queue.js as part of the policy modularization pass.
 *
 * Run-lifecycle helpers: continuing the queue after an entry finishes,
 * finalizing runs that no longer have runnable work, and resuming a run
 * after a phase gate passes. The control flow here is driven by Rust
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
var runHasBlockingPhaseGate = _autoQueuePhaseGateLib.runHasBlockingPhaseGate;
var beginPhaseGateGraceWindow = _autoQueuePhaseGateLib.beginPhaseGateGraceWindow;
var clearPhaseGateGraceWindow = _autoQueuePhaseGateLib.clearPhaseGateGraceWindow;
var runWithinPhaseGateGrace = _autoQueuePhaseGateLib.runWithinPhaseGateGrace;
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

function remainingRunnableEntryCount(runId, phase) {
  var sql =
    "SELECT COUNT(*) as cnt FROM auto_queue_entries " +
    "WHERE run_id = ? AND status IN ('pending', 'dispatched')";
  var params = [runId];
  if (phase !== null && phase !== undefined) {
    sql += " AND COALESCE(batch_phase, 0) = ?";
    params.push(phase);
  }
  var rows = agentdesk.db.query(sql, params);
  return (rows.length > 0) ? rows[0].cnt : 0;
}

function runHasUserCancelledEntry(runId) {
  var rows = agentdesk.db.query(
    "SELECT COUNT(*) as cnt FROM auto_queue_entries " +
    "WHERE run_id = ? AND status = 'user_cancelled'",
    [runId]
  );
  return (rows.length > 0) && rows[0].cnt > 0;
}

function finalizeRunWithoutPhaseGate(runId) {
  if (!runId) return false;

  if (runHasBlockingPhaseGate(runId)) return false;
  if (remainingRunnableEntryCount(runId) > 0) return false;
  // #815 P1: `user_cancelled` entries are operator-held terminal state.
  // They are intentionally non-runnable, but they must still block the
  // tick-side backstop from auto-completing the run; otherwise the next
  // minute tick would strand a user-stopped run in `completed`.
  if (runHasUserCancelledEntry(runId)) {
    autoQueueLog("info", "Deferring finalize for run " + runId + " — user_cancelled entry still present", {
      run_id: runId
    });
    return false;
  }
  // Phase-gate race guard: the main engine's `onCardTerminal` may still be
  // in the middle of creating gate dispatches. Respect the grace window so
  // we never mark a run completed before phase gates get registered.
  if (runWithinPhaseGateGrace(runId)) {
    autoQueueLog("info", "Deferring finalize for run " + runId + " — phase-gate grace window active", {
      run_id: runId
    });
    return false;
  }

  var completed = false;
  try {
    var result = agentdesk.autoQueue.completeRun(
      runId,
      "finalize_without_phase_gate",
      { releaseSlots: true }
    );
    completed = !!(result && result.changed);
  } catch (e) {
    autoQueueLog("warn", "Failed to finalize run " + runId + ": " + e, {
      run_id: runId
    });
    return false;
  }
  if (!completed) return false;

  autoQueueLog("info", "Finalized non-phase-gate run " + runId + " and released its slots", {
    run_id: runId
  });
  return true;
}

function completeRunAndNotify(runId) {
  if (!runId) return;
  try {
    var completed = agentdesk.autoQueue.completeRun(
      runId,
      "phase_gate_complete",
      { releaseSlots: true }
    );
    if (completed && completed.changed) return;
    autoQueueLog("warn", "Phase-gate completion did not mark run " + runId + " completed; falling back to resume", {
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
}

function continueRunAfterEntry(runId, agentId, doneGroup, donePhase, anchorCardId) {
  if (!runId || !agentId) return;

  // #747 round-2: Open the phase-gate grace window BEFORE we do any work so
  // an overlapping `onTick1min.finalizeRunWithoutPhaseGate` on the tick
  // engine cannot steal-complete the run. Cleared on all non-phase-gate
  // exits below; `_createPhaseGateDispatches` leaves the gate in place (the
  // pending phase-gate row itself now guards the run via
  // `runHasBlockingPhaseGate`).
  beginPhaseGateGraceWindow(runId);

  var remainingCount = remainingRunnableEntryCount(runId, null);

  var effectiveDonePhase = (donePhase !== null && donePhase !== undefined) ? donePhase : -1;
  if (effectiveDonePhase >= 0) {
    var currentPhaseDone = remainingRunnableEntryCount(runId, donePhase) === 0;
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

  var groupRemaining = agentdesk.db.query(
    "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND COALESCE(thread_group, 0) = ? AND status IN ('pending', 'dispatched')",
    [runId, doneGroup || 0]
  );
  var groupDone = groupRemaining.length > 0 && groupRemaining[0].cnt === 0;

  var tCfg = agentdesk.pipeline.getConfig();
  var tKickoff = agentdesk.pipeline.kickoffState(tCfg);
  var tInProgress = agentdesk.pipeline.nextGatedTarget(tKickoff, tCfg);
  var tReview = agentdesk.pipeline.nextGatedTarget(tInProgress, tCfg);
  var activeStates = [tKickoff, tInProgress, tReview].filter(function(s) { return s; });
  var agentBusy = false;
  if (activeStates.length > 0) {
    var placeholders = activeStates.map(function() { return "?"; }).join(",");
    var active = agentdesk.db.query(
      "SELECT COUNT(*) as cnt FROM kanban_cards WHERE assigned_agent_id = ? AND status IN (" + placeholders + ")",
      [agentId].concat(activeStates)
    );
    agentBusy = active.length > 0 && active[0].cnt > 0;
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
  remainingRunnableEntryCount: remainingRunnableEntryCount,
  runHasUserCancelledEntry: runHasUserCancelledEntry,
  finalizeRunWithoutPhaseGate: finalizeRunWithoutPhaseGate,
  completeRunAndNotify: completeRunAndNotify,
  continueRunAfterEntry: continueRunAfterEntry,
  resumeRunAndActivate: resumeRunAndActivate
};
