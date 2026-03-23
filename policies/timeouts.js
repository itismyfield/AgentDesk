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
 * [I-0] 미전송 디스패치 알림 복구 (2분)
 * [I] 턴 데드락 감지 + 자동 복구 (15분 주기, 최대 3회 연장 후 강제 중단 + 재디스패치)
 */

// Send notification via notify bot (system alerts, not agent communication)
function sendNotifyAlert(channelTarget, message) {
  if (!channelTarget) return;
  try {
    var port = agentdesk.config.get("server_port") || 8791;
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
  var ch = agentdesk.config.get("kanban_manager_channel_id");
  if (!ch) {
    agentdesk.log.warn("[notify] No kanban_manager_channel_id configured, skipping");
    return null;
  }
  return "channel:" + ch;
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
      var kmChannel = getPMDChannel();
      if (kmChannel) try {
        var port = agentdesk.config.get("server_port") || 8791;
        agentdesk.http.post("http://127.0.0.1:" + port + "/api/send", {
          target: kmChannel,
          content: "[칸반매니저] ⏰ 타임아웃 결정 요청\n\n" +
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
      var kmChannel2 = getPMDChannel();
      if (kmChannel2) try {
        var port = agentdesk.config.get("server_port") || 8791;
        agentdesk.http.post("http://127.0.0.1:" + port + "/api/send", {
          target: kmChannel2,
          content: "[칸반매니저] ⚠️ 정체 카드 결정 요청\n\n" +
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
    // Auto-accept: same effect as manual review-decision accept
    // (status → in_progress, review_status → rework_pending, create rework dispatch)
    var staleSuggestions = agentdesk.db.query(
      "SELECT id, assigned_agent_id, title FROM kanban_cards " +
      "WHERE review_status = 'suggestion_pending' " +
      "AND updated_at < datetime('now', '-15 minutes')"
    );
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
          // Dispatch succeeded — now transition to in_progress + rework_pending
          agentdesk.kanban.setStatus(sc.id, "in_progress");
          agentdesk.db.execute(
            "UPDATE kanban_cards SET review_status = 'rework_pending', updated_at = datetime('now') WHERE id = ?",
            [sc.id]
          );
          agentdesk.log.warn("[timeout] Auto-accepted suggestions for card " + sc.id + " — rework dispatch created");
        } catch (e) {
          // Dispatch failed — route to pending_decision instead
          agentdesk.kanban.setStatus(sc.id, "pending_decision");
          agentdesk.db.execute(
            "UPDATE kanban_cards SET blocked_reason = 'Auto-accept rework dispatch failed: " + e + "' WHERE id = ?",
            [sc.id]
          );
          agentdesk.log.error("[timeout] Failed to create rework dispatch for " + sc.id + ": " + e + " → pending_decision");
        }
      } else {
        agentdesk.log.warn("[timeout] Auto-accepted card " + sc.id + " but no agent assigned — no rework dispatch");
      }
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
        var port = agentdesk.config.get("server_port") || 8791;
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

    // ─── [I] 턴 데드락 감지 + 자동 복구 (15분 주기) ─────────
    // 판별: sessions.last_heartbeat 기반 — 15분 무응답 시 데드락 의심
    // 연장: 15분 단위로 최대 MAX_EXTENSIONS회 (실제 작업 진행 시 자연 리셋)
    // 확정: 연장 상한 초과 시 강제 중단 + 재디스패치
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
        // ── 데드락 확정: 강제 중단 + 자동 복구 ──
        var totalMin = DEADLOCK_MINUTES * (MAX_EXTENSIONS + 1);
        agentdesk.log.warn("[deadlock] Session " + sess.session_key +
          " — max extensions (" + MAX_EXTENSIONS + ") reached. Force cancelling + re-dispatch.");

        // 1) tmux 세션 강제 종료 (cancel_token 대체 — JS에서 직접 kill)
        var tmuxName = (sess.session_key.indexOf(":") >= 0)
          ? sess.session_key.split(":")[1] : sess.session_key;
        try {
          agentdesk.exec("tmux", ["kill-session", "-t", tmuxName]);
          agentdesk.log.info("[deadlock] Killed tmux session: " + tmuxName);
        } catch (e) {
          agentdesk.log.warn("[deadlock] tmux kill failed (may already be dead): " + e);
        }

        // 2) 세션 상태 disconnected
        agentdesk.db.execute(
          "UPDATE sessions SET status = 'disconnected', last_heartbeat = datetime('now') WHERE session_key = ?",
          [sess.session_key]
        );

        // 3) 현재 디스패치 실패 + 재디스패치
        var redispatched = false;
        if (sess.active_dispatch_id) {
          var dispInfo = agentdesk.db.query(
            "SELECT kanban_card_id, to_agent_id, dispatch_type, title " +
            "FROM task_dispatches WHERE id = ?",
            [sess.active_dispatch_id]
          );
          agentdesk.db.execute(
            "UPDATE task_dispatches SET status = 'failed', " +
            "result = 'Deadlock auto-recovery: " + totalMin + "min timeout', " +
            "updated_at = datetime('now') WHERE id = ? AND status IN ('pending','dispatched')",
            [sess.active_dispatch_id]
          );

          if (dispInfo.length > 0) {
            var di = dispInfo[0];
            try {
              agentdesk.dispatch.create(
                di.kanban_card_id,
                di.to_agent_id,
                di.dispatch_type || "implementation",
                "[Retry] " + (di.title || "deadlock recovery")
              );
              redispatched = true;
              agentdesk.log.info("[deadlock] Re-dispatched card " +
                di.kanban_card_id + " → " + di.to_agent_id);
            } catch (e) {
              // 재디스패치 실패 시 PMD 판단으로 이관
              agentdesk.kanban.setStatus(di.kanban_card_id, "pending_decision");
              agentdesk.db.execute(
                "UPDATE kanban_cards SET blocked_reason = ? WHERE id = ?",
                ["Deadlock recovery re-dispatch failed: " + e, di.kanban_card_id]
              );
              agentdesk.log.error("[deadlock] Re-dispatch failed for " +
                di.kanban_card_id + ": " + e + " → pending_decision");
            }
          }
        }

        // 4) PMD 알림
        sendNotifyAlert(getPMDChannel(),
          "🔴 [Deadlock 복구] " + sess.agent_id + " 세션 " + sess.session_key +
          " — " + totalMin + "분 무응답 → 강제 중단" +
          (redispatched ? " + 재디스패치 완료" : " (재디스패치 실패 — pending_decision)"));

        // 5) 이력 기록
        agentdesk.db.execute(
          "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
          ["deadlock_history:" + sess.session_key + ":" + Date.now(),
           JSON.stringify({
             session_key: sess.session_key,
             agent_id: sess.agent_id,
             dispatch_id: sess.active_dispatch_id,
             extensions: extensions,
             action: redispatched ? "force_cancel_and_redispatch" : "force_cancel_pending_decision",
             ts: new Date().toISOString()
           })]
        );

        // 카운터 삭제 (다음 세션은 새 카운터)
        agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [deadlockKey]);

      } else if (extensions >= 0) {
        // ── 데드락 의심: 타임아웃 15분 연장 ──
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
      // extensions < 0: 이전 세션의 잔여 카운터 — 무시
    }

    // Clean up deadlock counters for sessions no longer working
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

    // Clean up old deadlock history entries (7일 이상)
    var historyKeys = agentdesk.db.query(
      "SELECT key FROM kv_meta WHERE key LIKE 'deadlock_history:%'"
    );
    var sevenDaysAgo = Date.now() - 7 * 24 * 60 * 60 * 1000;
    for (var hk = 0; hk < historyKeys.length; hk++) {
      var parts = historyKeys[hk].key.split(":");
      var ts = parseInt(parts[parts.length - 1], 10);
      if (ts && ts < sevenDaysAgo) {
        agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [historyKeys[hk].key]);
      }
    }
  },

  // ─── [I] 컨텍스트 윈도우 자동 관리 ─────────────────────
  // onTick에서 세션 토큰 사용량을 모니터링하고 compact/clear 자동 호출
  onContextCheck: function() {
    var CONTEXT_WINDOW = 1000000; // 1M tokens
    var compactPercent = parseInt(agentdesk.config.get("context_compact_percent") || "60", 10);
    var clearPercent = parseInt(agentdesk.config.get("context_clear_percent") || "40", 10);
    var clearIdleMin = parseInt(agentdesk.config.get("context_clear_idle_minutes") || "60", 10);

    var sessions = agentdesk.db.query(
      "SELECT session_key, agent_id, tokens, status, last_heartbeat FROM sessions WHERE status != 'disconnected' AND tokens > 0"
    );

    var now = Date.now();

    for (var i = 0; i < sessions.length; i++) {
      var s = sessions[i];
      if (!s.session_key) continue;

      var pct = (s.tokens / CONTEXT_WINDOW) * 100;

      // Skip working sessions — don't interrupt active work
      if (s.status === "working") continue;

      // Check provider — /compact and /clear are Claude Code commands only
      var sessionInfo = agentdesk.db.query(
        "SELECT provider FROM sessions WHERE session_key = ?", [s.session_key]
      );
      var provider = sessionInfo.length > 0 ? sessionInfo[0].provider : "claude";
      if (provider !== "claude") continue; // Skip non-Claude sessions for now

      // Check cooldown (5 min) to avoid spamming commands
      var cooldownKey = "context_action_" + s.session_key;
      var lastAction = agentdesk.db.query(
        "SELECT value FROM kv_meta WHERE key = ?", [cooldownKey]
      );
      if (lastAction.length > 0) {
        var lastMs = parseInt(lastAction[0].value, 10);
        if (now - lastMs < 300000) continue; // 5 min cooldown
      }

      // Compact: >= compactPercent
      if (pct >= compactPercent) {
        var result = JSON.parse(agentdesk.session.sendCommand(s.session_key, "/compact"));
        if (result.ok) {
          agentdesk.log.info("[context] Auto-compact: " + s.session_key + " (" + Math.round(pct) + "%)");
          agentdesk.db.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
            [cooldownKey, "" + now]
          );
          // Discord notification
          var agent = agentdesk.db.query("SELECT discord_channel_id FROM agents WHERE id = ?", [s.agent_id]);
          if (agent.length > 0 && agent[0].discord_channel_id) {
            sendNotifyAlert(
              "channel:" + agent[0].discord_channel_id,
              "⚡ 컨텍스트 자동 compact 실행 (" + Math.round(pct) + "% → " + s.session_key + ")"
            );
          }
        }
        continue; // Don't also clear in same tick
      }

      // Clear: >= clearPercent AND idle for clearIdleMin
      if (pct >= clearPercent && s.last_heartbeat) {
        var lastHb = new Date(s.last_heartbeat).getTime();
        var idleMs = now - lastHb;
        var idleMin = idleMs / 60000;

        if (idleMin >= clearIdleMin) {
          var result2 = JSON.parse(agentdesk.session.sendCommand(s.session_key, "/clear"));
          if (result2.ok) {
            agentdesk.log.info("[context] Auto-clear: " + s.session_key + " (" + Math.round(pct) + "%, idle " + Math.round(idleMin) + "min)");
            agentdesk.db.execute(
              "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
              [cooldownKey, "" + now]
            );
            var agent2 = agentdesk.db.query("SELECT discord_channel_id FROM agents WHERE id = ?", [s.agent_id]);
            if (agent2.length > 0 && agent2[0].discord_channel_id) {
              sendNotifyAlert(
                "channel:" + agent2[0].discord_channel_id,
                "🧹 컨텍스트 자동 clear 실행 (" + Math.round(pct) + "%, idle " + Math.round(idleMin) + "분 → " + s.session_key + ")"
              );
            }
          }
        }
      }
    }
  }
};

// Wire onContextCheck into onTick
var _origOnTick = timeouts.onTick;
timeouts.onTick = function() {
  _origOnTick.call(this);
  if (timeouts.onContextCheck) {
    try { timeouts.onContextCheck(); } catch(e) {
      agentdesk.log.warn("[context] onContextCheck error: " + e);
    }
  }
};

agentdesk.registerPolicy(timeouts);
