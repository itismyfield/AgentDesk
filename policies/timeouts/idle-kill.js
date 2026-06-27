module.exports = function attachIdleKill(timeouts, helpers) {
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

  timeouts._section_O = function() {
      var apiPort = agentdesk.config.get("server_port");
      if (!apiPort) {
        agentdesk.log.error("[idle-kill] server_port missing — cannot call kill-tmux API");
        return;
      }
      var agents = loadAgentDirectory();
      backfillMissingSessionAgentIds(agents);

      // Sessions table is PostgreSQL (SQLite path retired in #1239 series).
      // Threshold raised from 60min → 6h: shorter values churn through
      // user-active sessions during natural away periods (lunch / meeting).
      //
      // Cleanup mode: kill-tmux only (not force-kill). force-kill atomically
      // disconnects the session row and clears retry metadata, which was
      // wiping `claude_session_id` selector context that the next user turn
      // could otherwise resume via recap. kill-tmux leaves the DB row intact
      // (status='idle', selector preserved) so the next message can rehydrate
      // the provider session through the recap path.
      //
      // Scope: main channels only. Thread-suffixed sessions are filtered both
      // server-side (`thread_channel_id IS NULL` + session_key regex guard)
      // and client-side (parseSessionThreadId) so the JS LIMIT-50 window is
      // not starved by thread-heavy backlogs. Thread sessions are managed
      // by gc_stale_thread_sessions_pg, which deletes stale thread rows and
      // reaps their owner-marked tmux sessions before an 8h JS backstop could
      // observe them.
      var mainChannelSqlGuard =
        "AND thread_channel_id IS NULL " +
        "AND session_key !~ '-t[0-9]{15,}(-dev)?$' ";
      var effectiveLastSeenJoin =
        "CROSS JOIN LATERAL (SELECT COALESCE(s.last_heartbeat, s.created_at) AS last_seen_at) latest ";
      var idleSessions = agentdesk.db.query(
        "SELECT s.session_key, s.agent_id, s.provider, s.active_dispatch_id, s.thread_channel_id, latest.last_seen_at " +
        "FROM sessions s " +
        effectiveLastSeenJoin +
        "WHERE status = 'idle' " +
        "AND provider IN ('claude', 'codex', 'qwen') " +
        "AND active_dispatch_id IS NULL " +
        mainChannelSqlGuard +
        "AND latest.last_seen_at < NOW() - INTERVAL '6 hours' " +
        "ORDER BY latest.last_seen_at ASC LIMIT 50"
      );

      // Defense-in-depth: client-side filter catches anything the SQL guard
      // missed (e.g. helper logic evolves with new session_key shapes).
      function isMainChannelSession(s) {
        return !s.thread_channel_id
          && !parseSessionThreadId(s.session_key, s.provider);
      }
      idleSessions = idleSessions.filter(isMainChannelSession);

      function formatIdleDuration(idleMin) {
        if (idleMin >= 60 * 24) {
          return Math.round(idleMin / (60 * 24)) + "일";
        }
        if (idleMin >= 60) {
          return Math.round(idleMin / 60) + "시간";
        }
        return idleMin + "분";
      }

      var now = Date.now();
      var processed = {};

      function killTmuxIdleSessions(sessions, minimumIdleMinutes, reasonLabel, maxKills) {
        var killedCount = 0;
        for (var i = 0; i < sessions.length; i++) {
          if (killedCount >= maxKills) {
            agentdesk.log.info("[idle-kill] Reached max " + maxKills + " kills for this category. Breaking early.");
            break;
          }

          var s = sessions[i];
          if (!s.session_key || processed[s.session_key]) continue;
          processed[s.session_key] = true;

          var lastSeenMs = s.last_seen_at ? new Date(s.last_seen_at).getTime() : NaN;
          var idleMin = isNaN(lastSeenMs)
            ? minimumIdleMinutes
            : Math.max(minimumIdleMinutes, Math.round((now - lastSeenMs) / 60000));

          var killResp = null;
          try {
            var killUrl = "http://127.0.0.1:" + apiPort +
              "/api/sessions/" + encodeURIComponent(s.session_key) + "/kill-tmux";
            killResp = agentdesk.http.post(killUrl, {
              reason: "idle " + formatIdleDuration(idleMin) + " 초과 — 자동 정리",
              minimum_idle_minutes: minimumIdleMinutes
            });
          } catch (e) {
            agentdesk.log.error("[idle-kill] kill-tmux API exception for " + s.session_key + ": " + e);
            continue;
          }

          if (!killResp || !killResp.ok) {
            agentdesk.log.error("[idle-kill] kill-tmux API failed for " + s.session_key + ": " + JSON.stringify(killResp));
            continue;
          }

          if (killResp.tmux_was_alive === false) {
            // #2861: tmux already gone — a no-op kill that must NOT consume the
            // per-category kill budget. Otherwise zombie idle rows at the front
            // of the oldest-first queue spend the entire budget every tick and
            // permanently starve genuinely-alive idle sessions behind them. The
            // kill-tmux handler reconciles such a stale row to `disconnected`
            // (session_row_disconnected=true), so it leaves the candidate pool
            // on the next tick rather than blocking forever.
            agentdesk.log.warn(
              "[idle-kill] kill-tmux: tmux already gone for " + s.session_key +
              " (reconciled=" + (killResp.session_row_disconnected === true) + ", not counted toward budget)"
            );
            continue;
          }

          if (killResp.skipped_live_activity_guard) {
            agentdesk.log.info(
              "[idle-kill] kill-tmux skipped live activity guard for " + s.session_key +
              " (heartbeat refreshed, not counted toward budget)"
            );
            continue;
          }

          if (killResp.skipped_active_dispatch_guard) {
            agentdesk.log.warn(
              "[idle-kill] kill-tmux refused active-dispatch session for " + s.session_key +
              " (not counted toward budget; dispatch cleanup owns this case)"
            );
            continue;
          }

          if (!killResp.tmux_killed) {
            // tmux WAS alive but `tmux kill-session` failed — a genuine failure,
            // not a zombie. Count it toward the budget so a stuck-but-live
            // session can't be retried unbounded every tick, and surface it as
            // an error (distinct from the already-gone case above).
            agentdesk.log.error(
              "[idle-kill] kill-tmux: tmux was alive but kill failed for " + s.session_key +
              " (counted toward budget)"
            );
            killedCount++;
            continue;
          }

          killedCount++;

          agentdesk.log.info(
            "[idle-kill] Killed idle tmux after " + idleMin + "min (" + reasonLabel + "): " + s.session_key
          );

          var agentContext = resolveSessionAgentContext(s, agents);
          agentdesk.log.info("[timeouts] idle kill: " + (agentContext.agent_id || "unknown") + " idle=" + idleMin + "m reason=" + reasonLabel);
        }
      }

      killTmuxIdleSessions(idleSessions, 360, "idle 6시간 경과 (active_dispatch_id 없음)", 3);
    };
};
