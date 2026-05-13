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
        agentdesk.log.error("[idle-kill] server_port missing — cannot call force-kill API");
        return;
      }
      var agents = loadAgentDirectory();
      backfillMissingSessionAgentIds(agents);

      // Sessions table is PostgreSQL (SQLite path retired in #1239 series).
      // Threshold raised from 60min → 6h: shorter values churn through
      // user-active sessions during natural away periods (lunch / meeting),
      // and the provider session ID is preserved on tmux cleanup so resume
      // is still possible after the kill.
      //
      // Scope: main channels only. Thread-suffixed sessions are filtered both
      // server-side (`thread_channel_id IS NULL` + session_key regex guard)
      // and client-side (parseSessionThreadId) so the JS LIMIT-50 window is
      // not starved by thread-heavy backlogs. Thread sessions are managed
      // by the auto-queue lifecycle (slot release clears claude_session_id
      // on task completion, src/db/auto_queue/slots.rs:63-75) and the
      // stuck-dispatch watchdog (#1546).
      var mainChannelSqlGuard =
        "AND thread_channel_id IS NULL " +
        "AND session_key !~ '-t[0-9]{15,}(-dev)?$' ";
      var idleSessions = agentdesk.db.query(
        "SELECT session_key, agent_id, provider, active_dispatch_id, thread_channel_id, " +
        "COALESCE(last_heartbeat, created_at) AS last_seen_at " +
        "FROM sessions " +
        "WHERE status = 'idle' " +
        "AND provider IN ('claude', 'codex', 'qwen') " +
        "AND active_dispatch_id IS NULL " +
        mainChannelSqlGuard +
        "AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '6 hours' " +
        "ORDER BY COALESCE(last_heartbeat, created_at) ASC LIMIT 50"
      );
      var safetySessions = agentdesk.db.query(
        "SELECT session_key, agent_id, provider, active_dispatch_id, thread_channel_id, " +
        "COALESCE(last_heartbeat, created_at) AS last_seen_at " +
        "FROM sessions " +
        "WHERE status = 'idle' " +
        "AND provider IN ('claude', 'codex', 'qwen') " +
        "AND active_dispatch_id IS NOT NULL " +
        mainChannelSqlGuard +
        "AND COALESCE(last_heartbeat, created_at) < NOW() - INTERVAL '24 hours' " +
        "ORDER BY COALESCE(last_heartbeat, created_at) ASC LIMIT 50"
      );

      // Defense-in-depth: client-side filter catches anything the SQL guard
      // missed (e.g. helper logic evolves with new session_key shapes).
      function isMainChannelSession(s) {
        return !s.thread_channel_id
          && !parseSessionThreadId(s.session_key, s.provider);
      }
      idleSessions = idleSessions.filter(isMainChannelSession);
      safetySessions = safetySessions.filter(isMainChannelSession);

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

      function forceKillIdleSessions(sessions, minimumIdleMinutes, reasonLabel, maxKills) {
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

          var forceKillResp = null;
          try {
            var forceKillUrl = "http://127.0.0.1:" + apiPort +
              "/api/sessions/" + encodeURIComponent(s.session_key) + "/force-kill";
            forceKillResp = agentdesk.http.post(forceKillUrl, { retry: false, reason: "idle " + formatIdleDuration(idleMin) + " 초과 — 자동 정리" });
          } catch (e) {
            agentdesk.log.error("[idle-kill] force-kill API exception for " + s.session_key + ": " + e);
            continue;
          }

          if (!forceKillResp || !forceKillResp.ok) {
            agentdesk.log.error("[idle-kill] force-kill API failed for " + s.session_key + ": " + JSON.stringify(forceKillResp));
            continue;
          }

          killedCount++;

          if (!forceKillResp.tmux_killed) {
            agentdesk.log.warn("[idle-kill] force-kill API succeeded but tmux was already gone for " + s.session_key);
            continue;
          }

          agentdesk.log.info(
            "[idle-kill] Killed idle session after " + idleMin + "min (" + reasonLabel + "): " + s.session_key
          );

          var agentContext = resolveSessionAgentContext(s, agents);
          agentdesk.log.info("[timeouts] idle kill: " + (agentContext.agent_id || "unknown") + " idle=" + idleMin + "m reason=" + reasonLabel);
        }
      }

      forceKillIdleSessions(safetySessions, 1440, "idle 24시간 경과 (safety TTL)", 2);
      forceKillIdleSessions(idleSessions, 360, "idle 6시간 경과 (active_dispatch_id 없음)", 3);
    };
};
