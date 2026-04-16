function _autoQueueHasValue(value) {
  return value !== null && value !== undefined && !(typeof value === "string" && value.trim() === "");
}

function _autoQueueLogContextKeys() {
  return ["run_id", "entry_id", "card_id", "dispatch_id", "thread_group", "batch_phase", "slot_index", "agent_id"];
}

function _mergeAutoQueueLogContext(target, source) {
  if (!source) return target;
  var keys = _autoQueueLogContextKeys();
  for (var i = 0; i < keys.length; i++) {
    var key = keys[i];
    if (!_autoQueueHasValue(target[key]) && _autoQueueHasValue(source[key])) {
      target[key] = source[key];
    }
  }
  return target;
}

function _loadAutoQueueEntryLogContext(entryId) {
  if (!_autoQueueHasValue(entryId)) return null;
  var rows = agentdesk.db.query(
    "SELECT run_id, id as entry_id, kanban_card_id as card_id, dispatch_id, agent_id, " +
    "COALESCE(thread_group, 0) as thread_group, COALESCE(batch_phase, 0) as batch_phase, slot_index " +
    "FROM auto_queue_entries WHERE id = ? LIMIT 1",
    [entryId]
  );
  return rows.length > 0 ? rows[0] : null;
}

function _loadAutoQueueDispatchLogContext(dispatchId) {
  if (!_autoQueueHasValue(dispatchId)) return null;
  var rows = agentdesk.db.query(
    "SELECT " +
    "COALESCE(e.run_id, " +
    "json_extract(COALESCE(td.context, '{}'), '$.run_id'), " +
    "json_extract(COALESCE(td.context, '{}'), '$.phase_gate.run_id')) as run_id, " +
    "COALESCE(e.id, json_extract(COALESCE(td.context, '{}'), '$.entry_id')) as entry_id, " +
    "COALESCE(e.kanban_card_id, td.kanban_card_id, json_extract(COALESCE(td.context, '{}'), '$.phase_gate.anchor_card_id')) as card_id, " +
    "td.id as dispatch_id, " +
    "COALESCE(e.thread_group, CAST(json_extract(COALESCE(td.context, '{}'), '$.thread_group') AS INTEGER)) as thread_group, " +
    "COALESCE(e.batch_phase, " +
    "CAST(json_extract(COALESCE(td.context, '{}'), '$.batch_phase') AS INTEGER), " +
    "CAST(json_extract(COALESCE(td.context, '{}'), '$.phase_gate.batch_phase') AS INTEGER)) as batch_phase, " +
    "COALESCE(e.slot_index, CAST(json_extract(COALESCE(td.context, '{}'), '$.slot_index') AS INTEGER)) as slot_index, " +
    "COALESCE(e.agent_id, json_extract(COALESCE(td.context, '{}'), '$.agent_id'), " +
    "json_extract(COALESCE(td.context, '{}'), '$.target_agent_id'), " +
    "json_extract(COALESCE(td.context, '{}'), '$.source_agent_id')) as agent_id " +
    "FROM task_dispatches td " +
    "LEFT JOIN auto_queue_entries e ON e.dispatch_id = td.id " +
    "WHERE td.id = ? LIMIT 1",
    [dispatchId]
  );
  return rows.length > 0 ? rows[0] : null;
}

function _normalizeAutoQueueLogContext(context) {
  var merged = {};
  var hydratedEntryId = null;
  _mergeAutoQueueLogContext(merged, context || {});
  if (_autoQueueHasValue(merged.entry_id)) {
    hydratedEntryId = merged.entry_id;
    _mergeAutoQueueLogContext(merged, _loadAutoQueueEntryLogContext(merged.entry_id));
  }
  if (_autoQueueHasValue(merged.dispatch_id)) {
    _mergeAutoQueueLogContext(merged, _loadAutoQueueDispatchLogContext(merged.dispatch_id));
  }
  if (_autoQueueHasValue(merged.entry_id) && merged.entry_id !== hydratedEntryId) {
    _mergeAutoQueueLogContext(merged, _loadAutoQueueEntryLogContext(merged.entry_id));
  }
  return merged;
}

function _formatAutoQueueLogContext(context) {
  var orderedKeys = _autoQueueLogContextKeys();
  var parts = [];
  for (var i = 0; i < orderedKeys.length; i++) {
    var key = orderedKeys[i];
    if (_autoQueueHasValue(context[key])) {
      parts.push(key + "=" + context[key]);
    }
  }
  return parts.length > 0 ? " | " + parts.join(" ") : "";
}

function autoQueueLog(level, message, context) {
  if (!agentdesk.log || typeof agentdesk.log[level] !== "function") return;
  var merged = _normalizeAutoQueueLogContext(context || {});
  agentdesk.log[level]("[auto-queue] " + message + _formatAutoQueueLogContext(merged));
}

var PHASE_GATE_HUMAN_ESCALATION_THRESHOLD = 3;
var PHASE_GATE_FAILURE_TTL_SEC = 7 * 24 * 60 * 60;
var PHASE_GATE_REWORK_CARD_TITLE_MAX_CHARS = 120;
var PHASE_GATE_REWORK_DETAIL_MAX_CHARS = 72;

function _autoQueueParseJsonObject(raw) {
  if (!raw) return {};
  try {
    return JSON.parse(raw) || {};
  } catch (e) {
    return {};
  }
}

function _autoQueueTruncateText(text, maxChars) {
  var normalized = String(text || "");
  if (maxChars <= 0) return "";
  if (normalized.length <= maxChars) return normalized;
  if (maxChars === 1) return "…";
  return normalized.substring(0, maxChars - 1) + "…";
}

function phaseGateMaxReworkRetries() {
  var configured = parseInt(agentdesk.config.get("phase_gate_max_rework_retries") || "3", 10);
  if (isNaN(configured) || configured < 0) return 3;
  return configured;
}

function phaseGateFailureKey(cardId, phase) {
  return "phase_gate_failure:" + cardId + ":" + phase;
}

function incrementPhaseGateFailureCount(cardId, phase) {
  if (!cardId) return 0;
  var key = phaseGateFailureKey(cardId, phase);
  var current = parseInt(agentdesk.kv.get(key) || "0", 10);
  if (!current || current < 0) current = 0;
  var next = current + 1;
  agentdesk.kv.set(key, String(next), PHASE_GATE_FAILURE_TTL_SEC);
  return next;
}

function resetPhaseGateFailureCount(cardId, phase) {
  if (!cardId) return;
  agentdesk.kv.delete(phaseGateFailureKey(cardId, phase));
}

function loadPhaseGateCardLabel(cardId) {
  if (!cardId) return "unknown card";
  var rows = agentdesk.db.query(
    "SELECT COALESCE(title, id) as title, github_issue_number FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (rows.length === 0) return cardId;
  if (rows[0].github_issue_number) {
    return "#" + rows[0].github_issue_number + " " + rows[0].title;
  }
  return rows[0].title || cardId;
}

function handlePhaseGateFailure(cardId, phase, reason, context) {
  var failureCount = incrementPhaseGateFailureCount(cardId, phase);
  notifyCardOwner(cardId, reason, "auto-queue");
  if (failureCount >= PHASE_GATE_HUMAN_ESCALATION_THRESHOLD) {
    notifyHumanAlert(
      "⚠️ [Phase Gate] " + loadPhaseGateCardLabel(cardId) + "\n" +
        "phase " + phase + " 실패가 " + failureCount + "회 누적되었습니다.\n" +
        reason + "\n" +
        "사람 확인이 필요합니다.",
      "auto-queue"
    );
  }
  autoQueueLog("warn", "Phase gate failure recorded for card " + (cardId || "unknown") + " phase " + phase + " count=" + failureCount, context);
  return failureCount;
}

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
  // before firing OnCardTerminal, so we don't re-mark here.
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
    if (!gate || !gate.run_id || !gate.batch_phase) {
      return;
    }

    var phase = gate.batch_phase || 0;
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

    if (verdict !== passVerdict) {
      state.status = "failed";
      state.failed_dispatch_id = dispatch.id;
      state.failed_verdict = verdict;
      state.failed_reason = result.summary || result.reason || ("expected " + passVerdict + ", got " + (verdict || "none"));
      savePhaseGateState(gate.run_id, phase, state);
      pauseRun(gate.run_id);
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
      if (gateDispatch.status !== "completed" || gateVerdict !== expectedVerdict) {
        state.status = "failed";
        state.failed_dispatch_id = gateDispatch.id;
        state.failed_verdict = gateVerdict;
        state.failed_reason = gateResult.summary || gateResult.reason || ("gate verdict mismatch for dispatch " + gateDispatch.id);
        savePhaseGateState(gate.run_id, phase, state);
        pauseRun(gate.run_id);
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

    // Recovery path 1 (#295): terminal cards should never remain pending in
    // active/paused runs. Clean them before dispatch recovery so they do not
    // get re-dispatched or block their groups.
    var terminalPending = agentdesk.db.query(
      "SELECT e.id, e.kanban_card_id, kc.status, e.run_id " +
      "FROM auto_queue_entries e " +
      "JOIN auto_queue_runs r ON e.run_id = r.id " +
      "JOIN kanban_cards kc ON kc.id = e.kanban_card_id " +
      "WHERE e.status = 'pending' AND r.status IN ('active', 'paused')",
      []
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
      ")",
      []
    );
    for (var fr = 0; fr < finishedRuns.length; fr++) {
      finalizeRunWithoutPhaseGate(finishedRuns[fr].id);
    }

    var pausedPhaseGates = agentdesk.db.query(
      "SELECT DISTINCT g.run_id, g.phase " +
      "FROM auto_queue_phase_gates g " +
      "JOIN auto_queue_runs r ON r.id = g.run_id " +
      "WHERE r.status = 'paused' AND g.status IN ('pending', 'failed') " +
      "ORDER BY datetime(COALESCE(g.updated_at, g.created_at)) ASC, g.rowid ASC",
      []
    );
    for (var pg = 0; pg < pausedPhaseGates.length; pg++) {
      var gateRow = pausedPhaseGates[pg];
      try {
        reevaluateTickPhaseGate(gateRow.run_id, gateRow.phase);
      } catch (e) {
        autoQueueLog("warn", "onTick1min: phase gate reevaluation failed for run " + gateRow.run_id + " phase " + gateRow.phase + ": " + e, {
          run_id: gateRow.run_id,
          batch_phase: gateRow.phase
        });
      }
    }

    // Find active runs with pending entries
    var activeRuns = agentdesk.db.query(
      "SELECT DISTINCT r.id " +
      "FROM auto_queue_runs r " +
      "JOIN auto_queue_entries e ON e.run_id = r.id " +
      "WHERE r.status = 'active' AND e.status = 'pending'",
      []
    );

    for (var ri = 0; ri < activeRuns.length; ri++) {
      var run = activeRuns[ri];
      activateRun(run.id, null);
    }

    // Recovery path 2 (#179/#191/#214): dispatched entries whose dispatch is stuck
    // Covers: cancelled/failed dispatch, phantom dispatch_id (row missing),
    // AND orphan entries (dispatched status but dispatch_id is NULL)
    // #214: Grace period — only check entries dispatched >2 min ago to avoid
    // false orphan detection when dispatch intent hasn't drained yet
    var stuckDispatched = agentdesk.db.query(
      "SELECT e.id, e.agent_id, e.dispatch_id, e.kanban_card_id " +
      "FROM auto_queue_entries e " +
      "JOIN auto_queue_runs r ON e.run_id = r.id " +
      "WHERE e.status = 'dispatched' AND r.status = 'active' " +
      "AND e.dispatched_at IS NOT NULL AND e.dispatched_at < datetime('now', '-2 minutes') " +
      "AND (" +
      "  e.dispatch_id IS NULL" +
      "  OR EXISTS (" +
      "    SELECT 1 FROM task_dispatches td " +
      "    WHERE td.id = e.dispatch_id " +
      "    AND td.status IN ('cancelled', 'failed')" +
      "  )" +
      "  OR NOT EXISTS (" +
      "    SELECT 1 FROM task_dispatches td WHERE td.id = e.dispatch_id" +
      "  )" +
      ")",
      []
    );

    // #214: pendingIntents check REMOVED — it caused permanent recovery block when
    // intent drain failed (dispatch never created in DB but intent stayed in array
    // across ticks, skipping recovery forever). The 2-min grace period on
    // dispatched_at is sufficient to avoid false detection within the same tick.

    for (var j = 0; j < stuckDispatched.length; j++) {
      var stuck = stuckDispatched[j];
      autoQueueLog("info", "onTick1min: resetting stuck dispatched entry " + stuck.id + " (dispatch " + (stuck.dispatch_id || "NULL") + " is orphan/cancelled/failed/phantom)", {
        entry_id: stuck.id,
        card_id: stuck.kanban_card_id,
        dispatch_id: stuck.dispatch_id
      });
      agentdesk.autoQueue.updateEntryStatus(
        stuck.id,
        "pending",
        "tick_recovery"
      );
    }
  }
};

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

function loadPhaseGateState(runId, phase) {
  var rows = agentdesk.db.query(
    "SELECT dispatch_id, status, verdict, pass_verdict, next_phase, final_phase, " +
    "rework_count, anchor_card_id, failure_reason, created_at " +
    "FROM auto_queue_phase_gates " +
    "WHERE run_id = ? AND phase = ? " +
    "ORDER BY CASE WHEN dispatch_id IS NULL THEN 0 ELSE 1 END ASC, created_at ASC, dispatch_id ASC",
    [runId, phase]
  );
  if (rows.length === 0) return null;

  var dispatchIds = [];
  var status = "pending";
  var failedRow = null;
  var hasPassedRows = false;

  for (var i = 0; i < rows.length; i++) {
    var row = rows[i];
    if (row.dispatch_id) {
      dispatchIds.push(row.dispatch_id);
    }
    if (row.status === "failed" && !failedRow) {
      failedRow = row;
      status = "failed";
    } else if (row.status === "passed") {
      hasPassedRows = true;
    }
  }

  if (!failedRow && dispatchIds.length > 0 && hasPassedRows && rows.every(function(row) { return row.status === "passed"; })) {
    status = "passed";
  } else if (!failedRow) {
    status = rows[0].status || "pending";
  }

  var state = {
    run_id: runId,
    batch_phase: phase,
    next_phase: rows[0].next_phase,
    final_phase: !!rows[0].final_phase,
    rework_count: rows[0].rework_count || 0,
    anchor_card_id: rows[0].anchor_card_id || null,
    status: status,
    dispatch_ids: dispatchIds,
    gates: [],
    created_at: rows[0].created_at || null
  };

  if (failedRow) {
    state.failed_dispatch_id = failedRow.dispatch_id || null;
    state.failed_verdict = failedRow.verdict || null;
    state.failed_reason = failedRow.failure_reason || null;
  }

  return state;
}

function savePhaseGateState(runId, phase, state) {
  if (!state) return;
  agentdesk.autoQueue.savePhaseGateState(runId, phase, {
    status: state.status || "pending",
    verdict: state.failed_verdict || state.verdict || null,
    dispatch_ids: Array.isArray(state.dispatch_ids) ? state.dispatch_ids : [],
    pass_verdict: state.pass_verdict ||
      (state.gates && state.gates[0] && state.gates[0].pass_verdict) ||
      "phase_gate_passed",
    next_phase: state.next_phase !== undefined ? state.next_phase : null,
    final_phase: !!state.final_phase,
    rework_count: state.rework_count || 0,
    anchor_card_id: state.anchor_card_id || null,
    failure_reason: state.failed_reason || null,
    created_at: state.created_at || null
  });
}

function clearPhaseGateState(runId, phase) {
  agentdesk.autoQueue.clearPhaseGateState(runId, phase);
}

function loadRunInfo(runId) {
  var rows = agentdesk.db.query(
    "SELECT status, repo, unified_thread_id, unified_thread_channel_id, COALESCE(thread_group_count, 1) as group_count " +
    "FROM auto_queue_runs WHERE id = ?",
    [runId]
  );
  return rows.length > 0 ? rows[0] : null;
}

function runHasBlockingPhaseGate(runId) {
  var rows = agentdesk.db.query(
    "SELECT COUNT(*) as cnt FROM auto_queue_phase_gates " +
    "WHERE run_id = ? AND status IN ('pending', 'failed')",
    [runId]
  );
  return rows.length > 0 && rows[0].cnt > 0;
}

function finalizeRunWithoutPhaseGate(runId) {
  if (!runId) return false;

  if (runHasBlockingPhaseGate(runId)) return false;
  if (remainingRunnableEntryCount(runId) > 0) return false;

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

function pauseRun(runId, source) {
  if (!runId) return false;
  try {
    var result = agentdesk.autoQueue.pauseRun(runId, source || "policy_pause");
    return !!(result && result.changed);
  } catch (e) {
    autoQueueLog("warn", "Failed to pause run " + runId + ": " + e, {
      run_id: runId
    });
    return false;
  }
}

function loadPhaseGateDispatches(dispatchIds) {
  if (!dispatchIds || dispatchIds.length === 0) return [];
  var placeholders = dispatchIds.map(function() { return "?"; }).join(",");
  return agentdesk.db.query(
    "SELECT id, status, result, context FROM task_dispatches WHERE id IN (" + placeholders + ")",
    dispatchIds
  );
}

function countDistinctBatchPhases(runId) {
  var rows = agentdesk.db.query(
    "SELECT COUNT(DISTINCT COALESCE(batch_phase, 0)) as cnt " +
    "FROM auto_queue_entries WHERE run_id = ?",
    [runId]
  );
  return (rows.length > 0) ? (rows[0].cnt || 0) : 0;
}

function _isDeployPhase(runId, phase) {
  var rows = agentdesk.db.query(
    "SELECT deploy_phases FROM auto_queue_runs WHERE id = ?",
    [runId]
  );
  if (rows.length === 0 || !rows[0].deploy_phases) return false;
  try {
    var phases = JSON.parse(rows[0].deploy_phases);
    return Array.isArray(phases) && phases.indexOf(phase) >= 0;
  } catch (e) {
    return false;
  }
}

function _phaseGateRequired(runId, phase) {
  if (_isDeployPhase(runId, phase)) return true;
  return countDistinctBatchPhases(runId) > 1;
}

function _loadPhaseGateCards(runId, phase) {
  return agentdesk.db.query(
    "SELECT e.id as entry_id, e.kanban_card_id as card_id, e.agent_id, " +
    "COALESCE(kc.title, kc.id) as title, kc.github_issue_number, kc.repo_id, kc.status, kc.blocked_reason " +
    "FROM auto_queue_entries e " +
    "JOIN kanban_cards kc ON kc.id = e.kanban_card_id " +
    "WHERE e.run_id = ? AND COALESCE(e.batch_phase, 0) = ? " +
    "ORDER BY e.priority_rank ASC, e.created_at ASC, e.id ASC",
    [runId, phase]
  );
}

function _loadPhaseGateActiveFixDispatch(cardId) {
  var rows = agentdesk.db.query(
    "SELECT id, dispatch_type, to_agent_id, status, context " +
    "FROM task_dispatches " +
    "WHERE kanban_card_id = ? " +
    "AND dispatch_type IN ('implementation', 'rework') " +
    "AND status IN ('pending', 'dispatched') " +
    "ORDER BY CASE WHEN dispatch_type = 'rework' THEN 0 ELSE 1 END ASC, " +
    "datetime(COALESCE(updated_at, created_at)) DESC, rowid DESC LIMIT 1",
    [cardId]
  );
  if (rows.length === 0) return null;
  rows[0].context_json = _autoQueueParseJsonObject(rows[0].context);
  return rows[0];
}

function _resolvePhaseGatePrInfo(card) {
  if (!card || !card.card_id) return null;
  var tracking = agentdesk.prTracking.load(card.card_id, { fallback_state: "wait-ci" }) || null;
  var repo = (tracking && tracking.repo_id) || card.repo_id || agentdesk.prTracking.repoForCard(card.card_id);
  if (!repo) return null;

  var prNumber = tracking && tracking.pr_number ? tracking.pr_number : null;
  var branch = tracking && tracking.branch ? tracking.branch : null;
  var headSha = tracking && tracking.head_sha ? tracking.head_sha : null;
  if (!prNumber && branch) {
    var discovered = agentdesk.prTracking.findOpenPrByBranch(repo, branch);
    if (discovered) {
      prNumber = discovered.number || null;
      branch = discovered.branch || branch;
      headSha = discovered.sha || headSha;
      agentdesk.prTracking.upsert(
        card.card_id,
        repo,
        tracking ? tracking.worktree_path : null,
        branch,
        prNumber,
        headSha,
        (tracking && tracking.state) || "wait-ci",
        (tracking && tracking.last_error) || null
      );
      tracking = agentdesk.prTracking.load(card.card_id, { fallback_state: "wait-ci" }) || tracking;
    }
  }

  if (!prNumber) return null;
  return {
    card_id: card.card_id,
    repo: repo,
    number: prNumber,
    branch: branch,
    sha: headSha,
    worktree_path: tracking && tracking.worktree_path ? tracking.worktree_path : null,
    tracking_state: tracking && tracking.state ? tracking.state : null
  };
}

function _loadPhaseGatePrView(prInfo) {
  if (!prInfo || !prInfo.repo || !prInfo.number) return null;
  var raw = agentdesk.exec("gh", [
    "pr", "view", String(prInfo.number),
    "--json", "number,state,mergedAt,mergeable,headRefName,headRefOid,title",
    "--repo", prInfo.repo
  ]);
  if (!raw || raw.indexOf("ERROR") === 0) return null;
  try {
    return JSON.parse(raw);
  } catch (e) {
    return null;
  }
}

function _loadPhaseGateIssueClosed(issueNumber, repo) {
  if (!issueNumber) return true;
  if (!repo) return null;
  var raw = agentdesk.exec("gh", [
    "issue", "view", String(issueNumber),
    "--repo", repo,
    "--json", "state",
    "--jq", ".state"
  ]);
  if (!raw || raw.indexOf("ERROR") === 0) return null;
  return String(raw || "").trim().toUpperCase() === "CLOSED";
}

function _loadPhaseGateCiState(prInfo, prView) {
  if (!prInfo || !prInfo.repo) {
    return { status: "pending", reason: "missing PR repo" };
  }
  var branch = (prView && prView.headRefName) || prInfo.branch;
  var headSha = (prView && prView.headRefOid) || prInfo.sha;
  if (!branch) {
    return { status: "pending", reason: "missing PR branch" };
  }

  var raw = agentdesk.exec("gh", [
    "run", "list",
    "--branch", branch,
    "--repo", prInfo.repo,
    "--json", "databaseId,status,conclusion,headSha,event",
    "--limit", "10"
  ]);
  if (!raw || raw.indexOf("ERROR") === 0) {
    return { status: "pending", reason: "CI lookup failed" };
  }

  var runs = [];
  try {
    runs = JSON.parse(raw) || [];
  } catch (e) {
    return { status: "pending", reason: "CI payload parse failed" };
  }
  if (!runs || runs.length === 0) {
    return { status: "pending", reason: "CI run not found" };
  }

  var selected = runs[0];
  if (headSha) {
    for (var i = 0; i < runs.length; i++) {
      if (runs[i].headSha === headSha) {
        selected = runs[i];
        break;
      }
    }
  }

  var runId = selected.databaseId || null;
  var runUrl = runId ? ("https://github.com/" + prInfo.repo + "/actions/runs/" + runId) : null;
  if (selected.status !== "completed") {
    return {
      status: "pending",
      reason: "CI running",
      run_id: runId,
      run_url: runUrl
    };
  }
  if (selected.conclusion === "success") {
    return {
      status: "success",
      run_id: runId,
      run_url: runUrl
    };
  }
  if (selected.conclusion === "failure") {
    return {
      status: "failure",
      reason: "CI failure confirmed",
      conclusion: selected.conclusion,
      run_id: runId,
      run_url: runUrl
    };
  }
  return {
    status: "pending",
    reason: "CI conclusion=" + (selected.conclusion || "unknown"),
    conclusion: selected.conclusion || null,
    run_id: runId,
    run_url: runUrl
  };
}

function _evaluateTickPhaseGateCard(card) {
  var evaluation = {
    card_id: card.card_id,
    title: card.title || card.card_id,
    agent_id: card.agent_id || null,
    github_issue_number: card.github_issue_number || null,
    repo_id: card.repo_id || null,
    status: card.status || null,
    blocked_reason: card.blocked_reason || null,
    merge_verified: false,
    build_passed: false,
    issue_closed: false,
    waiting_reason: null,
    failure_type: null,
    failure_message: null,
    active_fix_dispatch: null,
    pr: null,
    pr_view: null,
    ci: null
  };

  var prInfo = _resolvePhaseGatePrInfo(card);
  if (!prInfo) {
    evaluation.waiting_reason = "PR tracking pending";
    return evaluation;
  }
  evaluation.pr = prInfo;

  if (prInfo.tracking_state === "post-merge-cleanup" || prInfo.tracking_state === "closed") {
    evaluation.merge_verified = true;
    evaluation.build_passed = true;
  }

  var prView = _loadPhaseGatePrView(prInfo);
  evaluation.pr_view = prView;

  if (prView && prView.mergedAt) {
    evaluation.merge_verified = true;
    evaluation.build_passed = true;
  }

  if (!evaluation.merge_verified && prView && prView.mergeable === "CONFLICTING") {
    evaluation.failure_type = "merge_conflict";
    evaluation.failure_message = "PR #" + prInfo.number + " has merge conflicts with main";
    evaluation.active_fix_dispatch = _loadPhaseGateActiveFixDispatch(card.card_id);
    return evaluation;
  }

  if (!evaluation.build_passed) {
    evaluation.ci = _loadPhaseGateCiState(prInfo, prView);
    if (evaluation.ci.status === "success") {
      evaluation.build_passed = true;
    } else if (evaluation.ci.status === "failure") {
      evaluation.failure_type = "ci_failure";
      evaluation.failure_message = "PR #" + prInfo.number + " CI failed";
      evaluation.active_fix_dispatch = _loadPhaseGateActiveFixDispatch(card.card_id);
      return evaluation;
    } else {
      evaluation.waiting_reason = evaluation.ci.reason || "CI pending";
    }
  }

  if (!evaluation.merge_verified) {
    evaluation.waiting_reason = evaluation.waiting_reason || "merge pending";
  }

  var issueClosed = _loadPhaseGateIssueClosed(card.github_issue_number, prInfo.repo || card.repo_id);
  if (issueClosed === true) {
    evaluation.issue_closed = true;
  } else if (!card.github_issue_number) {
    evaluation.issue_closed = true;
  } else if (issueClosed === false) {
    evaluation.waiting_reason = evaluation.waiting_reason || ("issue #" + card.github_issue_number + " still open");
  } else {
    evaluation.waiting_reason = evaluation.waiting_reason || ("issue #" + card.github_issue_number + " lookup pending");
  }

  return evaluation;
}

function _phaseGateReworkDetailLabel(evaluation) {
  if (!evaluation || !evaluation.failure_type) return "재작업";
  if (evaluation.failure_type === "merge_conflict") return "충돌 해결";
  return "CI 실패 수정";
}

function _phaseGateReworkTitle(evaluation) {
  var issueLabel = evaluation.github_issue_number ? ("#" + evaluation.github_issue_number + " ") : "";
  var compactTitle = _autoQueueTruncateText(evaluation.title || evaluation.card_id, PHASE_GATE_REWORK_CARD_TITLE_MAX_CHARS);
  var compactDetail = _autoQueueTruncateText(_phaseGateReworkDetailLabel(evaluation), PHASE_GATE_REWORK_DETAIL_MAX_CHARS);
  return "[Phase Gate Rework] " + issueLabel + compactTitle + " — " + compactDetail;
}

function _phaseGateReworkContext(runId, phase, evaluation) {
  return {
    auto_queue: true,
    sidecar_dispatch: true,
    target_repo: evaluation.pr ? evaluation.pr.repo : null,
    worktree_path: evaluation.pr ? evaluation.pr.worktree_path : null,
    worktree_branch: evaluation.pr ? evaluation.pr.branch : null,
    phase_gate_rework: {
      run_id: runId,
      batch_phase: phase,
      card_id: evaluation.card_id,
      failure_type: evaluation.failure_type,
      failure_reason: evaluation.failure_message,
      repo: evaluation.pr ? evaluation.pr.repo : null,
      pr_number: evaluation.pr ? evaluation.pr.number : null,
      run_id_ci: evaluation.ci && evaluation.ci.run_id ? evaluation.ci.run_id : null,
      run_url: evaluation.ci && evaluation.ci.run_url ? evaluation.ci.run_url : null
    }
  };
}

function _movePhaseGateCardToRework(evaluation) {
  var cfg = agentdesk.pipeline.resolveForCard(evaluation.card_id);
  var card = agentdesk.kanban.getCard(evaluation.card_id);
  var initialState = agentdesk.pipeline.kickoffState(cfg);
  var inProgressState = agentdesk.pipeline.nextGatedTarget(initialState, cfg);
  var reviewState = agentdesk.pipeline.nextGatedTarget(inProgressState, cfg);
  var reworkTarget = agentdesk.pipeline.nextGatedTargetWithGate(reviewState, "review_rework", cfg) || inProgressState;
  if (reworkTarget) {
    if (card && agentdesk.pipeline.isTerminal(card.status, cfg)) {
      agentdesk.kanban.reopen(evaluation.card_id, reworkTarget);
    } else {
      agentdesk.kanban.setStatus(evaluation.card_id, reworkTarget);
    }
  }
  if (evaluation.failure_type === "ci_failure") {
    agentdesk.db.execute(
      "UPDATE kanban_cards SET blocked_reason = 'ci:rework' WHERE id = ?",
      [evaluation.card_id]
    );
  } else {
    agentdesk.db.execute(
      "UPDATE kanban_cards SET blocked_reason = NULL WHERE id = ?",
      [evaluation.card_id]
    );
  }
}

function _saveFinalFailedPhaseGate(runId, phase, state, cardId, reason, context) {
  state.status = "failed";
  state.failed_verdict = "phase_gate_final_failure";
  state.failed_reason = reason;
  savePhaseGateState(runId, phase, state);
  pauseRun(runId);
  notifyCardOwner(cardId, reason, "auto-queue");
  notifyHumanAlert(
    "⚠️ [Phase Gate] " + loadPhaseGateCardLabel(cardId) + "\n" + reason,
    "auto-queue"
  );
  autoQueueLog("warn", "Phase gate finalized as failed: " + reason, context);
}

function _handleConfirmedTickPhaseGateFailure(runId, phase, state, evaluation) {
  var activeFix = evaluation.active_fix_dispatch;
  if (activeFix) {
    if ((state.rework_count || 0) === 0 && activeFix.dispatch_type === "rework") {
      state.rework_count = 1;
      savePhaseGateState(runId, phase, state);
    }
    autoQueueLog("info", "Phase gate waiting for active fix dispatch " + activeFix.id + " on card " + evaluation.card_id, {
      run_id: runId,
      card_id: evaluation.card_id,
      dispatch_id: activeFix.id,
      batch_phase: phase
    });
    return;
  }

  var limit = phaseGateMaxReworkRetries();
  if ((state.rework_count || 0) >= limit) {
    _saveFinalFailedPhaseGate(
      runId,
      phase,
      state,
      evaluation.card_id,
      "phase " + phase + " 자동 재작업 한도(" + limit + ")를 초과했습니다. " + evaluation.failure_message,
      {
        run_id: runId,
        card_id: evaluation.card_id,
        batch_phase: phase
      }
    );
    return;
  }

  var cardRows = agentdesk.db.query(
    "SELECT assigned_agent_id FROM kanban_cards WHERE id = ? LIMIT 1",
    [evaluation.card_id]
  );
  if (cardRows.length === 0 || !cardRows[0].assigned_agent_id) {
    _saveFinalFailedPhaseGate(
      runId,
      phase,
      state,
      evaluation.card_id,
      "phase " + phase + " 재작업을 보낼 담당 에이전트가 없습니다. " + evaluation.failure_message,
      {
        run_id: runId,
        card_id: evaluation.card_id,
        batch_phase: phase
      }
    );
    return;
  }

  try {
    var dispatchId = agentdesk.dispatch.create(
      evaluation.card_id,
      cardRows[0].assigned_agent_id,
      "rework",
      _phaseGateReworkTitle(evaluation),
      _phaseGateReworkContext(runId, phase, evaluation)
    );
    _movePhaseGateCardToRework(evaluation);
    state.rework_count = (state.rework_count || 0) + 1;
    savePhaseGateState(runId, phase, state);
    autoQueueLog("info", "Created phase gate rework dispatch " + dispatchId + " for card " + evaluation.card_id, {
      run_id: runId,
      card_id: evaluation.card_id,
      dispatch_id: dispatchId,
      batch_phase: phase
    });
  } catch (e) {
    _saveFinalFailedPhaseGate(
      runId,
      phase,
      state,
      evaluation.card_id,
      "phase " + phase + " 재작업 dispatch 생성에 실패했습니다: " + e,
      {
        run_id: runId,
        card_id: evaluation.card_id,
        batch_phase: phase
      }
    );
  }
}

function reevaluateTickPhaseGate(runId, phase) {
  var state = loadPhaseGateState(runId, phase);
  if (!state) return false;
  if (_isDeployPhase(runId, phase)) return false;
  if (state.dispatch_ids && state.dispatch_ids.length > 0) return false;
  if (state.status === "failed" && state.failed_verdict === "phase_gate_final_failure") {
    return false;
  }

  var cards = _loadPhaseGateCards(runId, phase);
  if (cards.length === 0) {
    _saveFinalFailedPhaseGate(
      runId,
      phase,
      state,
      state.anchor_card_id || null,
      "phase " + phase + " gate target cards could not be found",
      {
        run_id: runId,
        card_id: state.anchor_card_id || null,
        batch_phase: phase
      }
    );
    return true;
  }

  var allPassed = true;
  var firstFailure = null;
  for (var i = 0; i < cards.length; i++) {
    var evaluation = _evaluateTickPhaseGateCard(cards[i]);
    if (evaluation.failure_type && !firstFailure) {
      firstFailure = evaluation;
      allPassed = false;
      break;
    }
    if (!(evaluation.merge_verified && evaluation.build_passed && evaluation.issue_closed)) {
      allPassed = false;
    }
  }

  if (allPassed) {
    clearPhaseGateState(runId, phase);
    resetPhaseGateFailureCount(state.anchor_card_id || cards[0].card_id, phase);
    if (state.final_phase) {
      completeRunAndNotify(runId);
    } else {
      resumeRunAndActivate(runId, state.next_phase);
    }
    autoQueueLog("info", "Tick phase gate passed for run " + runId + " phase " + phase, {
      run_id: runId,
      card_id: state.anchor_card_id || cards[0].card_id,
      batch_phase: phase
    });
    return true;
  }

  state.status = "pending";
  state.failed_verdict = null;
  state.failed_reason = null;
  savePhaseGateState(runId, phase, state);

  if (firstFailure) {
    _handleConfirmedTickPhaseGateFailure(runId, phase, state, firstFailure);
    return true;
  }

  return false;
}

function completeRunAndNotify(runId) {
  if (!runId) return;
  try {
    agentdesk.autoQueue.resumeRun(runId, "phase_gate_complete_resume");
  } catch (e) {
    autoQueueLog("warn", "Failed to resume final phase-gate run " + runId + ": " + e, {
      run_id: runId
    });
  }
  activateRun(runId, null);
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

function _deployGateTitle(phase) {
  return "[Deploy Gate] Phase " + phase + " 빌드+배포";
}

function continueRunAfterEntry(runId, agentId, doneGroup, donePhase, anchorCardId) {
  if (!runId || !agentId) return;

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
        activateRun(runId, null);
        return;
      }
    }
  }

  if (remainingCount === 0) {
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

function _buildPhaseGateGroups(runId, phase) {
  var rows = agentdesk.db.query(
    "SELECT e.id as entry_id, e.kanban_card_id, e.agent_id, e.status, e.priority_rank, " +
    "kc.title, kc.github_issue_number, kc.repo_id, " +
    "(SELECT td.result FROM task_dispatches td " +
    " WHERE td.kanban_card_id = e.kanban_card_id " +
    "   AND td.status = 'completed' " +
    "   AND td.result IS NOT NULL " +
    " ORDER BY td.completed_at DESC, td.rowid DESC LIMIT 1) as latest_result " +
    "FROM auto_queue_entries e " +
    "JOIN kanban_cards kc ON kc.id = e.kanban_card_id " +
    "WHERE e.run_id = ? AND COALESCE(e.batch_phase, 0) = ? " +
    "ORDER BY e.agent_id ASC, e.priority_rank ASC",
    [runId, phase]
  );
  var groups = {};
  var ordered = [];

  for (var i = 0; i < rows.length; i++) {
    var row = rows[i];
    var gate = agentdesk.pipeline.resolvePhaseGateForCard(row.kanban_card_id);
    var targetAgentId = gate.dispatch_to === "self" ? row.agent_id : gate.dispatch_to;
    var checks = Array.isArray(gate.checks) ? gate.checks.slice() : [];
    var key = [
      row.agent_id || "",
      targetAgentId || "",
      gate.dispatch_type || "phase-gate",
      gate.pass_verdict || "phase_gate_passed",
      checks.join("|")
    ].join("::");
    if (!groups[key]) {
      groups[key] = {
        source_agent_id: row.agent_id,
        target_agent_id: targetAgentId,
        dispatch_type: gate.dispatch_type || "phase-gate",
        pass_verdict: gate.pass_verdict || "phase_gate_passed",
        checks: checks,
        anchor_card_id: row.kanban_card_id,
        repo_id: row.repo_id || null,
        card_ids: [],
        issue_numbers: [],
        work_items: []
      };
      ordered.push(groups[key]);
    }

    var latestResult = {};
    try { latestResult = JSON.parse(row.latest_result || "{}"); } catch (e) { latestResult = {}; }

    groups[key].card_ids.push(row.kanban_card_id);
    if (row.github_issue_number !== null && row.github_issue_number !== undefined) {
      groups[key].issue_numbers.push(row.github_issue_number);
    }
    groups[key].work_items.push({
      entry_id: row.entry_id,
      card_id: row.kanban_card_id,
      agent_id: row.agent_id,
      status: row.status,
      title: row.title || row.kanban_card_id,
      issue_number: row.github_issue_number,
      completed_branch: latestResult.completed_branch || null,
      completed_worktree_path: latestResult.completed_worktree_path || null
    });
  }

  return ordered;
}

function _phaseGateTitle(group, phase, runId) {
  var issues = group.issue_numbers.filter(function(issue) {
    return issue !== null && issue !== undefined;
  });
  var issueLabel = issues.slice(0, 3).map(function(issue) {
    return "#" + issue;
  }).join(", ");
  if (issues.length > 3) {
    issueLabel += " +" + (issues.length - 3);
  }
  if (!issueLabel) {
    issueLabel = "run " + runId.substring(0, 8);
  }
  return "[" + group.dispatch_type + " P" + phase + "] " + issueLabel;
}

function _createDeployGateDispatch(runId, phase, nextPhase, finalPhase, anchorCardId) {
  var livePhaseCount = remainingRunnableEntryCount(runId, phase);
  if (livePhaseCount > 0) {
    autoQueueLog("info", "Skipping deploy gate for phase " + phase + " — " + livePhaseCount + " live entries remain", {
      run_id: runId,
      card_id: anchorCardId,
      batch_phase: phase
    });
    return null;
  }

  pauseRun(runId);

  var state = {
    run_id: runId,
    batch_phase: phase,
    next_phase: nextPhase,
    final_phase: !!finalPhase,
    anchor_card_id: anchorCardId,
    status: "pending",
    dispatch_ids: [],
    gates: [],
    gate_title: _deployGateTitle(phase),
    created_at: new Date().toISOString()
  };

  savePhaseGateState(runId, phase, state);
  autoQueueLog("info", _deployGateTitle(phase) + " 생성 — Rust가 비동기로 실행합니다", {
    run_id: runId,
    card_id: anchorCardId,
    batch_phase: phase
  });
  return state;
}

function _createPhaseGateDispatches(runId, phase, nextPhase, finalPhase, anchorCardId) {
  if (_isDeployPhase(runId, phase)) {
    return _createDeployGateDispatch(runId, phase, nextPhase, finalPhase, anchorCardId);
  }

  var existing = loadPhaseGateState(runId, phase);
  if (existing) {
    pauseRun(runId);
    autoQueueLog("info", "Phase gate already exists for run " + runId + " phase " + phase, {
      run_id: runId,
      card_id: anchorCardId,
      batch_phase: phase
    });
    return existing;
  }

  var state = {
    run_id: runId,
    batch_phase: phase,
    next_phase: nextPhase,
    final_phase: !!finalPhase,
    anchor_card_id: anchorCardId,
    status: "pending",
    dispatch_ids: [],
    gates: [],
    created_at: new Date().toISOString()
  };
  pauseRun(runId);

  savePhaseGateState(runId, phase, state);
  autoQueueLog("info", "Created tick-based phase gate for run " + runId + " phase " + phase, {
    run_id: runId,
    card_id: anchorCardId,
    batch_phase: phase
  });
  return state;
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

// ── Consultation dispatch helper (#256) ─────────────────────────
function _createConsultationDispatch(entry, agentId, preflightMeta) {
  // Find the counterpart agent for consultation
  var agent = agentdesk.db.query(
    "SELECT cli_provider FROM agents WHERE id = ?",
    [agentId]
  );
  var provider = (agent.length > 0) ? agent[0].cli_provider : "claude";
  var counterProvider = (provider === "claude") ? "codex" : "claude";
  var counterAgent = agentdesk.db.query(
    "SELECT id FROM agents WHERE cli_provider = ? LIMIT 1",
    [counterProvider]
  );
  var consultAgentId = (counterAgent.length > 0) ? counterAgent[0].id : agentId;

  try {
    var dispatchId = agentdesk.dispatch.create(
      entry.kanban_card_id,
      consultAgentId,
      "consultation",
      "[Consultation] " + entry.title
    );
    if (dispatchId) {
      agentdesk.autoQueue.recordConsultationDispatch(
        entry.id,
        entry.kanban_card_id,
        dispatchId,
        "consultation_dispatch_created",
        preflightMeta
      );
      autoQueueLog("info", "Created consultation dispatch " + dispatchId + " for " + entry.kanban_card_id, {
        entry_id: entry.id,
        card_id: entry.kanban_card_id,
        dispatch_id: dispatchId
      });
    }
  } catch (e) {
    autoQueueLog("warn", "Consultation dispatch failed for " + entry.kanban_card_id + ": " + e, {
      entry_id: entry.id,
      card_id: entry.kanban_card_id
    });
  }
}

agentdesk.registerPolicy(autoQueue);
