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
      agentdesk.log.info("[auto-queue] Skipped entry " + entries[i].id + " — card " + payload.card_id + " progressed externally to " + payload.to);
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
      if (!dispatchCompletedWithoutChanges(dispatch, result) || !dispatch.kanban_card_id) return;

      var cards = agentdesk.db.query(
        "SELECT id, status, assigned_agent_id FROM kanban_cards WHERE id = ?",
        [dispatch.kanban_card_id]
      );
      if (cards.length === 0) return;

      var card = cards[0];
      var cfg = agentdesk.pipeline.resolveForCard(card.id);
      if (agentdesk.pipeline.isTerminal(card.status, cfg)) {
        return;
      }

      var aqEntries = agentdesk.db.query(
        "SELECT e.id, e.run_id, e.agent_id, e.status, COALESCE(e.thread_group, 0) as thread_group, COALESCE(e.batch_phase, 0) as batch_phase " +
        "FROM auto_queue_entries e " +
        "JOIN auto_queue_runs r ON e.run_id = r.id " +
        "WHERE e.dispatch_id = ? AND r.status IN ('active', 'paused') " +
        "ORDER BY CASE " +
        "  WHEN e.status = 'done' THEN 0 " +
        "  WHEN e.status = 'skipped' THEN 1 " +
        "  ELSE 2 END ASC, e.completed_at DESC LIMIT 1",
        [dispatch.id]
      );
      if (aqEntries.length === 0) return;

      var entry = aqEntries[0];
      if (entry.status === "pending" || entry.status === "dispatched") {
        agentdesk.db.execute(
          "UPDATE auto_queue_entries SET status = 'done', completed_at = datetime('now') WHERE id = ? AND status IN ('pending', 'dispatched')",
          [entry.id]
        );
        entry.status = "done";
      }

      continueRunAfterEntry(
        entry.run_id,
        entry.agent_id || card.assigned_agent_id,
        entry.thread_group,
        entry.batch_phase,
        dispatch.kanban_card_id
      );
      agentdesk.log.info("[auto-queue] noop completion advanced entry " + entry.id + " in run " + entry.run_id);
      return;
    }

    var phase = gate.batch_phase || 0;
    var state = loadPhaseGateState(gate.run_id, phase);
    if (!state || !Array.isArray(state.dispatch_ids) || state.dispatch_ids.indexOf(dispatch.id) < 0) {
      return;
    }
    if (state.status === "failed") {
      agentdesk.log.info("[auto-queue] Ignoring phase gate completion for failed run " + gate.run_id + " phase " + phase);
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
      notifyPMD(state.anchor_card_id || dispatch.kanban_card_id, "[phase-gate] phase " + phase + " failed: " + state.failed_reason);
      agentdesk.log.warn("[auto-queue] Phase gate failed for run " + gate.run_id + " phase " + phase + ": " + state.failed_reason);
      return;
    }

    var gateDispatches = loadPhaseGateDispatches(state.dispatch_ids);
    if (gateDispatches.length !== state.dispatch_ids.length) {
      state.status = "failed";
      state.failed_dispatch_id = dispatch.id;
      state.failed_reason = "missing phase gate dispatch rows";
      savePhaseGateState(gate.run_id, phase, state);
      pauseRun(gate.run_id);
      notifyPMD(state.anchor_card_id || dispatch.kanban_card_id, "[phase-gate] phase " + phase + " failed: missing gate dispatch rows");
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
        notifyPMD(state.anchor_card_id || dispatch.kanban_card_id, "[phase-gate] phase " + phase + " failed: " + state.failed_reason);
        return;
      }
    }

    if (pendingCount > 0) {
      agentdesk.log.info("[auto-queue] Phase gate pass waiting for remaining dispatches: run " + gate.run_id + " phase " + phase + " pending=" + pendingCount);
      return;
    }

    clearPhaseGateState(gate.run_id, phase);
    if (state.final_phase || gate.final_phase) {
      completeRunAndNotify(gate.run_id);
      agentdesk.log.info("[auto-queue] Phase gate passed, completed run " + gate.run_id + " at phase " + phase);
      return;
    }

    resumeRunAndActivate(gate.run_id, gate.next_phase);
    agentdesk.log.info("[auto-queue] Phase gate passed, resumed run " + gate.run_id + " from phase " + phase + " to " + (gate.next_phase || "next"));
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
      agentdesk.log.info("[auto-queue] onTick1min: skipping terminal pending entry " + pending.id + " for card " + pending.kanban_card_id + " at " + pending.status);
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
      agentdesk.log.info("[auto-queue] onTick1min: resetting stuck dispatched entry " + stuck.id + " (dispatch " + (stuck.dispatch_id || "NULL") + " is orphan/cancelled/failed/phantom)");
      agentdesk.autoQueue.updateEntryStatus(
        stuck.id,
        "pending",
        "tick_recovery"
      );
    }
  }
};

function dispatchCompletedWithoutChanges(dispatch, result) {
  if (!dispatch) return false;
  if (dispatch.dispatch_type !== "implementation" && dispatch.dispatch_type !== "rework") {
    return false;
  }
  return (result && result.work_outcome === "noop") || (result && result.completed_without_changes === true);
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

function phaseGateKey(runId, phase) {
  return "aq_phase_gate:" + runId + ":" + phase;
}

function loadPhaseGateState(runId, phase) {
  var rows = agentdesk.db.query(
    "SELECT value FROM kv_meta WHERE key = ?",
    [phaseGateKey(runId, phase)]
  );
  if (rows.length === 0 || !rows[0].value) return null;
  try { return JSON.parse(rows[0].value); } catch (e) { return null; }
}

function savePhaseGateState(runId, phase, state) {
  if (!state) return;
  agentdesk.db.execute(
    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
    [phaseGateKey(runId, phase), JSON.stringify(state)]
  );
}

function clearPhaseGateState(runId, phase) {
  agentdesk.db.execute(
    "DELETE FROM kv_meta WHERE key = ?",
    [phaseGateKey(runId, phase)]
  );
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
    "SELECT COUNT(*) as cnt FROM kv_meta " +
    "WHERE key LIKE ? " +
    "AND json_extract(COALESCE(value, '{}'), '$.status') IN ('pending', 'failed')",
    ["aq_phase_gate:" + runId + ":%"]
  );
  return rows.length > 0 && rows[0].cnt > 0;
}

function remainingRunnableEntryCount(runId) {
  var rows = agentdesk.db.query(
    "SELECT COUNT(*) as cnt FROM auto_queue_entries " +
    "WHERE run_id = ? AND status IN ('pending', 'dispatched')",
    [runId]
  );
  return rows.length > 0 ? (rows[0].cnt || 0) : 0;
}

function finalizeRunWithoutPhaseGate(runId) {
  if (!runId) return false;

  var runInfo = loadRunInfo(runId);
  if (!runInfo) return false;
  if (runInfo.status !== "active" && runInfo.status !== "paused") return false;
  if (runHasBlockingPhaseGate(runId)) return false;
  if (remainingRunnableEntryCount(runId) > 0) return false;

  agentdesk.db.execute(
    "UPDATE auto_queue_slots " +
    "SET assigned_run_id = NULL, assigned_thread_group = NULL, updated_at = datetime('now') " +
    "WHERE assigned_run_id = ?",
    [runId]
  );
  agentdesk.db.execute(
    "UPDATE auto_queue_runs " +
    "SET status = 'completed', completed_at = datetime('now') " +
    "WHERE id = ? AND status IN ('active', 'paused')",
    [runId]
  );
  agentdesk.log.info("[auto-queue] Finalized non-phase-gate run " + runId + " and released its slots");
  notifyRunCompleted(runId, runInfo);
  return true;
}

function pauseRun(runId) {
  agentdesk.db.execute(
    "UPDATE auto_queue_runs SET status = 'paused' WHERE id = ? AND status = 'active'",
    [runId]
  );
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

function _phaseGateRequired(runId, phase) {
  return countDistinctBatchPhases(runId) > 1;
}

function completeRunAndNotify(runId) {
  agentdesk.db.execute(
    "UPDATE auto_queue_runs SET status = 'active', completed_at = NULL WHERE id = ? AND status = 'paused'",
    [runId]
  );
  activateRun(runId, null);
  var runInfo = agentdesk.db.query(
    "SELECT repo, unified_thread_id, unified_thread_channel_id, COALESCE(thread_group_count, 1) as group_count FROM auto_queue_runs WHERE id = ?",
    [runId]
  );
  notifyRunCompleted(runId, runInfo.length > 0 ? runInfo[0] : null);
}

function continueRunAfterEntry(runId, agentId, doneGroup, donePhase, anchorCardId) {
  if (!runId || !agentId) return;

  var remaining = agentdesk.db.query(
    "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND status IN ('pending', 'dispatched')",
    [runId]
  );
  var remainingCount = (remaining.length > 0) ? remaining[0].cnt : 0;

  if ((donePhase || 0) > 0) {
    var phaseRemaining = agentdesk.db.query(
      "SELECT COUNT(*) as cnt FROM auto_queue_entries " +
      "WHERE run_id = ? AND status IN ('pending', 'dispatched') AND COALESCE(batch_phase, 0) = ?",
      [runId, donePhase]
    );
    var currentPhaseDone = phaseRemaining.length > 0 && phaseRemaining[0].cnt === 0;
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
      activateRun(runId, doneGroup || 0);
    } else {
      agentdesk.log.info("[auto-queue] Agent " + agentId + " still busy, deferring group " + (doneGroup || 0) + " next dispatch");
    }
    return;
  }

  activateRun(runId, null);
}

function resumeRunAndActivate(runId, nextPhase) {
  agentdesk.db.execute(
    "UPDATE auto_queue_runs SET status = 'active', completed_at = NULL WHERE id = ? AND status = 'paused'",
    [runId]
  );
  if (nextPhase !== null && nextPhase !== undefined) {
    agentdesk.log.info("[auto-queue] Resuming run " + runId + " for phase " + nextPhase);
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

function _createPhaseGateDispatches(runId, phase, nextPhase, finalPhase, anchorCardId) {
  var existing = loadPhaseGateState(runId, phase);
  if (existing) {
    pauseRun(runId);
    agentdesk.log.info("[auto-queue] Phase gate already exists for run " + runId + " phase " + phase);
    return existing;
  }

  var groups = _buildPhaseGateGroups(runId, phase);
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

  if (groups.length === 0) {
    state.status = "failed";
    state.failed_reason = "no phase gate targets found";
    savePhaseGateState(runId, phase, state);
    notifyPMD(anchorCardId, "[phase-gate] run " + runId.substring(0, 8) + " phase " + phase + " has no gate targets");
    return state;
  }

  var errors = [];
  for (var i = 0; i < groups.length; i++) {
    var group = groups[i];
    try {
      var dispatchId = agentdesk.dispatch.create(
        group.anchor_card_id || anchorCardId,
        group.target_agent_id,
        group.dispatch_type,
        _phaseGateTitle(group, phase, runId),
        {
          auto_queue: true,
          sidecar_dispatch: true,
          phase_gate: {
            run_id: runId,
            batch_phase: phase,
            next_phase: nextPhase,
            final_phase: !!finalPhase,
            source_agent_id: group.source_agent_id,
            target_agent_id: group.target_agent_id,
            dispatch_type: group.dispatch_type,
            pass_verdict: group.pass_verdict,
            checks: group.checks,
            card_ids: group.card_ids,
            issue_numbers: group.issue_numbers,
            work_items: group.work_items,
            expected_gate_count: groups.length
          }
        }
      );
      state.dispatch_ids.push(dispatchId);
      state.gates.push({
        dispatch_id: dispatchId,
        source_agent_id: group.source_agent_id,
        target_agent_id: group.target_agent_id,
        dispatch_type: group.dispatch_type,
        pass_verdict: group.pass_verdict,
        checks: group.checks,
        card_ids: group.card_ids
      });
    } catch (e) {
      errors.push((group.target_agent_id || "unknown") + ": " + e);
    }
  }

  if (errors.length > 0 || state.dispatch_ids.length === 0) {
    state.status = "failed";
    state.failed_reason = errors.join("; ") || "phase gate dispatch creation failed";
    savePhaseGateState(runId, phase, state);
    notifyPMD(anchorCardId, "[phase-gate] run " + runId.substring(0, 8) + " phase " + phase + " setup failed: " + state.failed_reason);
    agentdesk.log.warn("[auto-queue] Phase gate setup failed for run " + runId + " phase " + phase + ": " + state.failed_reason);
    return state;
  }

  savePhaseGateState(runId, phase, state);
  agentdesk.log.info("[auto-queue] Created " + state.dispatch_ids.length + " phase gate dispatch(es) for run " + runId + " phase " + phase);
  return state;
}

function activateRun(runId, threadGroup) {
  if (!runId) return null;
  try {
    return agentdesk.autoQueue.activate(runId, threadGroup);
  } catch (e) {
    agentdesk.log.warn("[auto-queue] activate bridge failed for run " + runId + ": " + e);
    return null;
  }
}

// ── Shared dispatch helper (group-aware) (#140) ─────────────────
function dispatchNextEntryInGroup(agentId, runId, threadGroup) {
  var result = activateRun(runId, threadGroup);
  if (!result) return;
  if (result.count > 0) {
    agentdesk.log.info("[auto-queue] activate bridge dispatched " + result.count + " entry(s) for run " + runId + " group " + threadGroup);
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
      // Update metadata with consultation info
      var newMeta = JSON.parse(JSON.stringify(preflightMeta));
      newMeta.consultation_status = "pending";
      newMeta.consultation_dispatch_id = dispatchId;
      agentdesk.db.execute(
        "UPDATE kanban_cards SET metadata = ? WHERE id = ?",
        [JSON.stringify(newMeta), entry.kanban_card_id]
      );
      agentdesk.autoQueue.updateEntryStatus(
        entry.id,
        "dispatched",
        "consultation_dispatch_created",
        { dispatchId: dispatchId }
      );
      agentdesk.log.info("[auto-queue] Created consultation dispatch " + dispatchId + " for " + entry.kanban_card_id);
    }
  } catch (e) {
    agentdesk.log.warn("[auto-queue] Consultation dispatch failed for " + entry.kanban_card_id + ": " + e);
  }
}

// Legacy helper for backward compatibility
function dispatchNextEntry(agentId) {
  if (!agentId) return;
  try {
    agentdesk.autoQueue.activate({
      agent_id: agentId,
      active_only: true
    });
  } catch (e) {
    agentdesk.log.warn("[auto-queue] legacy activate bridge failed for agent " + agentId + ": " + e);
  }
}

function collectRunMainChannels(runId, runInfo) {
  var targets = {};

  if (runInfo && runInfo.unified_thread_id) {
    try {
      var map = JSON.parse(runInfo.unified_thread_id);
      for (var key in map) {
        if (!Object.prototype.hasOwnProperty.call(map, key)) continue;
        var value = map[key];
        if (value && typeof value === "object" && !Array.isArray(value)) {
          for (var nestedKey in value) {
            if (!Object.prototype.hasOwnProperty.call(value, nestedKey)) continue;
            if (/^\d+$/.test(nestedKey)) targets[nestedKey] = true;
          }
        } else if (/^\d+$/.test(key)) {
          targets[key] = true;
        }
      }
    } catch (e) {
      agentdesk.log.warn("[auto-queue] Failed to parse unified_thread_id for run " + runId + ": " + e);
    }
  }

  var channelIds = Object.keys(targets);
  if (channelIds.length > 0) return channelIds;

  // #304: resolve primary channel via centralized resolver instead of legacy column
  var fallbackAgents = agentdesk.db.query(
    "SELECT DISTINCT e.agent_id FROM auto_queue_entries e WHERE e.run_id = ?",
    [runId]
  );
  for (var i = 0; i < fallbackAgents.length; i++) {
    try {
      var ch = agentdesk.agents && agentdesk.agents.resolvePrimaryChannel
        ? agentdesk.agents.resolvePrimaryChannel(fallbackAgents[i].agent_id)
        : null;
      if (ch) targets[ch] = true;
    } catch (e) {
      agentdesk.log.warn("[auto-queue] resolvePrimaryChannel failed for " + fallbackAgents[i].agent_id + ": " + e);
    }
  }
  return Object.keys(targets);
}

function notifyRunCompleted(runId, runInfo) {
  var channelIds = collectRunMainChannels(runId, runInfo);
  if (channelIds.length === 0) {
    agentdesk.log.info("[auto-queue] Run " + runId + " complete — no main channel found for notify");
    return;
  }

  var totals = agentdesk.db.query(
    "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ?",
    [runId]
  );
  var totalCount = (totals.length > 0) ? totals[0].cnt : 0;
  var repoLabel = (runInfo && runInfo.repo) ? runInfo.repo : "auto-queue";
  var shortRun = runId.substring(0, 8);
  var message = "자동큐 완료: " + repoLabel + " / run " + shortRun + " / " + totalCount + "개";

  for (var i = 0; i < channelIds.length; i++) {
    agentdesk.message.queue("channel:" + channelIds[i], message, "notify", "system");
  }
}

agentdesk.registerPolicy(autoQueue);
