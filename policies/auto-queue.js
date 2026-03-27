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
      "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?",
      [payload.card_id]
    );
    if (cards.length === 0 || !cards[0].assigned_agent_id) return;

    var agentId = cards[0].assigned_agent_id;

    // Verify this card had a dispatched queue entry (Rust already set it to 'done').
    // If no entry was in 'done' state with recent completion, this card is not from auto-queue.
    var wasQueued = agentdesk.db.query(
      "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE kanban_card_id = ? AND status = 'done'",
      [payload.card_id]
    );
    if (wasQueued.length === 0 || wasQueued[0].cnt === 0) return;

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
    agentdesk.dispatch.create(
      entry.kanban_card_id,
      agentId,
      "implementation",
      entry.title
    );

    // Dispatch succeeded — mark entry
    agentdesk.db.execute(
      "UPDATE auto_queue_entries SET status = 'dispatched', dispatched_at = datetime('now') WHERE id = ?",
      [entry.id]
    );

    // Check if run is complete (no more pending)
    var remaining = agentdesk.db.query(
      "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND status = 'pending'",
      [entry.run_id]
    );
    if (remaining.length > 0 && remaining[0].cnt === 0) {
      // #145: If unified-thread run is done, kill the shared tmux session
      var runInfo = agentdesk.db.query(
        "SELECT unified_thread_id, unified_thread_channel_id FROM auto_queue_runs WHERE id = ?",
        [entry.run_id]
      );
      if (runInfo.length > 0 && runInfo[0].unified_thread_id) {
        var channelId = runInfo[0].unified_thread_channel_id;
        if (channelId) {
          agentdesk.log.info("[auto-queue] Unified-thread run " + entry.run_id + " complete — requesting tmux cleanup for channel " + channelId);
          // Mark a kv_meta flag for the Rust runtime to pick up and kill the session
          agentdesk.db.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
            ["kill_unified_thread:" + channelId, entry.run_id]
          );
        }
      }

      agentdesk.db.execute(
        "UPDATE auto_queue_runs SET status = 'completed', completed_at = datetime('now') WHERE id = ?",
        [entry.run_id]
      );
    }
  } catch (e) {
    agentdesk.log.warn("[auto-queue] dispatch failed for " + entry.kanban_card_id + ", will retry on next tick: " + e);
  }
}

agentdesk.registerPolicy(autoQueue);
