/* giant-file-exemption: reason=auto-queue-phase-gate-pending-split ticket=#1078 */
// #1078: auto-queue.js is the policy entrypoint. The helper bodies have been
// extracted to the policies/lib/ submodules so each module owns a single
// concern. The wiring below is a 1:1 alias of the original top-level
// helpers — public hook semantics and `module.exports` are unchanged.
//
//   - policies/lib/auto-queue-log.js              structured logging
//   - policies/lib/auto-queue-config.js           runtime config readers
//   - policies/lib/auto-queue-dispatch.js         activation + pipeline helpers
//   - policies/lib/auto-queue-phase-gate.js       phase-gate verdict / state / autoclose
//   - policies/lib/auto-queue-lifecycle.js        run continuation / finalize / resume
//   - policies/lib/auto-queue-error-recovery.js   stuck-entry alerts + consultation dispatch
//
// The `autoQueue` policy object + `module.exports` shape are preserved verbatim
// so callers (registerPolicy(), `__test.inferPhaseGatePassVerdict`, etc.) keep
// working without changes.
var _autoQueueLogLib = require("./lib/auto-queue-log");
var _autoQueueConfigLib = require("./lib/auto-queue-config");
var _autoQueueDispatchLib = require("./lib/auto-queue-dispatch");
var _autoQueuePhaseGateLib = require("./lib/auto-queue-phase-gate");
var _autoQueueLifecycleLib = require("./lib/auto-queue-lifecycle");
var _autoQueueErrorRecoveryLib = require("./lib/auto-queue-error-recovery");

// ── log helpers (re-export the names the rest of the file used) ────────
var _autoQueueHasValue = _autoQueueLogLib.hasValue;
var _autoQueueLogContextKeys = _autoQueueLogLib.logContextKeys;
var _mergeAutoQueueLogContext = _autoQueueLogLib.mergeLogContext;
var _loadAutoQueueEntryLogContext = _autoQueueLogLib.loadEntryLogContext;
var _loadAutoQueueDispatchLogContext = _autoQueueLogLib.loadDispatchLogContext;
var _normalizeAutoQueueLogContext = _autoQueueLogLib.normalizeLogContext;
var _formatAutoQueueLogContext = _autoQueueLogLib.formatLogContext;
var autoQueueLog = _autoQueueLogLib.autoQueueLog;

// ── runtime config readers ────────────────────────────────────────────
var configuredAutoQueueMaxEntryRetries = _autoQueueConfigLib.maxEntryRetries;
var configuredStaleDispatchedGraceMinutes = _autoQueueConfigLib.staleDispatchedGraceMinutes;
var configuredStaleDispatchedTerminalStatuses = _autoQueueConfigLib.staleDispatchedTerminalStatuses;
var configuredStaleDispatchedRecoverNullDispatch = _autoQueueConfigLib.staleDispatchedRecoverNullDispatch;
var configuredStaleDispatchedRecoverMissingDispatch = _autoQueueConfigLib.staleDispatchedRecoverMissingDispatch;
var staleDispatchedRecoveryConditionsSql = _autoQueueConfigLib.staleDispatchedRecoveryConditionsSql;

// ── dispatch / activation helpers ────────────────────────────────────
var terminalStatesFromConfig = _autoQueueDispatchLib.terminalStatesFromConfig;
var activationDispatchCount = _autoQueueDispatchLib.activationDispatchCount;
var activationWasDeferred = _autoQueueDispatchLib.activationWasDeferred;
var rotateActiveRunSweepCursor = _autoQueueDispatchLib.rotateActiveRunSweepCursor;
var _isDispatchableState = _autoQueueDispatchLib.isDispatchableState;
var _dispatchableTargets = _autoQueueDispatchLib.dispatchableTargets;
var _freePathToDispatchable = _autoQueueDispatchLib.freePathToDispatchable;
var activateRun = _autoQueueDispatchLib.activateRun;

// ── phase-gate helpers ──────────────────────────────────────────────
var PHASE_GATE_HUMAN_ESCALATION_THRESHOLD = _autoQueuePhaseGateLib.PHASE_GATE_HUMAN_ESCALATION_THRESHOLD;
var PHASE_GATE_FAILURE_TTL_SEC = _autoQueuePhaseGateLib.PHASE_GATE_FAILURE_TTL_SEC;
var PHASE_GATE_ALERT_DEBOUNCE_TTL_SEC = _autoQueuePhaseGateLib.PHASE_GATE_ALERT_DEBOUNCE_TTL_SEC;
var PHASE_GATE_AUTOCLOSE_TTL_SEC = _autoQueuePhaseGateLib.PHASE_GATE_AUTOCLOSE_TTL_SEC;
var PHASE_GATE_GRACE_WINDOW_MS = _autoQueuePhaseGateLib.PHASE_GATE_GRACE_WINDOW_MS;
var _inferPhaseGatePassVerdict = _autoQueuePhaseGateLib.inferPhaseGatePassVerdict;
var phaseGateFailureKey = _autoQueuePhaseGateLib.phaseGateFailureKey;
var incrementPhaseGateFailureCount = _autoQueuePhaseGateLib.incrementPhaseGateFailureCount;
var resetPhaseGateFailureCount = _autoQueuePhaseGateLib.resetPhaseGateFailureCount;
var loadPhaseGateCardLabel = _autoQueuePhaseGateLib.loadPhaseGateCardLabel;
var handlePhaseGateFailure = _autoQueuePhaseGateLib.handlePhaseGateFailure;
var _maybeAlertPhaseGateVerdictMismatch = _autoQueuePhaseGateLib.maybeAlertPhaseGateVerdictMismatch;
var _phaseGateOnlyIssueClosedFailing = _autoQueuePhaseGateLib.phaseGateOnlyIssueClosedFailing;
var _loadCardForPhaseGateFallback = _autoQueuePhaseGateLib.loadCardForPhaseGateFallback;
var _extractRepoSlugFromIssueUrl = _autoQueuePhaseGateLib.extractRepoSlugFromIssueUrl;
var _attemptPhaseGateAutoCloseFallback = _autoQueuePhaseGateLib.attemptPhaseGateAutoCloseFallback;
var loadPhaseGateState = _autoQueuePhaseGateLib.loadPhaseGateState;
var savePhaseGateState = _autoQueuePhaseGateLib.savePhaseGateState;
var clearPhaseGateState = _autoQueuePhaseGateLib.clearPhaseGateState;
var runHasBlockingPhaseGate = _autoQueuePhaseGateLib.runHasBlockingPhaseGate;
var beginPhaseGateGraceWindow = _autoQueuePhaseGateLib.beginPhaseGateGraceWindow;
var clearPhaseGateGraceWindow = _autoQueuePhaseGateLib.clearPhaseGateGraceWindow;
var runWithinPhaseGateGrace = _autoQueuePhaseGateLib.runWithinPhaseGateGrace;
var pauseRun = _autoQueuePhaseGateLib.pauseRun;
var loadPhaseGateDispatches = _autoQueuePhaseGateLib.loadPhaseGateDispatches;
var _phaseGateRequired = _autoQueuePhaseGateLib.phaseGateRequired;
var _buildPhaseGateGroups = _autoQueuePhaseGateLib.buildPhaseGateGroups;
var _phaseGateTitle = _autoQueuePhaseGateLib.phaseGateTitle;
var _createPhaseGateDispatches = _autoQueuePhaseGateLib.createPhaseGateDispatches;

// ── lifecycle helpers ───────────────────────────────────────────────
var loadRunInfo = _autoQueueLifecycleLib.loadRunInfo;
var remainingRunnableEntryCount = _autoQueueLifecycleLib.remainingRunnableEntryCount;
var runHasUserCancelledEntry = _autoQueueLifecycleLib.runHasUserCancelledEntry;
var finalizeRunWithoutPhaseGate = _autoQueueLifecycleLib.finalizeRunWithoutPhaseGate;
var completeRunAndNotify = _autoQueueLifecycleLib.completeRunAndNotify;
var continueRunAfterEntry = _autoQueueLifecycleLib.continueRunAfterEntry;
var resumeRunAndActivate = _autoQueueLifecycleLib.resumeRunAndActivate;

// ── error recovery helpers ──────────────────────────────────────────
var notifyAutoQueueEntryFailure = _autoQueueErrorRecoveryLib.notifyAutoQueueEntryFailure;
var _createConsultationDispatch = _autoQueueErrorRecoveryLib.createConsultationDispatch;

var autoQueue = {
  name: "auto-queue",
  priority: 500,

  // ── Auto-skip: detect cards progressed outside of auto-queue ──
  // If a pending queue entry's card gets dispatched externally (by PMD, user, etc.),
  // skip the entry so auto-queue doesn't try to dispatch it again.
  onCardTransition: function(payload) {
    if (payload.source === "auto-queue-walk" || payload.source === "auto-queue-generate") {
      return;
    }
    var aqCfg = agentdesk.pipeline.getConfig();
    var aqKickoff = agentdesk.pipeline.kickoffState(aqCfg);
    var aqNext = agentdesk.pipeline.nextGatedTarget(aqKickoff, aqCfg);
    if (payload.to !== aqKickoff && payload.to !== aqNext) return;
    var entries = agentdesk.db.query(
      "SELECT e.id FROM auto_queue_entries e " +
      "WHERE e.kanban_card_id = ? AND e.status = 'pending'",
      [payload.card_id]
    );
    for (var i = 0; i < entries.length; i++) {
      agentdesk.autoQueue.updateEntryStatus(
        entries[i].id,
        "skipped",
        "external_progress"
      );
      autoQueueLog("info", "Skipped entry " + entries[i].id + " — card " + payload.card_id + " progressed externally to " + payload.to, {
        entry_id: entries[i].id,
        card_id: payload.card_id
      });
    }
  },

  // ── Authoritative auto-queue continuation (#110, #140) ──────────────
  // This is the SINGLE path for done → next queued item.
  // Rust transition_status() already marks auto_queue_entries as 'done'
  // before firing OnCardTerminal and now defers final run completion here so
  // single-phase runs can still create a phase gate before they finish.
  // kanban-rules.js does NOT touch auto_queue_entries (removed in #110).
  // #140: Group-aware continuation — dispatches next entry in same group,
  //       and starts new groups when slots become available.
  onCardTerminal: function(payload) {
    var cards = agentdesk.db.query(
      "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?",
      [payload.card_id]
    );
    if (cards.length === 0 || !cards[0].assigned_agent_id) return;

    var agentId = cards[0].assigned_agent_id;

    // #145/#295: Prefer the just-finished `done` entry for continuation. Sibling
    // runs may also be auto-skipped for the same card, but they must not steal
    // continuation from the originating run.
    var doneEntries = agentdesk.db.query(
      "SELECT e.run_id, COALESCE(e.thread_group, 0) as thread_group, COALESCE(e.batch_phase, 0) as batch_phase FROM auto_queue_entries e " +
      "JOIN auto_queue_runs r ON e.run_id = r.id " +
      "WHERE e.kanban_card_id = ? AND e.status IN ('done', 'skipped') " +
      "AND r.status IN ('active', 'paused') " +
      "ORDER BY CASE WHEN e.status = 'done' THEN 0 ELSE 1 END ASC, e.completed_at DESC LIMIT 1",
      [payload.card_id]
    );
    if (!doneEntries || doneEntries.length === 0 || !doneEntries[0] || !doneEntries[0].run_id) {
      return;
    }

    continueRunAfterEntry(
      doneEntries[0].run_id,
      agentId,
      doneEntries[0].thread_group,
      doneEntries[0].batch_phase || 0,
      payload.card_id
    );
  },

  onDispatchCompleted: function(payload) {
    var dispatches = agentdesk.db.query(
      "SELECT id, kanban_card_id, dispatch_type, result, context FROM task_dispatches WHERE id = ?",
      [payload.dispatch_id]
    );
    if (dispatches.length === 0) return;

    var dispatch = dispatches[0];
    var context = {};
    try { context = JSON.parse(dispatch.context || "{}"); } catch (e) { context = {}; }
    var result = {};
    try { result = JSON.parse(dispatch.result || "{}"); } catch (e) { result = {}; }
    var gate = context.phase_gate;
    if (!gate || !gate.run_id || gate.batch_phase == null) {
      return;
    }

    var phase = null;
    if (typeof gate.batch_phase === "number") {
      phase = gate.batch_phase;
    } else if (typeof gate.batch_phase === "string" && gate.batch_phase.trim() !== "") {
      phase = Number(gate.batch_phase);
    }
    if (!Number.isFinite(phase) || Math.floor(phase) !== phase || phase < 0) {
      autoQueueLog("warn", "Ignoring phase gate completion with invalid batch_phase", {
        run_id: gate.run_id,
        dispatch_id: dispatch.id,
        card_id: dispatch.kanban_card_id,
        batch_phase: gate.batch_phase
      });
      return;
    }
    var state = loadPhaseGateState(gate.run_id, phase);
    if (!state || !Array.isArray(state.dispatch_ids) || state.dispatch_ids.indexOf(dispatch.id) < 0) {
      return;
    }
    if (state.status === "failed") {
      autoQueueLog("info", "Ignoring phase gate completion for failed run " + gate.run_id + " phase " + phase, {
        run_id: gate.run_id,
        dispatch_id: dispatch.id,
        card_id: dispatch.kanban_card_id,
        batch_phase: phase
      });
      return;
    }

    var verdict = result.verdict || result.decision || null;
    var passVerdict = gate.pass_verdict || "phase_gate_passed";

    // #699 fallback: legacy callers may have persisted a phase-gate result
    // with every declared check passing but no explicit `verdict`. The
    // server finalize path now injects the verdict, but this guard recovers
    // rows stored before the fix shipped. Never infer "pass" when any check
    // reports fail, and never override an explicit verdict/decision.
    if (!verdict) {
      var inferred = _inferPhaseGatePassVerdict(context, result);
      if (inferred) {
        verdict = inferred;
        autoQueueLog("info", "Inferred phase gate verdict '" + inferred + "' for dispatch " + dispatch.id + " (no explicit verdict)", {
          run_id: gate.run_id,
          dispatch_id: dispatch.id,
          card_id: dispatch.kanban_card_id,
          batch_phase: phase
        });
      }
    }

    if (verdict !== passVerdict) {
      // #2035: before failing this gate, try the issue_closed-only fallback.
      // Conservative entry conditions: merge_verified=pass, build_passed=pass,
      // ONLY issue_closed failing, same commit hash recorded on the card, and
      // a per-(card, phase, commit) one-shot guard. The fallback issues a
      // `gh issue close` and then we restart this hook so the gate re-checks
      // against fresh `issue_closed_at` state. Failures fall through to the
      // existing pauseRun path so nothing is silently swallowed.
      var fallback = _attemptPhaseGateAutoCloseFallback(
        gate.run_id, phase, dispatch.id, context, result
      );
      if (fallback.attempted) {
        autoQueueLog("info", "Phase gate autoclose fallback attempted for run " + gate.run_id + " phase " + phase + " — anyClosed=" + !!fallback.anyClosed, {
          run_id: gate.run_id,
          dispatch_id: dispatch.id,
          card_id: state.anchor_card_id || dispatch.kanban_card_id,
          batch_phase: phase
        });
        if (fallback.anyClosed) {
          // Re-evaluate exactly once with the fresh issue_closed_at state.
          // We do NOT pauseRun here so the queue continues if all checks
          // now pass. If they still do not pass, the second invocation will
          // skip the fallback (one-shot guard) and fall through to the
          // original pauseRun path.
          try {
            autoQueue.onDispatchCompleted({ dispatch_id: dispatch.id });
          } catch (e) {
            autoQueueLog("warn", "Phase gate re-evaluation after autoclose threw: " + e, {
              run_id: gate.run_id,
              dispatch_id: dispatch.id,
              card_id: state.anchor_card_id || dispatch.kanban_card_id,
              batch_phase: phase
            });
          }
          return;
        }
      }
      state.status = "failed";
      state.failed_dispatch_id = dispatch.id;
      state.failed_verdict = verdict;
      state.failed_reason = result.summary || result.reason || ("expected " + passVerdict + ", got " + (verdict || "none"));
      savePhaseGateState(gate.run_id, phase, state);
      pauseRun(gate.run_id);
      // #2035: surface verdict mismatch as a discord alert (debounced 1/hr).
      _maybeAlertPhaseGateVerdictMismatch(
        gate.run_id, phase,
        state.anchor_card_id || dispatch.kanban_card_id,
        state.failed_reason
      );
      handlePhaseGateFailure(
        state.anchor_card_id || dispatch.kanban_card_id,
        phase,
        "[phase-gate] phase " + phase + " failed: " + state.failed_reason,
        {
          run_id: gate.run_id,
          dispatch_id: dispatch.id,
          card_id: state.anchor_card_id || dispatch.kanban_card_id,
          batch_phase: phase
        }
      );
      autoQueueLog("warn", "Phase gate failed for run " + gate.run_id + " phase " + phase + ": " + state.failed_reason, {
        run_id: gate.run_id,
        dispatch_id: dispatch.id,
        card_id: state.anchor_card_id || dispatch.kanban_card_id,
        batch_phase: phase
      });
      return;
    }

    var gateDispatches = loadPhaseGateDispatches(state.dispatch_ids);
    if (gateDispatches.length !== state.dispatch_ids.length) {
      state.status = "failed";
      state.failed_dispatch_id = dispatch.id;
      state.failed_reason = "missing phase gate dispatch rows";
      savePhaseGateState(gate.run_id, phase, state);
      pauseRun(gate.run_id);
      handlePhaseGateFailure(
        state.anchor_card_id || dispatch.kanban_card_id,
        phase,
        "[phase-gate] phase " + phase + " failed: missing gate dispatch rows",
        {
          run_id: gate.run_id,
          dispatch_id: dispatch.id,
          card_id: state.anchor_card_id || dispatch.kanban_card_id,
          batch_phase: phase
        }
      );
      return;
    }

    var pendingCount = 0;
    for (var i = 0; i < gateDispatches.length; i++) {
      var gateDispatch = gateDispatches[i];
      if (gateDispatch.status === "pending" || gateDispatch.status === "dispatched") {
        pendingCount++;
        continue;
      }
      var gateContext = {};
      var gateResult = {};
      try { gateContext = JSON.parse(gateDispatch.context || "{}"); } catch (e) { gateContext = {}; }
      try { gateResult = JSON.parse(gateDispatch.result || "{}"); } catch (e) { gateResult = {}; }
      var expectedVerdict = (gateContext.phase_gate && gateContext.phase_gate.pass_verdict) || "phase_gate_passed";
      var gateVerdict = gateResult.verdict || gateResult.decision || null;
      // #699 (round 2): sibling gate dispatches persisted before the server
      // fix shipped will still be missing `verdict`. Re-run the inference
      // here so the aggregate gate evaluation does not trip on legacy rows.
      if (!gateVerdict) {
        var siblingInferred = _inferPhaseGatePassVerdict(gateContext, gateResult);
        if (siblingInferred) {
          gateVerdict = siblingInferred;
          autoQueueLog("info", "Inferred sibling phase gate verdict '" + siblingInferred + "' for dispatch " + gateDispatch.id + " (legacy row, no explicit verdict)", {
            run_id: gate.run_id,
            dispatch_id: gateDispatch.id,
            card_id: state.anchor_card_id || dispatch.kanban_card_id,
            batch_phase: phase
          });
        }
      }
      if (gateDispatch.status !== "completed" || gateVerdict !== expectedVerdict) {
        // #2035: sibling gate path — try the issue_closed-only autoclose
        // fallback before failing. Same one-shot guard as the primary path.
        if (gateDispatch.status === "completed") {
          var siblingFallback = _attemptPhaseGateAutoCloseFallback(
            gate.run_id, phase, gateDispatch.id, gateContext, gateResult
          );
          if (siblingFallback.attempted && siblingFallback.anyClosed) {
            autoQueueLog("info", "Phase gate autoclose fallback attempted for sibling dispatch " + gateDispatch.id + " — anyClosed=true", {
              run_id: gate.run_id,
              dispatch_id: gateDispatch.id,
              card_id: state.anchor_card_id || dispatch.kanban_card_id,
              batch_phase: phase
            });
            try {
              autoQueue.onDispatchCompleted({ dispatch_id: gateDispatch.id });
            } catch (e) {
              autoQueueLog("warn", "Phase gate sibling re-evaluation after autoclose threw: " + e, {
                run_id: gate.run_id,
                dispatch_id: gateDispatch.id,
                card_id: state.anchor_card_id || dispatch.kanban_card_id,
                batch_phase: phase
              });
            }
            return;
          }
        }
        state.status = "failed";
        state.failed_dispatch_id = gateDispatch.id;
        state.failed_verdict = gateVerdict;
        state.failed_reason = gateResult.summary || gateResult.reason || ("gate verdict mismatch for dispatch " + gateDispatch.id);
        savePhaseGateState(gate.run_id, phase, state);
        pauseRun(gate.run_id);
        _maybeAlertPhaseGateVerdictMismatch(
          gate.run_id, phase,
          state.anchor_card_id || dispatch.kanban_card_id,
          state.failed_reason
        );
        handlePhaseGateFailure(
          state.anchor_card_id || dispatch.kanban_card_id,
          phase,
          "[phase-gate] phase " + phase + " failed: " + state.failed_reason,
          {
            run_id: gate.run_id,
            dispatch_id: gateDispatch.id,
            card_id: state.anchor_card_id || dispatch.kanban_card_id,
            batch_phase: phase
          }
        );
        return;
      }
    }

    if (pendingCount > 0) {
      autoQueueLog("info", "Phase gate pass waiting for remaining dispatches: run " + gate.run_id + " phase " + phase + " pending=" + pendingCount, {
        run_id: gate.run_id,
        dispatch_id: dispatch.id,
        card_id: dispatch.kanban_card_id,
        batch_phase: phase
      });
      return;
    }

    clearPhaseGateState(gate.run_id, phase);
    resetPhaseGateFailureCount(state.anchor_card_id || dispatch.kanban_card_id, phase);
    if (state.final_phase || gate.final_phase) {
      completeRunAndNotify(gate.run_id);
      autoQueueLog("info", "Phase gate passed, completed run " + gate.run_id + " at phase " + phase, {
        run_id: gate.run_id,
        dispatch_id: dispatch.id,
        card_id: dispatch.kanban_card_id,
        batch_phase: phase
      });
      return;
    }

    resumeRunAndActivate(gate.run_id, gate.next_phase);
    autoQueueLog("info", "Phase gate passed, resumed run " + gate.run_id + " from phase " + phase + " to " + (gate.next_phase || "next"), {
      run_id: gate.run_id,
      dispatch_id: dispatch.id,
      card_id: dispatch.kanban_card_id,
      batch_phase: phase
    });
  },

  // ── Periodic recovery: dispatch next entry for idle agents (#110, #140, #179) ──
  // Group-aware: finds idle agents with pending entries across all groups.
  // Uses 1min tick instead of 5min for faster recovery.
  onTick1min: function() {
    var tickCfg = agentdesk.pipeline.getConfig();
    var tickKickoff = agentdesk.pipeline.kickoffState(tickCfg);
    var tickInProgress = agentdesk.pipeline.nextGatedTarget(tickKickoff, tickCfg);
    var tickReview = agentdesk.pipeline.nextGatedTarget(tickInProgress, tickCfg);
    var tickActiveStates = [tickKickoff, tickInProgress, tickReview].filter(function(s) { return s; });
    var tickPlaceholders = tickActiveStates.map(function() { return "?"; }).join(",");
    var tickTerminalStates = terminalStatesFromConfig(tickCfg);
    var tickTerminalPlaceholders = tickTerminalStates.map(function() { return "?"; }).join(",");

    // Recovery path 1 (#295): terminal cards should never remain pending in
    // active/paused runs. Clean them before dispatch recovery so they do not
    // get re-dispatched or block their groups.
    var terminalPending = agentdesk.db.query(
      "SELECT e.id, e.kanban_card_id, kc.status, e.run_id " +
      "FROM auto_queue_entries e " +
      "JOIN auto_queue_runs r ON e.run_id = r.id " +
      "JOIN kanban_cards kc ON kc.id = e.kanban_card_id " +
      "WHERE e.status = 'pending' AND r.status IN ('active', 'paused') " +
      "AND kc.status IN (" + tickTerminalPlaceholders + ") " +
      "ORDER BY e.updated_at ASC LIMIT 100",
      tickTerminalStates
    );
    for (var tp = 0; tp < terminalPending.length; tp++) {
      var pending = terminalPending[tp];
      if (!agentdesk.pipeline.isTerminal(pending.status, tickCfg)) continue;
      autoQueueLog("info", "onTick1min: skipping terminal pending entry " + pending.id + " for card " + pending.kanban_card_id + " at " + pending.status, {
        run_id: pending.run_id,
        entry_id: pending.id,
        card_id: pending.kanban_card_id
      });
      agentdesk.autoQueue.updateEntryStatus(
        pending.id,
        "skipped",
        "tick_terminal_cleanup"
      );
    }

    var finishedRuns = agentdesk.db.query(
      "SELECT r.id " +
      "FROM auto_queue_runs r " +
      "WHERE r.status IN ('active', 'paused') " +
      "AND NOT EXISTS (" +
      "  SELECT 1 FROM auto_queue_entries e " +
      "  WHERE e.run_id = r.id AND e.status IN ('pending', 'dispatched')" +
      ") " +
      "AND NOT EXISTS (" +
      "  SELECT 1 FROM auto_queue_entries e " +
      "  WHERE e.run_id = r.id AND e.status = 'user_cancelled'" +
      ") " +
      "AND NOT EXISTS (" +
      "  SELECT 1 FROM auto_queue_phase_gates pg " +
      "  WHERE pg.run_id = r.id AND pg.status IN ('pending', 'failed')" +
      ") " +
      "AND (" +
      "  r.phase_gate_grace_until IS NULL " +
      "  OR datetime(r.phase_gate_grace_until) IS NULL " +
      "  OR datetime(r.phase_gate_grace_until) <= datetime('now')" +
      ") ORDER BY r.id ASC LIMIT 50",
      []
    );
    for (var fr = 0; fr < finishedRuns.length; fr++) {
      finalizeRunWithoutPhaseGate(finishedRuns[fr].id);
    }

    // Find active runs with pending entries.
    // #815: `user_cancelled` entries are deliberately excluded here — they
    // represent an explicit operator stop and must never be resurrected by
    // the tick. Only `pending` entries are re-dispatchable.
    var activeRuns = agentdesk.db.query(
      "SELECT r.id " +
      "FROM auto_queue_runs r " +
      "JOIN auto_queue_entries e ON e.run_id = r.id " +
      "WHERE r.status = 'active' AND e.status = 'pending' " +
      "GROUP BY r.id " +
      "ORDER BY MIN(e.updated_at) ASC LIMIT 50",
      []
    );

    for (var ri = 0; ri < activeRuns.length; ri++) {
      var run = activeRuns[ri];
      var activation = activateRun(run.id, null);
      if (!activationWasDeferred(activation) && activationDispatchCount(activation) === 0) {
        rotateActiveRunSweepCursor(run.id);
      }
    }

    // Recovery path 2 (#179/#191/#214/#952): dispatched entries whose dispatch is stuck.
    // Threshold and stuck conditions are runtime-configurable so cron recovery can
    // be tuned without policy edits.
    var staleDispatchedGraceMinutes = configuredStaleDispatchedGraceMinutes();
    var staleDispatchedConditions = staleDispatchedRecoveryConditionsSql();
    var stuckDispatched = agentdesk.db.query(
      "SELECT e.id, e.agent_id, e.dispatch_id, e.kanban_card_id " +
      "FROM auto_queue_entries e " +
      "JOIN auto_queue_runs r ON e.run_id = r.id " +
      "WHERE e.status = 'dispatched' AND r.status = 'active' " +
      "AND e.dispatched_at IS NOT NULL " +
      "AND e.dispatched_at < datetime('now', '-" + staleDispatchedGraceMinutes + " minutes') " +
      "AND (" + staleDispatchedConditions + ") " +
      "ORDER BY e.dispatched_at ASC LIMIT 50",
      []
    );

    // #214: pendingIntents check REMOVED — it caused permanent recovery block when
    // intent drain failed (dispatch never created in DB but intent stayed in array
    // across ticks, skipping recovery forever). The 2-min grace period on
    // dispatched_at is sufficient to avoid false detection within the same tick.

    for (var j = 0; j < stuckDispatched.length; j++) {
      var stuck = stuckDispatched[j];
      var failure = agentdesk.autoQueue.recordDispatchFailure(
        stuck.id,
        configuredAutoQueueMaxEntryRetries(),
        "tick_recovery"
      );
      autoQueueLog("info", "onTick1min: recovered stuck dispatched entry " + stuck.id + " (dispatch " + (stuck.dispatch_id || "NULL") + " is orphan/cancelled/failed/phantom) retry " + failure.retryCount + "/" + failure.retryLimit + " -> " + failure.to, {
        entry_id: stuck.id,
        card_id: stuck.kanban_card_id,
        dispatch_id: stuck.dispatch_id
      });
      notifyAutoQueueEntryFailure(stuck, failure);
    }
  }
};

if (typeof agentdesk !== "undefined" && agentdesk && typeof agentdesk.registerPolicy === "function") {
  agentdesk.registerPolicy(autoQueue);
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = {
    policy: autoQueue,
    __test: {
      inferPhaseGatePassVerdict: _inferPhaseGatePassVerdict,
      dispatchableTargets: _dispatchableTargets,
      freePathToDispatchable: _freePathToDispatchable
    }
  };
}
