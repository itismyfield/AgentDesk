/* giant-file-exemption: reason=monitor-section-needs-further-split ticket=#1078 */
module.exports = function attachActiveMonitor(timeouts, helpers) {
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

  timeouts._tmuxHasLivePane = function(tmuxName) {
      try {
        // "=" prefix prevents tmux prefix-matching (exact_target convention)
        var out = agentdesk.exec("tmux", ["list-panes", "-t", "=" + tmuxName, "-F", "#{pane_dead}"]);
        // Success: lines of "0" (alive) or "1" (dead). Any "0" = live pane exists.
        // Failure: "ERROR: ..." (session gone)
        return typeof out === "string" && out.indexOf("ERROR") === -1 && out.indexOf("0") !== -1;
      } catch(e) {
        return false;
      }
    };

  timeouts._section_I = function() {
      // ─── [I] 턴 데드락 감지 + 자동 복구 (30분 주기) ─────────
      // 판별: sessions.last_heartbeat 기반. 정상 진행은 tmux live + inflight 최근 output으로 인정.
      // 회복: 정상 진행이면 watchdog을 30분씩 롤링 연장. 최근 output이 없으면 연속 스톨만 카운트.
      // 확정: 연속 스톨 상한 또는 turn 3시간 상한 도달 시 강제 중단 + 재디스패치.
      var DEADLOCK_MINUTES = 30;
      var MAX_EXTENSIONS = 3;
      var MAX_TURN_MINUTES = 180;
      var iCfg = agentdesk.pipeline.getConfig();
      var iInitial = agentdesk.pipeline.kickoffState(iCfg);
      var iInProgress = agentdesk.pipeline.nextGatedTarget(iInitial, iCfg);

      // 먼저: heartbeat가 신선한 working 세션의 카운터를 리셋 (비연속 스톨 누적 방지)
      var freshSessions = agentdesk.db.query(
        "SELECT session_key FROM sessions WHERE status IN ('turn_active', 'working') " +
        "AND last_heartbeat >= datetime('now', '-" + DEADLOCK_MINUTES + " minutes')"
      );
      for (var fs = 0; fs < freshSessions.length; fs++) {
        var freshKey = "deadlock_check:" + freshSessions[fs].session_key;
        agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [freshKey]);
      }

      // Fix stale working sessions: if status=working but no inflight file exists,
      // the turn has ended but DB wasn't updated. Fix to idle.
      // #219: Increased grace period from 3min to 10min — agents running long tool
      // calls (cargo build, subagents) may not send heartbeats for several minutes.
      var staleWorkingSessions = agentdesk.db.query(
        "SELECT session_key FROM sessions WHERE status IN ('turn_active', 'working') " +
        "AND last_heartbeat < datetime('now', '-10 minutes')"
      );
      for (var sw = 0; sw < staleWorkingSessions.length; sw++) {
        var swKey = staleWorkingSessions[sw].session_key;
        var tmuxName = (swKey || "").split(":").pop();
        // #219: Check if tmux session has a live pane (not just session existence).
        // has-session returns true for zombie sessions with dead panes;
        // list-panes #{pane_dead} distinguishes live vs dead workers.
        var tmuxAlive = timeouts._tmuxHasLivePane(tmuxName);
        var inflight;
        try {
          inflight = findRecentInflightForSession(swKey, tmuxName);
        } catch (e) {
          agentdesk.log.warn("[deadlock] Transient error looking up inflight for " + swKey + ": " + e);
          continue; // transient error, retry next time
        }
        if (!tmuxAlive || !inflight) {
          // #219: Fail any pending dispatch before transitioning to idle.
          // Without this, the dispatch stays "pending" as an orphan and gets
          // re-delivered or auto-completed, causing the failure loop.
          try {
            var swSessInfo = agentdesk.db.query(
              "SELECT active_dispatch_id FROM sessions WHERE session_key = ?", [swKey]
            );
            if (swSessInfo.length > 0 && swSessInfo[0].active_dispatch_id) {
              var swDispId = swSessInfo[0].active_dispatch_id;
              var swDispStatus = agentdesk.db.query(
                "SELECT status FROM task_dispatches WHERE id = ?", [swDispId]
              );
              if (swDispStatus.length > 0 && (swDispStatus[0].status === "pending" || swDispStatus[0].status === "dispatched")) {
                agentdesk.dispatch.markFailed(swDispId, "Stale working session recovery — no active tmux session after 10min");
                agentdesk.log.warn("[deadlock] Failed stale dispatch " + swDispId + " for session " + swKey);
              }
            }
          } catch(dispErr) {
            agentdesk.log.warn("[deadlock] Failed to mark dispatch for " + swKey + ": " + dispErr);
          }
          agentdesk.db.execute(
            "UPDATE sessions " +
            "SET status = 'idle', active_dispatch_id = NULL, last_heartbeat = datetime('now') " +
            "WHERE session_key = ? AND status IN ('turn_active', 'working')",
            [swKey]
          );
          agentdesk.log.info("[deadlock] Fixed stale working session → idle: " + swKey);
        }
      }

      // 데드락 의심 세션: sessions.last_heartbeat 기반 판별
      // deadlock-manager 자신의 세션은 제외 (자기 자신을 오탐하는 무한 루프 방지)
      var staleSessions = agentdesk.db.query(
        "SELECT session_key, agent_id, active_dispatch_id, last_heartbeat " +
        "FROM sessions WHERE status IN ('turn_active', 'working') " +
        "AND session_key NOT LIKE '%deadlock-manager%' " +
        "AND last_heartbeat < datetime('now', '-" + DEADLOCK_MINUTES + " minutes') " +
        "ORDER BY last_heartbeat ASC LIMIT 50"
      );
      for (var dl = 0; dl < staleSessions.length; dl++) {
        var sess = staleSessions[dl];
        var deadlockKey = "deadlock_check:" + sess.session_key;
        var dlTmuxName = (sess.session_key || "").split(":").pop();
        var tmuxAlive = timeouts._tmuxHasLivePane(dlTmuxName);
        var inflightProgress = tmuxAlive
          ? inspectInflightProgress(sess.session_key, dlTmuxName, DEADLOCK_MINUTES, MAX_TURN_MINUTES)
          : { recent: false, updated_age_min: null, turn_age_min: null, channel_id: null, max_turn_reached: false };

        // Recent terminal output is the authoritative signal for "normal progress".
        // A live pane alone is not enough — hung tools can leave a pane alive forever.
        if (tmuxAlive && inflightProgress.recent && !inflightProgress.max_turn_reached) {
          agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [deadlockKey]);
          var extendMin = DEADLOCK_MINUTES;
          if (inflightProgress.turn_age_min !== null) {
            extendMin = Math.min(
              DEADLOCK_MINUTES,
              Math.max(0, MAX_TURN_MINUTES - inflightProgress.turn_age_min)
            );
          }
          var extendResp = requestTurnWatchdogExtension(inflightProgress.channel_id, extendMin);
          var extendMinText = Math.max(1, Math.round(extendMin));
          if (extendResp.ok) {
            agentdesk.log.info("[deadlock] Session " + sess.session_key +
              " — live pane + recent output confirmed. Extended watchdog +" + extendMinText + "min.");
            sendDeadlockAlert(
              "🟢 [Deadlock 점검] " + sess.agent_id + "\n" +
              "session_key: " + sess.session_key + "\n" +
              "tmux: " + (dlTmuxName || "unknown") + "\n" +
              "최근 output: " + Math.round(inflightProgress.updated_age_min || 0) + "분 전\n" +
              "정상 진행 확인, +" + extendMinText + "분 연장"
            );
          } else {
            agentdesk.log.warn("[deadlock] Session " + sess.session_key +
              " — recent output confirmed but watchdog extension failed: " + extendResp.error);
            sendDeadlockAlert(
              "🟢 [Deadlock 점검] " + sess.agent_id + "\n" +
              "session_key: " + sess.session_key + "\n" +
              "tmux: " + (dlTmuxName || "unknown") + "\n" +
              "최근 output: " + Math.round(inflightProgress.updated_age_min || 0) + "분 전\n" +
              "정상 진행 확인, watchdog 연장 실패: " + extendResp.error
            );
          }
          continue;
        }

        // 활성 턴(inflight)이 없는 working 세션은 idle로 전환하고 스킵
        // (턴 완료 후 세션 상태가 working으로 남은 stale 케이스)
        if (!tmuxAlive || (!inflightProgress.channel_id && !inflightProgress.recent)) {
          agentdesk.db.execute(
            "UPDATE sessions " +
            "SET status = 'idle', last_heartbeat = datetime('now') " +
            "WHERE session_key = ? AND status IN ('turn_active', 'working')",
            [sess.session_key]
          );
          agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [deadlockKey]);
          agentdesk.log.info("[deadlock] Stale working session → idle (no active turn): " + sess.session_key);
          continue;
        }

        // Check extension count + last check timestamp
        var extRecord = agentdesk.db.query(
          "SELECT value FROM kv_meta WHERE key = ?", [deadlockKey]
        );
        var extensions = 0;
        var lastCheckAt = 0;
        if (extRecord.length > 0) {
          try {
            var parsed = JSON.parse(extRecord[0].value);
            extensions = parsed.count || 0;
            lastCheckAt = parsed.ts || 0;
          } catch(e) {
            // 기존 형식(숫자만) 마이그레이션
            extensions = parseInt(extRecord[0].value) || 0;
          }
        }

        // 마지막 체크 후 DEADLOCK_MINUTES 미경과 시 스킵 (1분마다 카운터 증가 방지)
        var nowMs = Date.now();
        if (lastCheckAt > 0 && (nowMs - lastCheckAt) < DEADLOCK_MINUTES * 60 * 1000) {
          continue;
        }

        var hitTurnCap = tmuxAlive && inflightProgress.recent && inflightProgress.max_turn_reached;
        if (hitTurnCap || extensions >= MAX_EXTENSIONS) {
          // ── 데드락 확정: 강제 중단 + 자동 복구 ──
          var totalMin = hitTurnCap
            ? Math.max(MAX_TURN_MINUTES, Math.round(inflightProgress.turn_age_min || 0))
            : DEADLOCK_MINUTES * (MAX_EXTENSIONS + 1);
          var timeoutLabel = hitTurnCap
            ? (MAX_TURN_MINUTES + "분 상한 도달")
            : (totalMin + "분 무응답");
          agentdesk.log.warn("[deadlock] Session " + sess.session_key +
            (hitTurnCap
              ? " — max turn cap reached. Force cancelling + re-dispatch."
              : " — max extensions (" + MAX_EXTENSIONS + ") reached. Force cancelling + re-dispatch."));

          // 1) authoritative force-kill API로 tmux 종료 + inflight cleanup + dispatch fail/retry 일원화
          var forceKillResp = null;
          try {
            var apiPort = agentdesk.config.get("server_port");
            if (!apiPort) {
              agentdesk.log.error("[deadlock] server_port missing — cannot call force-kill API");
              continue;
            }
            var forceKillUrl = "http://127.0.0.1:" + apiPort +
              "/api/sessions/" + encodeURIComponent(sess.session_key) + "/force-kill";
            forceKillResp = agentdesk.http.post(forceKillUrl, { retry: true, reason: "deadlock timeout — 턴 무응답으로 강제 종료" });
          } catch (e) {
            agentdesk.log.error("[deadlock] force-kill API exception for " + sess.session_key + ": " + e);
            continue;
          }

          if (!forceKillResp || !forceKillResp.ok) {
            agentdesk.log.error("[deadlock] force-kill API failed for " + sess.session_key + ": " + JSON.stringify(forceKillResp));
            continue;
          }

          if (forceKillResp.tmux_killed) {
            agentdesk.log.info("[deadlock] Killed tmux session via API: " + sess.session_key);
          } else {
            agentdesk.log.warn("[deadlock] tmux already gone or kill no-op for " + sess.session_key);
          }

          var redispatched = !!forceKillResp.retry_dispatch_id;
          if (redispatched) {
            agentdesk.log.info("[deadlock] Retry dispatch created: " + forceKillResp.retry_dispatch_id);
          } else if (forceKillResp.queue_activation_requested) {
            agentdesk.log.info("[deadlock] No retry dispatch created — requested auto-queue activation for agent " + sess.agent_id);
          }

          // 4) Deadlock-manager 알림 (announce 봇)
          sendDeadlockAlert(
            "🔴 [Deadlock 복구] " + sess.agent_id + "\n" +
            "session_key: " + sess.session_key + "\n" +
            "tmux: " + ((sess.session_key || "").split(":").pop() || "unknown") + "\n" +
            "연장: " + extensions + "/" + MAX_EXTENSIONS + "\n" +
            timeoutLabel + " → 강제 중단" +
            (redispatched ? " + 재디스패치 완료" : ""));

          // 5) Termination audit
          try {
            var probeInfo = "agent=" + sess.agent_id + " extensions=" + extensions + "/" + MAX_EXTENSIONS +
              " last_heartbeat=" + sess.last_heartbeat +
              " recent_output_age_min=" + (inflightProgress.updated_age_min === null ? "null" : Math.round(inflightProgress.updated_age_min)) +
              " turn_age_min=" + (inflightProgress.turn_age_min === null ? "null" : Math.round(inflightProgress.turn_age_min)) +
              " kill_ok=" + (!!forceKillResp.tmux_killed) +
              " inflight_cleared=" + (!!forceKillResp.inflight_cleared);
            agentdesk.db.execute(
              "INSERT INTO session_termination_events (session_key, dispatch_id, killer_component, reason_code, reason_text, probe_snapshot, tmux_alive) VALUES (?, ?, ?, ?, ?, ?, ?)",
              [sess.session_key, sess.active_dispatch_id || null, "deadlock_policy", "deadlock_timeout",
               timeoutLabel + " — " + (redispatched ? "redispatched" : "cancelled"), probeInfo, tmuxAlive ? 1 : 0]
            );
          } catch (e) { /* fire-and-forget */ }

          // 6) 이력 기록 (legacy)
          agentdesk.db.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
            ["deadlock_history:" + sess.session_key + ":" + Date.now(),
             JSON.stringify({
               session_key: sess.session_key,
               agent_id: sess.agent_id,
               dispatch_id: sess.active_dispatch_id,
               retry_dispatch_id: forceKillResp.retry_dispatch_id || null,
               extensions: extensions,
               action: redispatched ? "force_cancel_and_redispatch" : "force_cancel_only",
               ts: new Date().toISOString()
             })]
          );

          // 카운터 삭제 (다음 세션은 새 카운터)
          agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [deadlockKey]);

        } else {
          // ── 데드락 의심: 카운터 증가 (타임스탬프 포함, last_heartbeat 인위적 덮어쓰기 없음) ──
          agentdesk.db.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
            [deadlockKey, JSON.stringify({ count: extensions + 1, ts: nowMs })]
          );
          agentdesk.log.warn("[deadlock] Session " + sess.session_key +
            " — heartbeat stale " + DEADLOCK_MINUTES + "min. Extension " +
            (extensions + 1) + "/" + MAX_EXTENSIONS);
          sendDeadlockAlert(
            "⚠️ [Deadlock 의심] " + sess.agent_id + "\n" +
            "session_key: " + sess.session_key + "\n" +
            "tmux: " + ((sess.session_key || "").split(":").pop() || "unknown") + "\n" +
            "무응답: " + DEADLOCK_MINUTES + "분 (연장 " + (extensions + 1) + "/" + MAX_EXTENSIONS + ")");
        }
      }

      // Clean up deadlock counters for sessions no longer working
      var activeKeys = agentdesk.db.query(
        "SELECT key FROM kv_meta WHERE key LIKE 'deadlock_check:%'"
      );
      for (var ak = 0; ak < activeKeys.length; ak++) {
        var sessKey = activeKeys[ak].key.replace("deadlock_check:", "");
        var stillWorking = agentdesk.db.query(
          "SELECT COUNT(*) as cnt FROM sessions WHERE session_key = ? AND status IN ('turn_active', 'working')",
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
    };
};
