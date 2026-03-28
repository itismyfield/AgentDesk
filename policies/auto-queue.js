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

  // ── Authoritative auto-queue continuation (#110) ──────────────
  // This is the SINGLE path for done → next queued item.
  // Rust transition_status() already marks auto_queue_entries as 'done'
  // before firing OnCardTerminal, so we don't re-mark here.
  // kanban-rules.js does NOT touch auto_queue_entries (removed in #110).
  onCardTerminal: function(payload) {
    var cards = agentdesk.db.query(
      "SELECT assigned_agent_id, latest_dispatch_id FROM kanban_cards WHERE id = ?",
      [payload.card_id]
    );
    if (cards.length === 0 || !cards[0].assigned_agent_id) return;

    var agentId = cards[0].assigned_agent_id;
    var latestDispatchId = cards[0].latest_dispatch_id;

    // #145: Use dispatch_id to find the exact run this dispatch belongs to.
    // Eliminates card_id ambiguity when same card is in multiple runs.
    var doneEntries = latestDispatchId ? agentdesk.db.query(
      "SELECT e.run_id FROM auto_queue_entries e " +
      "JOIN auto_queue_runs r ON e.run_id = r.id " +
      "WHERE e.dispatch_id = ? AND e.status = 'done' " +
      "AND r.status IN ('active', 'paused') " +
      "LIMIT 1",
      [latestDispatchId]
    ) : [];
    if (doneEntries.length === 0) return;

    var runId = doneEntries[0].run_id;

    // #145: Check if the unified run is now complete (no pending or dispatched entries remain).
    // This must happen here in onCardTerminal — NOT inside dispatchNextEntry — because
    // the last entry goes terminal without triggering another dispatch.
    var remaining = agentdesk.db.query(
      "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND status IN ('pending', 'dispatched')",
      [runId]
    );
    if (remaining.length > 0 && remaining[0].cnt === 0) {
      var runInfo = agentdesk.db.query(
        "SELECT unified_thread_id, unified_thread_channel_id FROM auto_queue_runs WHERE id = ?",
        [runId]
      );
      if (runInfo.length > 0 && runInfo[0].unified_thread_id) {
        var channelId = runInfo[0].unified_thread_channel_id;
        if (channelId) {
          agentdesk.log.info("[auto-queue] Unified-thread run " + runId + " complete — requesting tmux cleanup for channel " + channelId);
          agentdesk.db.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
            ["kill_unified_thread:" + channelId, runId]
          );
        }
      }
      agentdesk.db.execute(
        "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') WHERE id = ?",
        [runId]
      );
      return;
    }

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
    if (active.length > 0 && active[0].cnt > 0) {
      agentdesk.log.info("[auto-queue] Agent " + agentId + " still has active cards, deferring next dispatch");
      return;
    }

    dispatchNextEntry(agentId);
  },

  // ── Periodic recovery: dispatch next entry for idle agents (#110) ──
  // Catches cases where onCardTerminal dispatch failed or was missed.
  onTick: function() {
    var tickCfg = agentdesk.pipeline.getConfig();
    var tickKickoff = agentdesk.pipeline.kickoffState(tickCfg);
    var tickInProgress = agentdesk.pipeline.nextGatedTarget(tickKickoff, tickCfg);
    var tickReview = agentdesk.pipeline.nextGatedTarget(tickInProgress, tickCfg);
    var tickActiveStates = [tickKickoff, tickInProgress, tickReview].filter(function(s) { return s; });
    var tickPlaceholders = tickActiveStates.map(function() { return "?"; }).join(",");
    var idleWithQueue = agentdesk.db.query(
      "SELECT DISTINCT e.agent_id " +
      "FROM auto_queue_entries e " +
      "JOIN auto_queue_runs r ON e.run_id = r.id " +
      "WHERE e.status = 'pending' AND r.status = 'active' " +
      "AND NOT EXISTS (" +
      "  SELECT 1 FROM kanban_cards kc " +
      "  WHERE kc.assigned_agent_id = e.agent_id " +
      "  AND kc.status IN (" + tickPlaceholders + ")" +
      ")",
      tickActiveStates
    );

    for (var i = 0; i < idleWithQueue.length; i++) {
      var agentId = idleWithQueue[i].agent_id;
      agentdesk.log.info("[auto-queue] onTick recovery: idle agent " + agentId + " has pending entries, dispatching");
      dispatchNextEntry(agentId);
    }
  }
};

// ── Shared dispatch helper ─────────────────────────────────────
function dispatchNextEntry(agentId) {
  var nextEntry = agentdesk.db.query(
    "SELECT e.id, e.kanban_card_id, e.run_id, kc.title " +
    "FROM auto_queue_entries e " +
    "JOIN auto_queue_runs r ON e.run_id = r.id " +
    "JOIN kanban_cards kc ON e.kanban_card_id = kc.id " +
    "WHERE e.agent_id = ? AND e.status = 'pending' AND r.status = 'active' " +
    "ORDER BY e.priority_rank ASC LIMIT 1",
    [agentId]
  );

  if (nextEntry.length === 0) return;

  var entry = nextEntry[0];
  agentdesk.log.info("[auto-queue] Dispatching next entry for " + agentId + ": " + entry.kanban_card_id);

  try {
    var dispatchId = agentdesk.dispatch.create(
      entry.kanban_card_id,
      agentId,
      "implementation",
      entry.title
    );

    // Dispatch succeeded — mark entry with dispatch_id for direct run association (#145)
    agentdesk.db.execute(
      "UPDATE auto_queue_entries SET status = 'dispatched', dispatch_id = ?, dispatched_at = datetime('now') WHERE id = ?",
      [dispatchId, entry.id]
    );

    // #145: Completion check moved to onCardTerminal where it can observe
    // the final entry reaching 'done' state. Inside dispatchNextEntry the
    // just-dispatched entry is counted as 'dispatched', so cnt can never be 0.
  } catch (e) {
    agentdesk.log.warn("[auto-queue] dispatch failed for " + entry.kanban_card_id + ", will retry on next tick: " + e);
  }
}

agentdesk.registerPolicy(autoQueue);
