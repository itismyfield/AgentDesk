module.exports = function attachOrphanDispatch(timeouts, helpers) {
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

  timeouts._section_K = function() {
      // ─── [K] 고아 디스패치 복구 (5분) ────────────────────────
      // Card가 in_progress이고 latest dispatch가 pending인데
      // 해당 dispatch_id를 가진 working 세션이 없는 경우 = 고아 디스패치.
      // dcserver 재시작 등으로 세션-디스패치 연결이 끊긴 상태.
      // dispatch를 failed로 마크하고 card를 dispatchable 상태로 되돌려 안전하게 재디스패치한다.

      // Grace period: 서버 부팅 후 10분간은 orphan 판정 유예.
      // 재시작 직후 세션이 아직 복원되지 않은 상태를 orphan으로 오판하는 것을 방지.
      var bootRows = agentdesk.db.query(
        "SELECT value FROM kv_meta WHERE key = 'server_boot_at'"
      );
      if (bootRows.length > 0) {
        var bootAt = new Date(bootRows[0].value + "Z");
        var bootElapsedMin = (Date.now() - bootAt.getTime()) / 60000;
        if (bootElapsedMin < 10) {
          return;
        }
      }

      var kCfg = agentdesk.pipeline.getConfig();
      var kInitial = agentdesk.pipeline.kickoffState(kCfg);
      var kInProgress = agentdesk.pipeline.nextGatedTarget(kInitial, kCfg);
      var kReview = agentdesk.pipeline.nextGatedTarget(kInProgress, kCfg);
      var orphanedDispatches = agentdesk.db.query(
        "SELECT td.id as dispatch_id, td.kanban_card_id, td.dispatch_type " +
        "FROM task_dispatches td " +
        "JOIN kanban_cards kc ON kc.id = td.kanban_card_id " +
        "WHERE kc.status = ? " +
        "AND td.status = 'pending' " +
        "AND kc.latest_dispatch_id = td.id " +
        "AND td.dispatch_type IN ('implementation', 'rework') " +
        "AND td.created_at < datetime('now', '-5 minutes') " +
        "AND NOT EXISTS (" +
        "  SELECT 1 FROM sessions s " +
        "  WHERE s.active_dispatch_id = td.id AND s.status IN ('turn_active', 'working')" +
        ")",
        [kInProgress]
      );
      for (var op = 0; op < orphanedDispatches.length; op++) {
        var od = orphanedDispatches[op];
        try {
          var decision = agentdesk.runtime.emitSignal("OrphanCandidate", {
            dispatch_id: od.dispatch_id,
            card_id: od.kanban_card_id,
            dispatch_type: od.dispatch_type,
            detected_from: "timeouts._section_K"
          });
          if (decision.executed) {
            agentdesk.log.warn("[orphan-recovery] Supervisor resumed orphaned dispatch " +
              od.dispatch_id + " → card " + od.kanban_card_id + " → " + kInitial);
          } else {
            agentdesk.log.info("[orphan-recovery] Supervisor skipped " + od.dispatch_id +
              (decision.note ? " — " + decision.note : ""));
          }
        } catch (e) {
          agentdesk.log.error("[orphan-recovery] Supervisor emit failed for " + od.dispatch_id + ": " + e);
        }
      }
    };
};
