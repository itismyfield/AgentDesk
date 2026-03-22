/**
 * timeouts.js — ADK Policy: Timeout & Stale Detection
 * priority: 100
 *
 * Hook: onTick (1분 간격 — Rust 서버에서 주기적으로 fire)
 *
 * [A] Requested 타임아웃 (45분) → pending_decision
 * [B] In-Progress 스테일 (2시간) → blocked
 * [C] 스테일 리뷰 (dispatch 완료인데 verdict 없음) → pending_decision
 * [D] DoD 대기 타임아웃 (15분) → pending_decision
 * [E] 자동-수용 결정 타임아웃 → auto-accept + rework
 * [F] 디스패치 큐 타임아웃 (100분) → 제거
 * [G] 스테일 디스패치 정리 (24시간) → failed
 * [H] Stale dispatched 큐 엔트리 진행
 */

// Send notification via notify bot (system alerts, not agent communication)
function sendNotifyAlert(channelTarget, message) {
  try {
    var port = agentdesk.config.get("health_port") || 8798;
    agentdesk.http.post("http://127.0.0.1:" + port + "/api/send", {
      target: channelTarget,
      content: message,
      bot: "notify",
      source: "timeouts"
    });
  } catch (e) {
    agentdesk.log.warn("[notify] Alert send failed: " + e);
  }
}

// Get PMD channel for alerts
function getPMDChannel() {
  return "channel:" + (agentdesk.config.get("pmd_channel_id") || "1478652416533463101");
}

var timeouts = {
  name: "timeouts",
  priority: 100,

  onTick: function() {
    // ─── [A] Requested 타임아웃 (45분) ─────────────────────
    var staleRequested = agentdesk.db.query(
      "SELECT id, assigned_agent_id, latest_dispatch_id FROM kanban_cards " +
      "WHERE status = 'requested' AND updated_at < datetime('now', '-45 minutes')"
    );
    for (var i = 0; i < staleRequested.length; i++) {
      // Dispatch를 failed로
      if (staleRequested[i].latest_dispatch_id) {
        agentdesk.db.execute(
          "UPDATE task_dispatches SET status = 'failed', result ='Timed out waiting for agent', updated_at = datetime('now') WHERE id = ? AND status IN ('pending','dispatched')",
          [staleRequested[i].latest_dispatch_id]
        );
      }
      // 카드는 pending_decision으로 (PMD가 판단)
      agentdesk.kanban.setStatus(staleRequested[i].id, "pending_decision");
      agentdesk.db.execute(
        "UPDATE kanban_cards SET blocked_reason = 'Timed out waiting for agent acceptance' WHERE id = ?",
        [staleRequested[i].id]
      );
      agentdesk.log.warn("[timeout] Card " + staleRequested[i].id + " requested timeout → pending_decision");
      // PMD에게 결정 요청 (announce bot — 에이전트가 반응)
      var cardInfo = agentdesk.db.query(
        "SELECT title, github_issue_url, assigned_agent_id FROM kanban_cards WHERE id = ?",
        [staleRequested[i].id]
      );
      var cardTitle = (cardInfo.length > 0) ? cardInfo[0].title : staleRequested[i].id;
      var cardUrl = (cardInfo.length > 0 && cardInfo[0].github_issue_url) ? "\n" + cardInfo[0].github_issue_url : "";
      var assignee = (cardInfo.length > 0 && cardInfo[0].assigned_agent_id) ? cardInfo[0].assigned_agent_id : "미배정";
      try {
        var port = agentdesk.config.get("health_port") || 8798;
        agentdesk.http.post("http://127.0.0.1:" + port + "/api/send", {
          target: getPMDChannel(),
          content: "[ADK → PMD] ⏰ 타임아웃 결정 요청\n\n" +
            "카드: " + cardTitle + "\n" +
            "담당: " + assignee + "\n" +
            "사유: 45분간 에이전트 무응답\n\n" +
            "다음 중 하나를 선택해주세요:\n" +
            "• 재디스패치 → 같은/다른 에이전트에게 재전송\n" +
            "• 백로그 → 우선순위 재조정\n" +
            "• 취소 → 이슈 닫기" + cardUrl,
          source: "timeouts",
          bot: "announce"
        });
      } catch(e) {
        agentdesk.log.warn("[timeout] PMD decision request failed: " + e);
      }
    }

    // ─── [B] In-Progress 스테일 (2시간) ────────────────────
    var staleInProgress = agentdesk.db.query(
      "SELECT id FROM kanban_cards WHERE status = 'in_progress' AND updated_at < datetime('now', '-2 hours')"
    );
    for (var j = 0; j < staleInProgress.length; j++) {
      agentdesk.kanban.setStatus(staleInProgress[j].id, "blocked");
      agentdesk.db.execute(
        "UPDATE kanban_cards SET blocked_reason = 'Stalled: no activity for 2+ hours' WHERE id = ?",
        [staleInProgress[j].id]
      );
      agentdesk.log.warn("[timeout] Card " + staleInProgress[j].id + " in_progress stale → blocked");
      // PMD에게 결정 요청 (announce bot)
      var stalledInfo = agentdesk.db.query(
        "SELECT title, github_issue_url, assigned_agent_id FROM kanban_cards WHERE id = ?",
        [staleInProgress[j].id]
      );
      var stalledTitle = (stalledInfo.length > 0) ? stalledInfo[0].title : staleInProgress[j].id;
      var stalledUrl = (stalledInfo.length > 0 && stalledInfo[0].github_issue_url) ? "\n" + stalledInfo[0].github_issue_url : "";
      var stalledAssignee = (stalledInfo.length > 0 && stalledInfo[0].assigned_agent_id) ? stalledInfo[0].assigned_agent_id : "미배정";
      try {
        var port = agentdesk.config.get("health_port") || 8798;
        agentdesk.http.post("http://127.0.0.1:" + port + "/api/send", {
          target: getPMDChannel(),
          content: "[ADK → PMD] ⚠️ 정체 카드 결정 요청\n\n" +
            "카드: " + stalledTitle + "\n" +
            "담당: " + stalledAssignee + "\n" +
            "사유: 2시간 이상 진행 없음 → blocked\n\n" +
            "다음 중 하나를 선택해주세요:\n" +
            "• 재디스패치 → 에이전트에게 재전송\n" +
            "• 백로그 → 우선순위 재조정\n" +
            "• 취소 → 이슈 닫기" + stalledUrl,
          source: "timeouts",
          bot: "announce"
        });
      } catch(e) {
        agentdesk.log.warn("[timeout] PMD stalled request failed: " + e);
      }
    }

    // ─── [C] 스테일 리뷰 (dispatch 완료인데 verdict 없음) ──
    var staleReviews = agentdesk.db.query(
      "SELECT kc.id as card_id " +
      "FROM kanban_cards kc " +
      "JOIN task_dispatches td ON td.kanban_card_id = kc.id " +
      "WHERE kc.status = 'review' AND kc.review_status = 'reviewing' " +
      "AND td.dispatch_type = 'review' AND td.status IN ('completed', 'failed') " +
      "AND kc.updated_at < datetime('now', '-30 minutes')"
    );
    for (var k = 0; k < staleReviews.length; k++) {
      agentdesk.kanban.setStatus(staleReviews[k].card_id, "pending_decision");
      agentdesk.db.execute("UPDATE kanban_cards SET review_status = NULL WHERE id = ?", [staleReviews[k].card_id]);
      agentdesk.log.warn("[timeout] Stale review → pending_decision: card " + staleReviews[k].card_id);
    }

    // ─── [D] DoD 대기 타임아웃 (15분) ──────────────────────
    var stuckDod = agentdesk.db.query(
      "SELECT id FROM kanban_cards " +
      "WHERE status = 'review' AND review_status = 'awaiting_dod' " +
      "AND updated_at < datetime('now', '-15 minutes')"
    );
    for (var d = 0; d < stuckDod.length; d++) {
      agentdesk.kanban.setStatus(stuckDod[d].id, "pending_decision");
      agentdesk.db.execute("UPDATE kanban_cards SET review_status = NULL WHERE id = ?", [stuckDod[d].id]);
      agentdesk.log.warn("[timeout] DoD await timeout → pending_decision: card " + stuckDod[d].id);
    }

    // ─── [E] 자동-수용 결정 타임아웃 (suggestion_pending 15분) ──
    var staleSuggestions = agentdesk.db.query(
      "SELECT id FROM kanban_cards " +
      "WHERE review_status = 'suggestion_pending' " +
      "AND updated_at < datetime('now', '-15 minutes')"
    );
    for (var s = 0; s < staleSuggestions.length; s++) {
      // Auto-accept: suggestion_pending → rework_pending
      agentdesk.db.execute(
        "UPDATE kanban_cards SET review_status = 'rework_pending', updated_at = datetime('now') WHERE id = ?",
        [staleSuggestions[s].id]
      );
      agentdesk.log.warn("[timeout] Auto-accepted suggestions for card " + staleSuggestions[s].id);
    }

    // ─── [F] 디스패치 큐 타임아웃 (100분) ──────────────────
    agentdesk.db.execute(
      "DELETE FROM dispatch_queue WHERE queued_at < datetime('now', '-100 minutes')"
    );

    // ─── [G] 스테일 디스패치 정리 (24시간) ──────────────────
    var staleDispatches = agentdesk.db.query(
      "SELECT id, kanban_card_id FROM task_dispatches WHERE status IN ('pending','dispatched') AND created_at < datetime('now', '-24 hours')"
    );
    for (var sd = 0; sd < staleDispatches.length; sd++) {
      agentdesk.db.execute(
        "UPDATE task_dispatches SET status = 'failed', result ='Stale dispatch auto-failed after 24h', updated_at = datetime('now') WHERE id = ?",
        [staleDispatches[sd].id]
      );
      if (staleDispatches[sd].kanban_card_id) {
        var card = agentdesk.kanban.getCard(staleDispatches[sd].kanban_card_id);
        if (card && card.status !== "done") {
          agentdesk.kanban.setStatus(staleDispatches[sd].kanban_card_id, "pending_decision");
          agentdesk.db.execute(
            "UPDATE kanban_cards SET blocked_reason = 'Stale dispatch auto-failed after 24h' WHERE id = ?",
            [staleDispatches[sd].kanban_card_id]
          );
        }
      }
      agentdesk.log.warn("[timeout] Dispatch " + staleDispatches[sd].id + " stale 24h → failed");
    }

    // ─── [H] Stale dispatched 큐 엔트리 진행 ───────────────
    var staleQueueEntries = agentdesk.db.query(
      "SELECT dq.id FROM dispatch_queue dq " +
      "JOIN kanban_cards kc ON kc.id = dq.kanban_card_id " +
      "WHERE dq.status = 'dispatched' AND kc.status NOT IN ('requested', 'in_progress')"
    );
    for (var se = 0; se < staleQueueEntries.length; se++) {
      agentdesk.db.execute(
        "DELETE FROM dispatch_queue WHERE id = ?",
        [staleQueueEntries[se].id]
      );
    }

    // ─── [I-0] 미전송 디스패치 알림 복구 ──────────────────────
    // pending dispatch가 2분 이상 됐는데 알림이 안 갔을 수 있음 → 재전송
    var unnotifiedDispatches = agentdesk.db.query(
      "SELECT td.id, td.dispatch_type, td.to_agent_id, kc.title, kc.github_issue_url, kc.github_issue_number " +
      "FROM task_dispatches td " +
      "JOIN kanban_cards kc ON td.kanban_card_id = kc.id " +
      "WHERE td.status = 'pending' " +
      "AND td.created_at < datetime('now', '-2 minutes') " +
      "AND td.id NOT IN (SELECT value FROM kv_meta WHERE key LIKE 'dispatch_notified:%')"
    );
    for (var un = 0; un < unnotifiedDispatches.length; un++) {
      var ud = unnotifiedDispatches[un];
      // Mark as notified to avoid spam
      agentdesk.db.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
        ["dispatch_notified:" + ud.id, ud.id]
      );

      // Determine channel
      var agentChannel = agentdesk.db.query(
        "SELECT discord_channel_id, discord_channel_alt FROM agents WHERE id = ?",
        [ud.to_agent_id]
      );
      if (agentChannel.length === 0) continue;

      var useAlt = (ud.dispatch_type === "review" || ud.dispatch_type === "review-decision");
      var channelId = useAlt ? agentChannel[0].discord_channel_alt : agentChannel[0].discord_channel_id;
      if (!channelId) continue;

      var issueLink = ud.github_issue_url
        ? "\n[" + ud.title + " #" + ud.github_issue_number + "](<" + ud.github_issue_url + ">)"
        : "";
      var prefix = useAlt
        ? "DISPATCH:" + ud.id + " - " + ud.title + "\n⚠️ 검토 전용 — 작업 착수 금지\n코드 리뷰만 수행하고 GitHub 이슈에 코멘트로 피드백해주세요."
        : "DISPATCH:" + ud.id + " - " + ud.title;

      try {
        var port = agentdesk.config.get("health_port") || 8798;
        agentdesk.http.post("http://127.0.0.1:" + port + "/api/send", {
          target: "channel:" + channelId,
          content: prefix + issueLink,
          source: "timeouts",
          bot: "announce"
        });
        agentdesk.log.info("[notify-recovery] Resent dispatch notification: " + ud.id + " → " + channelId);
      } catch(e) {
        agentdesk.log.warn("[notify-recovery] Failed: " + e);
      }
    }

    // ─── [I] 턴 데드락 감지 (15분) ──────────────────────────
    var DEADLOCK_MINUTES = 15;
    var MAX_EXTENSIONS = 3;
    var staleSessions = agentdesk.db.query(
      "SELECT session_key, agent_id, active_dispatch_id, last_heartbeat " +
      "FROM sessions WHERE status = 'working' " +
      "AND last_heartbeat < datetime('now', '-" + DEADLOCK_MINUTES + " minutes')"
    );
    for (var dl = 0; dl < staleSessions.length; dl++) {
      var sess = staleSessions[dl];
      var deadlockKey = "deadlock_check:" + sess.session_key;

      // Check extension count
      var extRecord = agentdesk.db.query(
        "SELECT value FROM kv_meta WHERE key = ?", [deadlockKey]
      );
      var extensions = 0;
      if (extRecord.length > 0) {
        try { extensions = parseInt(extRecord[0].value) || 0; } catch(e) {}
      }

      if (extensions >= MAX_EXTENSIONS) {
        // Max extensions reached — force alert for manual intervention
        agentdesk.log.warn("[deadlock] Session " + sess.session_key +
          " — max extensions (" + MAX_EXTENSIONS + ") reached. Manual intervention needed.");
        sendNotifyAlert(getPMDChannel(),
          "🔴 [Deadlock] " + sess.agent_id + " 세션 " + sess.session_key +
          " — " + (DEADLOCK_MINUTES * (MAX_EXTENSIONS + 1)) + "분 이상 무응답. 수동 개입 필요.");
        // Reset counter to avoid spam
        agentdesk.db.execute(
          "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, '-1')",
          [deadlockKey]
        );
      } else if (extensions >= 0) {
        // Extend timeout + alert
        agentdesk.db.execute(
          "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
          [deadlockKey, String(extensions + 1)]
        );
        // Update last_heartbeat to extend by 15 more minutes
        agentdesk.db.execute(
          "UPDATE sessions SET last_heartbeat = datetime('now') WHERE session_key = ?",
          [sess.session_key]
        );
        agentdesk.log.warn("[deadlock] Session " + sess.session_key +
          " — heartbeat stale " + DEADLOCK_MINUTES + "min. Extension " +
          (extensions + 1) + "/" + MAX_EXTENSIONS);
        sendNotifyAlert(getPMDChannel(),
          "⚠️ [Deadlock 의심] " + sess.agent_id + " 세션 — " +
          DEADLOCK_MINUTES + "분 무응답 (연장 " + (extensions + 1) + "/" + MAX_EXTENSIONS + ")");
      }
      // extensions == -1 means already force-alerted, skip
    }

    // Clean up deadlock counters for sessions that are no longer working
    var activeKeys = agentdesk.db.query(
      "SELECT key FROM kv_meta WHERE key LIKE 'deadlock_check:%'"
    );
    for (var ak = 0; ak < activeKeys.length; ak++) {
      var sessKey = activeKeys[ak].key.replace("deadlock_check:", "");
      var stillWorking = agentdesk.db.query(
        "SELECT COUNT(*) as cnt FROM sessions WHERE session_key = ? AND status = 'working'",
        [sessKey]
      );
      if (stillWorking.length > 0 && stillWorking[0].cnt === 0) {
        agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [activeKeys[ak].key]);
      }
    }
  }
};

agentdesk.registerPolicy(timeouts);
