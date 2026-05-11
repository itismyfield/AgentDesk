module.exports = function attachReviewAutoAccept(timeouts, helpers) {
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

  timeouts._section_E = function() {
      // ─── [E] 자동-수용 결정 타임아웃 (suggestion_pending 15분) ──
      // Auto-accept: same effect as manual review-decision accept
      // (status → rework target, review_status → rework_pending, create rework dispatch)
      var eCfg = agentdesk.pipeline.getConfig();
      var eInitial = agentdesk.pipeline.kickoffState(eCfg);
      var eInProgress = agentdesk.pipeline.nextGatedTarget(eInitial, eCfg);
      var eReview = agentdesk.pipeline.nextGatedTarget(eInProgress, eCfg);
      var eReworkTarget = agentdesk.pipeline.nextGatedTargetWithGate(eReview, "review_rework", eCfg) || eInProgress;
      var staleSuggestions = agentdesk.db.query(
        "SELECT id, assigned_agent_id, title FROM kanban_cards " +
        "WHERE status = ? AND review_status = 'suggestion_pending' " +
        "AND suggestion_pending_at IS NOT NULL AND suggestion_pending_at < datetime('now', '-15 minutes') " +
        "AND assigned_agent_id IS NOT NULL " +
        "ORDER BY suggestion_pending_at ASC LIMIT 50",
        [eReview]
      );
      var aggregateNeeded = false;
      for (var s = 0; s < staleSuggestions.length; s++) {
        var sc = staleSuggestions[s];
        if (sc.assigned_agent_id) {
          // Try dispatch creation FIRST — only transition on success
          try {
            agentdesk.dispatch.create(
              sc.id,
              sc.assigned_agent_id,
              "rework",
              "[Rework] " + (sc.title || sc.id)
            );
            // Dispatch succeeded — now transition to rework target + rework_pending
            agentdesk.kanban.setStatus(sc.id, eReworkTarget);
            agentdesk.kanban.setReviewStatus(sc.id, "rework_pending", {suggestion_pending_at: null});
            // #119: Record tuning outcome (auto-accept = true_positive) BEFORE transition clears last_verdict
            var reviewState = agentdesk.db.query(
              "SELECT review_round, last_verdict FROM card_review_state WHERE card_id = ?",
              [sc.id]
            );
            if (reviewState.length > 0) {
              var rs = reviewState[0];
              // Get finding categories from last completed review dispatch
              var lastReview = agentdesk.db.query(
                "SELECT result FROM task_dispatches WHERE kanban_card_id = ? AND dispatch_type = 'review' AND status = 'completed' ORDER BY rowid DESC LIMIT 1",
                [sc.id]
              );
              var findingCats = null;
              if (lastReview.length > 0 && lastReview[0].result) {
                try {
                  var parsed = JSON.parse(lastReview[0].result);
                  if (parsed.items) {
                    findingCats = JSON.stringify(parsed.items.map(function(it) { return it.category || "unknown"; }));
                  }
                } catch(e) {}
              }
              agentdesk.db.execute(
                "INSERT INTO review_tuning_outcomes (card_id, dispatch_id, review_round, verdict, decision, outcome, finding_categories) " +
                "VALUES (?, NULL, ?, ?, 'auto_accept', 'true_positive', ?)",
                [sc.id, rs.review_round || null, rs.last_verdict || "unknown", findingCats]
              );
              agentdesk.log.info("[review-tuning] #119 recorded true_positive (auto-accept): card=" + sc.id);
              aggregateNeeded = true;
            }
            // #117: sync canonical review state
            agentdesk.reviewState.sync(sc.id, "rework_pending", { last_decision: "auto_accept" });
            agentdesk.log.warn("[timeout] Auto-accepted suggestions for card " + sc.id + " — rework dispatch created");
          } catch (e) {
            var autoAcceptReason = "Auto-accept rework dispatch failed: " + e;
            escalateToManualIntervention(sc.id, autoAcceptReason, { review: true });
            agentdesk.log.error("[timeout] Failed to create rework dispatch for " + sc.id + ": " + e + " → dilemma_pending");
          }
        } else {
          agentdesk.log.warn("[timeout] Auto-accepted card " + sc.id + " but no agent assigned — no rework dispatch");
        }
      }
      // #119: Trigger re-aggregation after the batch so all newly inserted
      // outcomes are visible while still bounding this JS path to one POST.
      if (aggregateNeeded) {
        try {
          var aggPort = agentdesk.config.get("server_port");
          if (aggPort) {
            agentdesk.http.post("http://127.0.0.1:" + aggPort + "/api/reviews/tuning/aggregate", {});
          }
        } catch (aggErr) {
          agentdesk.log.warn("[review-tuning] aggregate trigger failed (non-fatal): " + aggErr);
        }
      }
    };
};
