var autoQueue = {
  name: "auto-queue",
  priority: 500,

  // ── Auto-skip: detect cards progressed outside of auto-queue ──
  // If a pending queue entry's card gets dispatched externally (by PMD, user, etc.),
  // skip the entry so auto-queue doesn't try to dispatch it again.
  onCardTransition: function(payload) {
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
      agentdesk.db.execute(
        "UPDATE auto_queue_entries SET status = 'skipped' WHERE id = ?",
        [entries[i].id]
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
      "SELECT e.run_id, COALESCE(e.thread_group, 0) as thread_group FROM auto_queue_entries e " +
      "JOIN auto_queue_runs r ON e.run_id = r.id " +
      "WHERE e.kanban_card_id = ? AND e.status IN ('done', 'skipped') " +
      "AND r.status IN ('active', 'paused') " +
      "ORDER BY CASE WHEN e.status = 'done' THEN 0 ELSE 1 END ASC, e.completed_at DESC LIMIT 1",
      [payload.card_id]
    );
    if (!doneEntries || doneEntries.length === 0 || !doneEntries[0] || !doneEntries[0].run_id) {
      return;
    }

    var runId = doneEntries[0].run_id;
    var doneGroup = doneEntries[0].thread_group;

    // Check if the entire run is complete (no pending or dispatched entries remain)
    var remaining = agentdesk.db.query(
      "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND status IN ('pending', 'dispatched')",
      [runId]
    );
    if (remaining.length > 0 && remaining[0].cnt === 0) {
      activateRun(runId, null);
      var runInfo = agentdesk.db.query(
        "SELECT repo, unified_thread_id, unified_thread_channel_id, COALESCE(thread_group_count, 1) as group_count FROM auto_queue_runs WHERE id = ?",
        [runId]
      );
      notifyRunCompleted(runId, runInfo.length > 0 ? runInfo[0] : null);
      return;
    }

    // #140: Check if the completed entry's GROUP is now done
    var groupRemaining = agentdesk.db.query(
      "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND COALESCE(thread_group, 0) = ? AND status IN ('pending', 'dispatched')",
      [runId, doneGroup]
    );
    var groupDone = groupRemaining.length > 0 && groupRemaining[0].cnt === 0;

    // Read run config for parallel dispatch
    var runCfg = agentdesk.db.query(
      "SELECT COALESCE(max_concurrent_threads, 1) as mct, COALESCE(max_concurrent_per_agent, 1) as mca FROM auto_queue_runs WHERE id = ?",
      [runId]
    );
    var maxConcurrent = (runCfg.length > 0) ? runCfg[0].mct : 1;
    var maxPerAgent = (runCfg.length > 0) ? runCfg[0].mca : 1;

    // Check if agent has any active (non-terminal) cards — don't dispatch if busy
    var tCfg = agentdesk.pipeline.getConfig();
    var tKickoff = agentdesk.pipeline.kickoffState(tCfg);
    var tInProgress = agentdesk.pipeline.nextGatedTarget(tKickoff, tCfg);
    var tReview = agentdesk.pipeline.nextGatedTarget(tInProgress, tCfg);
    var activeStates = [tKickoff, tInProgress, tReview].filter(function(s) { return s; });
    var placeholders = activeStates.map(function() { return "?"; }).join(",");
    var active = agentdesk.db.query(
      "SELECT COUNT(*) as cnt FROM kanban_cards WHERE assigned_agent_id = ? AND status IN (" + placeholders + ")",
      [agentId].concat(activeStates)
    );
    var agentBusy = active.length > 0 && active[0].cnt > 0;

    if (!groupDone) {
      // Group still has pending entries — dispatch next in same group (sequential within group)
      if (!agentBusy) {
        activateRun(runId, doneGroup);
      } else {
        agentdesk.log.info("[auto-queue] Agent " + agentId + " still busy, deferring group " + doneGroup + " next dispatch");
      }
      return;
    }

    activateRun(runId, null);

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
      agentdesk.db.execute(
        "UPDATE auto_queue_entries SET status = 'skipped', completed_at = datetime('now') WHERE id = ? AND status = 'pending'",
        [pending.id]
      );
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
      agentdesk.db.execute(
        "UPDATE auto_queue_entries SET status = 'pending', dispatch_id = NULL, dispatched_at = NULL WHERE id = ?",
        [stuck.id]
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

function activateRun(runId, threadGroup) {
  if (!runId) return null;
  var apiPort = agentdesk.config.get("server_port");
  if (!apiPort) {
    agentdesk.log.error("[auto-queue] server_port missing — cannot call /api/auto-queue/activate for run " + runId);
    return null;
  }

  var body = {
    run_id: runId,
    active_only: true
  };
  if (threadGroup !== null && threadGroup !== undefined) {
    body.thread_group = threadGroup;
  }

  var url = "http://127.0.0.1:" + apiPort + "/api/auto-queue/activate";
  var resp = agentdesk.http.post(url, body);
  if (!resp || resp.error) {
    agentdesk.log.warn("[auto-queue] activate API failed for run " + runId + ": " + JSON.stringify(resp));
    return null;
  }
  return resp;
}

// ── Shared dispatch helper (group-aware) (#140) ─────────────────
function dispatchNextEntryInGroup(agentId, runId, threadGroup) {
  var result = activateRun(runId, threadGroup);
  if (!result) return;
  if (result.count > 0) {
    agentdesk.log.info("[auto-queue] activate API dispatched " + result.count + " entry(s) for run " + runId + " group " + threadGroup);
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
      agentdesk.db.execute(
        "UPDATE auto_queue_entries SET status = 'dispatched', dispatch_id = ?, dispatched_at = datetime('now') WHERE id = ?",
        [dispatchId, entry.id]
      );
      agentdesk.log.info("[auto-queue] Created consultation dispatch " + dispatchId + " for " + entry.kanban_card_id);
    }
  } catch (e) {
    agentdesk.log.warn("[auto-queue] Consultation dispatch failed for " + entry.kanban_card_id + ": " + e);
  }
}

// Legacy helper for backward compatibility
function dispatchNextEntry(agentId) {
  var apiPort = agentdesk.config.get("server_port");
  if (!apiPort) return;
  agentdesk.http.post(
    "http://127.0.0.1:" + apiPort + "/api/auto-queue/activate",
    {
      agent_id: agentId,
      active_only: true
    }
  );
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
    var ch = agentdesk.agents.resolvePrimaryChannel(fallbackAgents[i].agent_id);
    if (ch) targets[ch] = true;
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
