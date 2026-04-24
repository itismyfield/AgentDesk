module.exports = function attachReviewTimeouts(timeouts, helpers) {
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

  timeouts._section_C = function() {
      // ─── [C] 스테일 리뷰 (dispatch 완료인데 verdict 없음) ──
      var cCfg = agentdesk.pipeline.getConfig();
      var cInitial = agentdesk.pipeline.kickoffState(cCfg);
      var cInProgress = agentdesk.pipeline.nextGatedTarget(cInitial, cCfg);
      var cReview = agentdesk.pipeline.nextGatedTarget(cInProgress, cCfg);
      var staleReviews = agentdesk.db.query(
        "SELECT kc.id as card_id " +
        "FROM kanban_cards kc " +
        "JOIN task_dispatches td ON td.kanban_card_id = kc.id " +
        "WHERE kc.status = ? AND COALESCE(kc.review_status, 'reviewing') = 'reviewing' " +
        "AND td.dispatch_type = 'review' AND td.status IN ('completed', 'failed') " +
        "AND kc.review_entered_at IS NOT NULL AND kc.review_entered_at < datetime('now', '-30 minutes') " +
        "AND NOT EXISTS (SELECT 1 FROM task_dispatches td2 WHERE td2.kanban_card_id = kc.id " +
        "AND td2.dispatch_type IN ('review', 'review-decision') AND td2.status = 'pending')",
        [cReview]
      );
      for (var k = 0; k < staleReviews.length; k++) {
        var staleReviewReason = "stale review — dispatch 완료 30분+ verdict 없음";
        escalateToManualIntervention(staleReviews[k].card_id, staleReviewReason, { review: true });
        agentdesk.log.warn("[timeout] Stale review → dilemma_pending: card " + staleReviews[k].card_id);
      }
    };

  timeouts._section_D = function() {
      // ─── [D] DoD 대기 타임아웃 (15분) ──────────────────────
      var dCfg = agentdesk.pipeline.getConfig();
      var dInitial = agentdesk.pipeline.kickoffState(dCfg);
      var dInProgress = agentdesk.pipeline.nextGatedTarget(dInitial, dCfg);
      var dReview = agentdesk.pipeline.nextGatedTarget(dInProgress, dCfg);
      var stuckDod = agentdesk.db.query(
        "SELECT id FROM kanban_cards " +
        "WHERE status = ? AND review_status = 'awaiting_dod' " +
        "AND awaiting_dod_at IS NOT NULL AND awaiting_dod_at < datetime('now', '-15 minutes')",
        [dReview]
      );
      for (var d = 0; d < stuckDod.length; d++) {
        var dodReason = "DoD 대기 15분 초과";
        escalateToManualIntervention(stuckDod[d].id, dodReason, { review: true });
        agentdesk.log.warn("[timeout] DoD await timeout → dilemma_pending: card " + stuckDod[d].id);
      }
    };

  timeouts._section_N = function() {
      var nCfg = agentdesk.pipeline.getConfig();
      var nInitial = agentdesk.pipeline.kickoffState(nCfg);
      var nInProgress = agentdesk.pipeline.nextGatedTarget(nInitial, nCfg);
      var nReview = agentdesk.pipeline.nextGatedTarget(nInProgress, nCfg);
      if (!nReview) return;

      var orphanReviews = agentdesk.db.query(
        "SELECT kc.id, kc.title, kc.github_issue_number, kc.assigned_agent_id " +
        "FROM kanban_cards kc " +
        "WHERE kc.status = ? " +
        "AND COALESCE(kc.review_status, 'reviewing') = 'reviewing' " +
        "AND kc.review_entered_at IS NOT NULL " +
        "AND kc.review_entered_at < datetime('now', '-5 minutes') " +
        "AND NOT EXISTS (" +
        "  SELECT 1 FROM task_dispatches td " +
        "  WHERE td.kanban_card_id = kc.id " +
        "  AND td.dispatch_type IN ('review', 'review-decision', 'e2e-test') " +
        "  AND td.status IN ('pending', 'dispatched')" +
        ") " +
        "AND NOT EXISTS (" +
        "  SELECT 1 FROM task_dispatches td " +
        "  WHERE td.kanban_card_id = kc.id " +
        "  AND td.dispatch_type = 'review' " +
        "  AND td.status = 'completed' " +
        "  AND COALESCE(td.completed_at, td.updated_at) >= datetime('now', '-1 minute')" +
        ")",
        [nReview]
      );

      var protectedE2EReviews = agentdesk.db.query(
        "SELECT kc.id, kc.title, kc.github_issue_number, td.id AS dispatch_id, td.status AS dispatch_status " +
        "FROM kanban_cards kc " +
        "JOIN task_dispatches td ON td.kanban_card_id = kc.id " +
        "WHERE kc.status = ? " +
        "AND kc.review_entered_at IS NOT NULL " +
        "AND kc.review_entered_at < datetime('now', '-5 minutes') " +
        "AND td.dispatch_type = 'e2e-test' " +
        "AND td.status IN ('pending', 'dispatched')",
        [nReview]
      );

      for (var p = 0; p < protectedE2EReviews.length; p++) {
        var pc = protectedE2EReviews[p];
        agentdesk.log.info("[timeout] Orphan review guard: card " + pc.id +
          " (#" + (pc.github_issue_number || "?") + ") keeps review state because e2e-test dispatch " +
          pc.dispatch_id + " is still " + pc.dispatch_status);
      }

      // Orphan review = review state with no active dispatch after 5 min.
      // Instead of reimplementing OnReviewEnter safeguards, escalate to
      // review(dilemma_pending) so PMD can decide the correct action.
      // This avoids partial policy reimplementation (R1/R2 review feedback).
      for (var n = 0; n < orphanReviews.length; n++) {
        var oc = orphanReviews[n];
        agentdesk.log.warn("[timeout] Orphan review detected: card " + oc.id +
          " (#" + (oc.github_issue_number || "?") + ") in review with no active dispatch → dilemma_pending");

        escalateToManualIntervention(oc.id, "orphan review — dispatch 없음", { review: true });
      }
    };
};
