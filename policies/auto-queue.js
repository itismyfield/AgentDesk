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

    // #145: Find the auto-queue entry that was just marked 'done' for this card.
    var doneEntries = agentdesk.db.query(
      "SELECT e.run_id, COALESCE(e.thread_group, 0) as thread_group FROM auto_queue_entries e " +
      "JOIN auto_queue_runs r ON e.run_id = r.id " +
      "WHERE e.kanban_card_id = ? AND e.status = 'done' " +
      "AND e.dispatch_id IS NOT NULL " +
      "AND r.status IN ('active', 'paused') " +
      "ORDER BY e.completed_at DESC LIMIT 1",
      [payload.card_id]
    );
    if (doneEntries.length === 0) return;

    var runId = doneEntries[0].run_id;
    var doneGroup = doneEntries[0].thread_group;

    // Check if the entire run is complete (no pending or dispatched entries remain)
    var remaining = agentdesk.db.query(
      "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND status IN ('pending', 'dispatched')",
      [runId]
    );
    if (remaining.length > 0 && remaining[0].cnt === 0) {
      var runInfo = agentdesk.db.query(
        "SELECT unified_thread_id, unified_thread_channel_id, COALESCE(thread_group_count, 1) as group_count FROM auto_queue_runs WHERE id = ?",
        [runId]
      );
      if (runInfo.length > 0 && runInfo[0].unified_thread_id) {
        // #140: For parallel runs, send kill signals for ALL group threads
        var threadIds = [];
        try {
          var map = JSON.parse(runInfo[0].unified_thread_id);
          if (runInfo[0].group_count > 1) {
            // Nested format: {"group_num": {"channel_id": "thread_id"}}
            for (var gk in map) {
              if (typeof map[gk] === "object") {
                for (var ck in map[gk]) {
                  threadIds.push(map[gk][ck]);
                }
              }
            }
          } else {
            // Flat format: {"channel_id": "thread_id"}
            for (var ck in map) {
              threadIds.push(map[ck]);
            }
          }
        } catch (e) { /* ignore parse errors */ }
        // Fallback: also include scalar unified_thread_channel_id if not already present
        var scalarId = runInfo[0].unified_thread_channel_id;
        if (scalarId && threadIds.indexOf(scalarId) === -1) {
          threadIds.push(scalarId);
        }
        for (var ti = 0; ti < threadIds.length; ti++) {
          agentdesk.log.info("[auto-queue] Run " + runId + " complete — requesting tmux cleanup for thread " + threadIds[ti]);
          agentdesk.db.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
            ["kill_unified_thread:" + threadIds[ti], runId]
          );
        }
      }
      agentdesk.db.execute(
        "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') WHERE id = ?",
        [runId]
      );
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
        dispatchNextEntryInGroup(agentId, runId, doneGroup);
      } else {
        agentdesk.log.info("[auto-queue] Agent " + agentId + " still busy, deferring group " + doneGroup + " next dispatch");
      }
      return;
    }

    // Group is done — check if we can start a new pending group
    var activeGroups = agentdesk.db.query(
      "SELECT COUNT(DISTINCT COALESCE(thread_group, 0)) as cnt FROM auto_queue_entries WHERE run_id = ? AND status = 'dispatched'",
      [runId]
    );
    var activeGroupCount = (activeGroups.length > 0) ? activeGroups[0].cnt : 0;
    var slotsAvailable = maxConcurrent - activeGroupCount;

    if (slotsAvailable <= 0) {
      agentdesk.log.info("[auto-queue] No slots available (active: " + activeGroupCount + ", max: " + maxConcurrent + ")");
      return;
    }

    // Find next pending groups (not currently active)
    var activeGroupIds = agentdesk.db.query(
      "SELECT DISTINCT COALESCE(thread_group, 0) as g FROM auto_queue_entries WHERE run_id = ? AND status = 'dispatched'",
      [runId]
    );
    var activeSet = {};
    for (var i = 0; i < activeGroupIds.length; i++) {
      activeSet[activeGroupIds[i].g] = true;
    }

    var pendingGroups = agentdesk.db.query(
      "SELECT DISTINCT COALESCE(thread_group, 0) as g FROM auto_queue_entries WHERE run_id = ? AND status = 'pending' ORDER BY thread_group ASC",
      [runId]
    );

    var dispatched = 0;
    for (var j = 0; j < pendingGroups.length && dispatched < slotsAvailable; j++) {
      var grp = pendingGroups[j].g;
      if (activeSet[grp]) continue;

      // Find the agent for the first entry in this group
      var nextInGroup = agentdesk.db.query(
        "SELECT e.agent_id FROM auto_queue_entries e WHERE e.run_id = ? AND COALESCE(e.thread_group, 0) = ? AND e.status = 'pending' ORDER BY e.priority_rank ASC LIMIT 1",
        [runId, grp]
      );
      if (nextInGroup.length === 0) continue;

      var nextAgent = nextInGroup[0].agent_id;

      // Per-agent concurrency check
      var agentActive = agentdesk.db.query(
        "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND agent_id = ? AND status = 'dispatched'",
        [runId, nextAgent]
      );
      if (agentActive.length > 0 && agentActive[0].cnt >= maxPerAgent) {
        agentdesk.log.info("[auto-queue] Agent " + nextAgent + " at max_concurrent_per_agent, skipping group " + grp);
        continue;
      }

      // Check if agent is busy with non-queue work
      var agentCards = agentdesk.db.query(
        "SELECT COUNT(*) as cnt FROM kanban_cards WHERE assigned_agent_id = ? AND status IN (" + placeholders + ")",
        [nextAgent].concat(activeStates)
      );
      if (agentCards.length > 0 && agentCards[0].cnt > 0) {
        agentdesk.log.info("[auto-queue] Agent " + nextAgent + " busy, skipping group " + grp);
        continue;
      }

      dispatchNextEntryInGroup(nextAgent, runId, grp);
      dispatched++;
    }

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

    // Find active runs with pending entries
    var activeRuns = agentdesk.db.query(
      "SELECT DISTINCT r.id, COALESCE(r.max_concurrent_threads, 1) as mct, COALESCE(r.max_concurrent_per_agent, 1) as mca " +
      "FROM auto_queue_runs r " +
      "JOIN auto_queue_entries e ON e.run_id = r.id " +
      "WHERE r.status = 'active' AND e.status = 'pending'",
      []
    );

    for (var ri = 0; ri < activeRuns.length; ri++) {
      var run = activeRuns[ri];

      // Count active groups for this run
      var activeGroupCount = agentdesk.db.query(
        "SELECT COUNT(DISTINCT COALESCE(thread_group, 0)) as cnt FROM auto_queue_entries WHERE run_id = ? AND status = 'dispatched'",
        [run.id]
      );
      var currentActive = (activeGroupCount.length > 0) ? activeGroupCount[0].cnt : 0;
      var slots = run.mct - currentActive;

      // Find pending groups not currently active
      var activeGroupIds = agentdesk.db.query(
        "SELECT DISTINCT COALESCE(thread_group, 0) as g FROM auto_queue_entries WHERE run_id = ? AND status = 'dispatched'",
        [run.id]
      );
      var aSet = {};
      for (var ai = 0; ai < activeGroupIds.length; ai++) {
        aSet[activeGroupIds[ai].g] = true;
      }

      var pendingGroups = agentdesk.db.query(
        "SELECT DISTINCT COALESCE(thread_group, 0) as g FROM auto_queue_entries WHERE run_id = ? AND status = 'pending' ORDER BY thread_group ASC",
        [run.id]
      );

      var tickDispatched = 0;
      for (var gi = 0; gi < pendingGroups.length; gi++) {
        var grp = pendingGroups[gi].g;

        // For active groups, check if they need intra-group continuation
        // For inactive groups, check if we have slots
        if (aSet[grp]) {
          // Active group — check if there's a dispatched entry still running
          var dispatched = agentdesk.db.query(
            "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND COALESCE(thread_group, 0) = ? AND status = 'dispatched'",
            [run.id, grp]
          );
          if (dispatched.length > 0 && dispatched[0].cnt > 0) continue; // Still running
        } else {
          if (tickDispatched >= slots) continue; // No slots
        }

        var nextEntry = agentdesk.db.query(
          "SELECT e.agent_id FROM auto_queue_entries e WHERE e.run_id = ? AND COALESCE(e.thread_group, 0) = ? AND e.status = 'pending' ORDER BY e.priority_rank ASC LIMIT 1",
          [run.id, grp]
        );
        if (nextEntry.length === 0) continue;

        var nextAgent = nextEntry[0].agent_id;

        // Check if agent is idle
        var busy = agentdesk.db.query(
          "SELECT COUNT(*) as cnt FROM kanban_cards WHERE assigned_agent_id = ? AND status IN (" + tickPlaceholders + ")",
          [nextAgent].concat(tickActiveStates)
        );
        if (busy.length > 0 && busy[0].cnt > 0) continue;

        // Per-agent concurrency check
        var agentDispatched = agentdesk.db.query(
          "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND agent_id = ? AND status = 'dispatched'",
          [run.id, nextAgent]
        );
        if (agentDispatched.length > 0 && agentDispatched[0].cnt >= run.mca) continue;

        agentdesk.log.info("[auto-queue] onTick1min recovery: dispatching group " + grp + " for agent " + nextAgent);
        dispatchNextEntryInGroup(nextAgent, run.id, grp);
        if (!aSet[grp]) tickDispatched++;
      }
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

// ── Shared dispatch helper (group-aware) (#140) ─────────────────
function dispatchNextEntryInGroup(agentId, runId, threadGroup) {
  // #179/#140: Guard — skip if this group already has a dispatched entry.
  // Prevents onCardTerminal + onTick1min race from creating duplicate dispatches.
  var alreadyDispatched = agentdesk.db.query(
    "SELECT e.id FROM auto_queue_entries e " +
    "WHERE e.run_id = ? AND COALESCE(e.thread_group, 0) = ? AND e.status = 'dispatched' LIMIT 1",
    [runId, threadGroup]
  );
  if (alreadyDispatched.length > 0) {
    agentdesk.log.info("[auto-queue] Skipping group " + threadGroup + " dispatch — already has dispatched entry " + alreadyDispatched[0].id);
    return;
  }

  var nextEntry = agentdesk.db.query(
    "SELECT e.id, e.kanban_card_id, kc.title " +
    "FROM auto_queue_entries e " +
    "JOIN kanban_cards kc ON e.kanban_card_id = kc.id " +
    "WHERE e.run_id = ? AND COALESCE(e.thread_group, 0) = ? AND e.agent_id = ? AND e.status = 'pending' " +
    "ORDER BY e.priority_rank ASC LIMIT 1",
    [runId, threadGroup, agentId]
  );

  if (nextEntry.length === 0) {
    // Try any agent in this group (agent may differ per entry)
    nextEntry = agentdesk.db.query(
      "SELECT e.id, e.kanban_card_id, kc.title, e.agent_id " +
      "FROM auto_queue_entries e " +
      "JOIN kanban_cards kc ON e.kanban_card_id = kc.id " +
      "WHERE e.run_id = ? AND COALESCE(e.thread_group, 0) = ? AND e.status = 'pending' " +
      "ORDER BY e.priority_rank ASC LIMIT 1",
      [runId, threadGroup]
    );
    if (nextEntry.length === 0) return;
    agentId = nextEntry[0].agent_id;
  }

  var entry = nextEntry[0];
  agentdesk.log.info("[auto-queue] Dispatching group " + threadGroup + " entry for " + agentId + ": " + entry.kanban_card_id);

  try {
    // #173: Use dispatch.create which defers INSERT via intent.
    // Mark entry as dispatched ONLY after dispatch.create succeeds validation.
    // The actual dispatch INSERT happens when intents are applied (post-hook).
    // If intent fails, recovery path 2 (onTick1min) will detect orphan entry
    // and reset it to pending.
    var dispatchId = agentdesk.dispatch.create(
      entry.kanban_card_id,
      agentId,
      "implementation",
      entry.title
    );

    // Only update entry if dispatchId is truthy (validation passed)
    if (dispatchId) {
      agentdesk.db.execute(
        "UPDATE auto_queue_entries SET status = 'dispatched', dispatch_id = ?, dispatched_at = datetime('now') WHERE id = ?",
        [dispatchId, entry.id]
      );
    }
  } catch (e) {
    agentdesk.log.warn("[auto-queue] dispatch failed for " + entry.kanban_card_id + " (group " + threadGroup + "), will retry on next tick: " + e);
  }
}

// Legacy helper for backward compatibility
function dispatchNextEntry(agentId) {
  // #179: Guard — skip if there's already a dispatched entry for this agent in the active run.
  // Prevents onCardTerminal + onTick1min race from creating duplicate dispatches.
  var alreadyDispatched = agentdesk.db.query(
    "SELECT e.id FROM auto_queue_entries e " +
    "JOIN auto_queue_runs r ON e.run_id = r.id " +
    "WHERE e.agent_id = ? AND e.status = 'dispatched' AND r.status = 'active' LIMIT 1",
    [agentId]
  );
  if (alreadyDispatched.length > 0) {
    agentdesk.log.info("[auto-queue] Skipping dispatch for " + agentId + " — already has dispatched entry " + alreadyDispatched[0].id);
    return;
  }

  var nextEntry = agentdesk.db.query(
    "SELECT e.id, e.kanban_card_id, e.run_id, COALESCE(e.thread_group, 0) as thread_group, kc.title " +
    "FROM auto_queue_entries e " +
    "JOIN auto_queue_runs r ON e.run_id = r.id " +
    "JOIN kanban_cards kc ON e.kanban_card_id = kc.id " +
    "WHERE e.agent_id = ? AND e.status = 'pending' AND r.status = 'active' " +
    "ORDER BY e.priority_rank ASC LIMIT 1",
    [agentId]
  );

  if (nextEntry.length === 0) return;

  var entry = nextEntry[0];
  dispatchNextEntryInGroup(agentId, entry.run_id, entry.thread_group);
}

agentdesk.registerPolicy(autoQueue);
