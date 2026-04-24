module.exports = function attachReconciliation(timeouts, helpers) {
  var sendDeadlockAlert = helpers.sendDeadlockAlert;
  var MAX_DISPATCH_RETRIES = helpers.MAX_DISPATCH_RETRIES;
  var getTimeoutInterval = helpers.getTimeoutInterval;
  var latestCardActivityExpr = helpers.latestCardActivityExpr;
  var parseLocalTimestampMs = helpers.parseLocalTimestampMs;
  var normalizedText = helpers.normalizedText;
  var parseSessionTmuxName = helpers.parseSessionTmuxName;
  var parseSessionChannelName = helpers.parseSessionChannelName;
  var parseParentChannelName = helpers.parseParentChannelName;
  var parseSessionThreadId = helpers.parseSessionThreadId;
  var loadAgentDirectory = helpers.loadAgentDirectory;
  var agentDisplayName = helpers.agentDisplayName;
  var findAgentById = helpers.findAgentById;
  var channelMatchesCandidate = helpers.channelMatchesCandidate;
  var findAgentByChannelValue = helpers.findAgentByChannelValue;
  var buildChannelTarget = helpers.buildChannelTarget;
  var resolveAgentNotifyTarget = helpers.resolveAgentNotifyTarget;
  var lookupDispatchTargetAgentId = helpers.lookupDispatchTargetAgentId;
  var lookupThreadTargetAgentId = helpers.lookupThreadTargetAgentId;
  var resolveSessionAgentContext = helpers.resolveSessionAgentContext;
  var backfillMissingSessionAgentIds = helpers.backfillMissingSessionAgentIds;
  var findRecentInflightForSession = helpers.findRecentInflightForSession;
  var inspectInflightProgress = helpers.inspectInflightProgress;
  var requestTurnWatchdogExtension = helpers.requestTurnWatchdogExtension;
  var _queuePMDecision = helpers._queuePMDecision;
  var _flushPMDecisions = helpers._flushPMDecisions;

  timeouts._section_R = function() {
      // ─── [R] Reconciliation: DB fallback dispatches that need hook chain ──
      // These dispatches were completed/failed via direct DB UPDATE (API retry exhausted).
      // We re-emit the OnDispatchCompleted payload so the full hook chain runs
      // (PM gate, DoD check, XP, review entry — same as normal complete_dispatch path).
      var reconcileKeys = agentdesk.db.query(
        "SELECT key, value FROM kv_meta WHERE key LIKE 'reconcile_dispatch:%'"
      );
      for (var r = 0; r < reconcileKeys.length; r++) {
        var dispatchId = reconcileKeys[r].value;
        agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [reconcileKeys[r].key]);
        agentdesk.log.info("[reconcile] Processing fallback dispatch " + dispatchId);
        // The dispatch is already completed/failed in DB.
        // Fire the same event that kanban-rules.js and review-automation.js listen to.
        // This is handled by the Rust engine — we can't re-emit hooks from JS.
        // Instead, call the same logic that onDispatchCompleted would:
        // 1. Read dispatch info
        var dispInfo = agentdesk.db.query(
          "SELECT id, kanban_card_id, to_agent_id, dispatch_type, chain_depth, status, result, context FROM task_dispatches WHERE id = ?",
          [dispatchId]
        );
        if (dispInfo.length === 0) continue;
        var di = dispInfo[0];
        if (!di.kanban_card_id) continue;
        if (di.status === "failed") {
          agentdesk.log.info("[reconcile] Dispatch " + dispatchId + " failed — no action needed");
          continue;
        }
        // 2. For completed dispatches, replay kanban-rules onDispatchCompleted logic
        var cards = agentdesk.db.query(
          "SELECT id, status, priority, assigned_agent_id, deferred_dod_json FROM kanban_cards WHERE id = ?",
          [di.kanban_card_id]
        );
        if (cards.length === 0) continue;
        var card = cards[0];
        var rCfg = agentdesk.pipeline.resolveForCard(card.id);
        var rInitial = agentdesk.pipeline.kickoffState(rCfg);
        var rInProgress = agentdesk.pipeline.nextGatedTarget(rInitial, rCfg);
        var rReview = agentdesk.pipeline.nextGatedTarget(rInProgress, rCfg);
        var rPending = rInitial;
        if (agentdesk.pipeline.isTerminal(card.status, rCfg)) continue;
        if (di.dispatch_type === "review" || di.dispatch_type === "review-decision") continue;
        if (di.dispatch_type === "rework") {
          agentdesk.kanban.setStatus(card.id, rReview);
          agentdesk.log.info("[reconcile] " + card.id + " rework done → " + rReview);
          continue;
        }
        // Implementation: run PM gate same as kanban-rules.js onDispatchCompleted
        var xpMap = { "low": 5, "medium": 10, "high": 18, "urgent": 30 };
        var xp = xpMap[card.priority] || 10;
        xp += Math.min(di.chain_depth || 0, 3) * 2;
        if (di.to_agent_id) {
          agentdesk.db.execute("UPDATE agents SET xp = xp + ? WHERE id = ?", [xp, di.to_agent_id]);
        }
        // Check skip_gate from dispatch context
        var dispatchContext = {};
        try { dispatchContext = JSON.parse(di.context || "{}"); } catch(e) {}
        var pmGateEnabled = agentdesk.config.get("pm_decision_gate_enabled");
        if (dispatchContext.skip_gate) {
          agentdesk.log.info("[reconcile] Skipped PM gate for card " + card.id + " (skip_gate flag)");
        } else if (pmGateEnabled !== false && pmGateEnabled !== "false") {
          var reasons = [];
          // Check 1: DoD completion
          // Format: { items: ["task1", "task2"], verified: ["task1"] }
          if (card.deferred_dod_json) {
            try {
              var dod = JSON.parse(card.deferred_dod_json);
              var items = dod.items || [];
              var verified = dod.verified || [];
              if (items.length > 0) {
                var unverified = 0;
                for (var di2 = 0; di2 < items.length; di2++) {
                  if (verified.indexOf(items[di2]) === -1) unverified++;
                }
                if (unverified > 0) reasons.push("DoD 미완료: " + (items.length - unverified) + "/" + items.length);
              }
            } catch (e) {}
          }
          // Minimum work duration heuristic intentionally removed to keep PM
          // escalation aligned with objective failure states only. Replay logic
          // must match kanban-rules.js and avoid false positives from unified
          // thread / turn-bridge completions.
          if (reasons.length > 0) {
            var dodOnly = reasons.length === 1 && reasons[0].indexOf("DoD 미완료") === 0;
            if (dodOnly) {
              agentdesk.kanban.setStatus(card.id, rReview);
              agentdesk.kanban.setReviewStatus(card.id, "awaiting_dod", {awaiting_dod_at: "now"});
              // #117: sync canonical review state
              agentdesk.reviewState.sync(card.id, "awaiting_dod");
              agentdesk.log.warn("[reconcile] Card " + card.id + " → " + rReview + "(awaiting_dod): " + reasons[0]);
              continue;
            }
            agentdesk.kanban.setStatus(card.id, rPending);
            agentdesk.kanban.setReviewStatus(card.id, null, {suggestion_pending_at: null});
            // #117: sync canonical review state
            agentdesk.reviewState.sync(card.id, "idle");
            agentdesk.log.warn("[reconcile] Card " + card.id + " → " + rPending + ": " + reasons.join("; "));
            // #231: Queue deduped PM notification (flushed at tick end)
            var cardTitle2 = agentdesk.db.query("SELECT title FROM kanban_cards WHERE id = ?", [card.id]);
            var t2 = cardTitle2.length > 0 ? cardTitle2[0].title : card.id;
            for (var ri = 0; ri < reasons.length; ri++) {
              _queuePMDecision(card.id, t2, reasons[ri]);
            }
            continue;
          }
        }
        agentdesk.kanban.setStatus(card.id, rReview);
        agentdesk.log.info("[reconcile] " + card.id + " implementation done → " + rReview + " (via DB fallback)");
      }
    };
};
