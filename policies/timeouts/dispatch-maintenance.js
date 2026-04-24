module.exports = function attachDispatchMaintenance(timeouts, helpers) {
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

  timeouts._section_F = function() {
      // ─── [F] 디스패치 큐 타임아웃 (100분) ──────────────────
      agentdesk.db.execute(
        "DELETE FROM dispatch_queue WHERE queued_at < datetime('now', '-100 minutes')"
      );
    };

  timeouts._section_G = function() {
      // ─── [G] 스테일 디스패치 정리 (24시간) ──────────────────
      var gCfg = agentdesk.pipeline.getConfig();
      var staleDispatches = agentdesk.db.query(
        "SELECT id, kanban_card_id FROM task_dispatches WHERE status IN ('pending','dispatched') AND created_at < datetime('now', '-24 hours')"
      );
      for (var sd = 0; sd < staleDispatches.length; sd++) {
        var sfResult = agentdesk.dispatch.markFailed(staleDispatches[sd].id, "Stale dispatch auto-failed after 24h");
        if (sfResult.rows_affected === 0) {
          agentdesk.log.info("[timeout] Dispatch " + staleDispatches[sd].id + " already terminal, skipping");
          continue;
        }
        if (staleDispatches[sd].kanban_card_id) {
          var card = agentdesk.kanban.getCard(staleDispatches[sd].kanban_card_id);
          if (card && !agentdesk.pipeline.isTerminal(card.status, gCfg)) {
            escalateToManualIntervention(
              staleDispatches[sd].kanban_card_id,
              "Stale dispatch auto-failed after 24h",
              { review: card.status === "review" }
            );
          }
        }
        agentdesk.log.warn("[timeout] Dispatch " + staleDispatches[sd].id + " stale 24h → failed");
      }
    };

  timeouts._section_H = function() {
      // ─── [H] Stale dispatched 큐 엔트리 진행 ───────────────
      var hCfg = agentdesk.pipeline.getConfig();
      var hInitial = agentdesk.pipeline.kickoffState(hCfg);
      var hInProgress = agentdesk.pipeline.nextGatedTarget(hInitial, hCfg);
      var staleQueueEntries = agentdesk.db.query(
        "SELECT dq.id FROM dispatch_queue dq " +
        "JOIN kanban_cards kc ON kc.id = dq.kanban_card_id " +
        "WHERE dq.status = 'dispatched' AND kc.status NOT IN (?, ?)",
        [hInitial, hInProgress]
      );
      for (var se = 0; se < staleQueueEntries.length; se++) {
        agentdesk.db.execute(
          "DELETE FROM dispatch_queue WHERE id = ?",
          [staleQueueEntries[se].id]
        );
      }
    };

  timeouts._section_I0 = function() {
      // ─── [I-0] 미전송 디스패치 알림 복구 ──────────────────────
      // pending dispatch가 2분 이상 됐는데 알림이 안 갔을 수 있음 → 재전송
      var unnotifiedDispatches = agentdesk.db.query(
        "SELECT td.id, td.dispatch_type, td.to_agent_id, kc.title, kc.github_issue_url, kc.github_issue_number, td.kanban_card_id " +
        "FROM task_dispatches td " +
        "JOIN kanban_cards kc ON td.kanban_card_id = kc.id " +
        "WHERE td.status = 'pending' " +
        "AND td.created_at < datetime('now', '-2 minutes') " +
        "AND NOT EXISTS (SELECT 1 FROM kv_meta WHERE key = 'dispatch_notified:' || td.id) " +
        "AND NOT EXISTS (SELECT 1 FROM kv_meta WHERE key = 'dispatch_reserving:' || td.id) " +
        "AND NOT EXISTS (SELECT 1 FROM dispatch_outbox WHERE dispatch_id = td.id AND status IN ('pending', 'processing', 'failed'))"
      );
      for (var un = 0; un < unnotifiedDispatches.length; un++) {
        var ud = unnotifiedDispatches[un];

        // Re-enqueue into dispatch_outbox so the Rust outbox worker handles delivery
        // with proper two-phase guard and retry/backoff (#209).
        // Do NOT send directly via message.queue — that bypasses the delivery guarantee.
        agentdesk.db.execute(
          "INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title, status) " +
          "VALUES (?1, 'notify', ?2, ?3, ?4, 'pending')",
          [ud.id, ud.to_agent_id, ud.kanban_card_id || "", ud.title]
        );
        agentdesk.log.info("[notify-recovery] Dispatch " + ud.id + " re-enqueued to dispatch_outbox");
      }
    };

  timeouts._section_J = function() {
      // ─── [J] Failed 디스패치 자동 재시도 (30초 쿨다운, 최대 10회) ──
      // failed 상태의 디스패치 중 retry_count < 10이고 30초+ 경과한 것을 재시도.
      // 실제 cadence는 onTick 60초 간격이므로 ~60-90초.
      // 10분 윈도우 제거 — latest_dispatch_id 체크로 stale 방지 충분.
      var jCfg = agentdesk.pipeline.getConfig();
      var jInitial = agentdesk.pipeline.kickoffState(jCfg);
      var jInProgress = agentdesk.pipeline.nextGatedTarget(jInitial, jCfg);
      var failedForRetry = agentdesk.db.query(
        "SELECT td.id, td.kanban_card_id, td.to_agent_id, td.dispatch_type, td.title, " +
        "COALESCE(td.retry_count, 0) as retry_count, kc.github_issue_url, kc.github_issue_number " +
        "FROM task_dispatches td " +
        "JOIN kanban_cards kc ON kc.id = td.kanban_card_id " +
        "WHERE td.status = 'failed' " +
        "AND COALESCE(td.retry_count, 0) < " + MAX_DISPATCH_RETRIES + " " +
        "AND td.updated_at < datetime('now', '-30 seconds') " +
        "AND kc.latest_dispatch_id = td.id " +
        "AND kc.status IN (?, ?)",
        [jInitial, jInProgress]
      );
      for (var jr = 0; jr < failedForRetry.length; jr++) {
        var fd = failedForRetry[jr];
        var newRetryCount = fd.retry_count + 1;
        try {
          var newDispatchId = agentdesk.dispatch.create(
            fd.kanban_card_id,
            fd.to_agent_id,
            fd.dispatch_type || "implementation",
            fd.title
          );
          // 새 디스패치에 retry_count 기록
          agentdesk.dispatch.setRetryCount(newDispatchId, newRetryCount);
          agentdesk.log.info("[retry] Auto-retry dispatch for card " + fd.kanban_card_id +
            " — attempt " + newRetryCount + "/" + MAX_DISPATCH_RETRIES +
            " (old: " + fd.id + " → new: " + newDispatchId + ")");

          // Discord notification is handled by the dispatch outbox system (#209).
          // agentdesk.dispatch.create() enqueues an outbox entry via queue_dispatch_notify,
          // and the outbox worker delivers with two-phase guard (no duplicate risk).
        } catch (e) {
          agentdesk.log.error("[retry] Failed to create retry dispatch for card " +
            fd.kanban_card_id + ": " + e);
          // Don't block the card on transient retry failure — leave status as-is
          // so the next tick can retry. Only log the error.
        }
      }
    };
};
