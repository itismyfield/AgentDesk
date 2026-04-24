module.exports = function attachCardTimeouts(timeouts, helpers) {
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

  timeouts._section_A = function() {
      // ─── [A] Requested 타임아웃 ─────────────────────
      // retry_count < 10이면 수동 판단 대신 failed만 마크 → [J]가 30초 후 재시도
      var aCfg = agentdesk.pipeline.getConfig();
      var aInitial = agentdesk.pipeline.kickoffState(aCfg);
      var requestedInterval = getTimeoutInterval("requested_timeout_min", 45);
      var staleRequested = agentdesk.db.query(
        "SELECT kc.id, kc.assigned_agent_id, kc.latest_dispatch_id, " +
        "COALESCE(td.retry_count, 0) as retry_count, td.dispatch_type " +
        "FROM kanban_cards kc " +
        "LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id " +
        "WHERE kc.status = ? AND kc.requested_at IS NOT NULL AND kc.requested_at < datetime('now', '" + requestedInterval + "')",
        [aInitial]
      );
      for (var i = 0; i < staleRequested.length; i++) {
        var rc = staleRequested[i];
        // #255: Skip cards without a dispatch — they are in preflight state,
        // waiting for auto-queue or tick to create a dispatch.
        if (!rc.latest_dispatch_id) {
          agentdesk.log.info("[timeout] Card " + rc.id + " in " + aInitial + " without dispatch — preflight, skipping timeout");
          continue;
        }
        // #256: Skip cards with consultation dispatch — consultation has its own
        // lifecycle via onDispatchCompleted; let it resolve naturally.
        if (rc.dispatch_type === "consultation") {
          agentdesk.log.info("[timeout] Card " + rc.id + " in " + aInitial + " with consultation dispatch — skipping timeout");
          continue;
        }
        // Dispatch를 failed로 — skip state changes if dispatch was already terminal
        if (rc.latest_dispatch_id) {
          var failResult = agentdesk.dispatch.markFailed(rc.latest_dispatch_id, "Timed out waiting for agent");
          if (failResult.rows_affected === 0) {
            agentdesk.log.info("[timeout] Card " + rc.id + " dispatch already terminal, skipping");
            continue;
          }
        }

        if (rc.retry_count < MAX_DISPATCH_RETRIES) {
          // 재시도 여유 있음 → card 상태 유지 (requested_at 갱신하여 [A] 재트리거 방지)
          // [J] 섹션에서 30초 후 자동 재시도
          agentdesk.db.execute(
            "UPDATE kanban_cards SET requested_at = datetime('now'), updated_at = datetime('now') WHERE id = ?",
            [rc.id]
          );
          agentdesk.log.warn("[timeout] Card " + rc.id + " requested timeout — retry " +
            rc.retry_count + "/" + MAX_DISPATCH_RETRIES + ", will auto-retry in 30s");
        } else {
          var requestedReason = "Timed out waiting for agent (" + MAX_DISPATCH_RETRIES + " retries exhausted)";
          escalateToManualIntervention(rc.id, requestedReason);
          agentdesk.log.warn("[timeout] Card " + rc.id + " " + aInitial + " timeout → manual intervention (" + MAX_DISPATCH_RETRIES + " retries exhausted)");
        }
      }
    };

  timeouts._section_B = function() {
      // ─── [B] In-Progress 스테일 ────────────────────
      var bCfg = agentdesk.pipeline.getConfig();
      var bInitial = agentdesk.pipeline.kickoffState(bCfg);
      var bInProgress = agentdesk.pipeline.nextGatedTarget(bInitial, bCfg);
      var inProgressInterval = getTimeoutInterval("in_progress_stale_min", 120);
      var staleInProgress = agentdesk.db.query(
        "SELECT kc.id FROM kanban_cards kc " +
        "LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id " +
        "WHERE kc.status = ? AND COALESCE(kc.blocked_reason, '') = '' AND " +
        latestCardActivityExpr("kc", "td") + " < datetime('now', '" + inProgressInterval + "')",
        [bInProgress]
      );
      for (var j = 0; j < staleInProgress.length; j++) {
        var staleMin = parseInt(agentdesk.config.get("in_progress_stale_min"), 10) || 120;
        var stalledReason = "Stalled: no activity for " + staleMin + "+ min";
        escalateToManualIntervention(staleInProgress[j].id, stalledReason);
        agentdesk.log.warn("[timeout] Card " + staleInProgress[j].id + " " + bInProgress + " stale → manual intervention");
      }
    };
};
