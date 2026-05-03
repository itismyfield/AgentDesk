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

      var idleSessions = agentdesk.db.query(
        "SELECT session_key, agent_id, provider, active_dispatch_id, thread_channel_id, " +
        "COALESCE(last_heartbeat, created_at) AS last_seen_at " +
        "FROM sessions " +
        "WHERE status = 'idle' " +
        "AND provider IN ('claude', 'codex', 'qwen') " +
        "AND active_dispatch_id IS NULL " +
        "AND COALESCE(last_heartbeat, created_at) < datetime('now', '-60 minutes') " +
        "ORDER BY COALESCE(last_heartbeat, created_at) ASC LIMIT 10"
      );
      var safetySessions = agentdesk.db.query(
        "SELECT session_key, agent_id, provider, active_dispatch_id, thread_channel_id, " +
        "COALESCE(last_heartbeat, created_at) AS last_seen_at " +
        "FROM sessions " +
        "WHERE status = 'idle' " +
        "AND provider IN ('claude', 'codex', 'qwen') " +
        "AND COALESCE(last_heartbeat, created_at) < datetime('now', '-180 minutes') " +
        "ORDER BY COALESCE(last_heartbeat, created_at) ASC LIMIT 10"
      );

      var now = Date.now();
      var processed = {};

      function forceKillIdleSessions(sessions, minimumIdleMinutes, reasonLabel) {
        for (var i = 0; i < sessions.length; i++) {
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
            forceKillResp = agentdesk.http.post(forceKillUrl, { retry: false, reason: "idle " + idleMin + "분 초과 — 자동 정리" });
          } catch (e) {
            agentdesk.log.error("[idle-kill] force-kill API exception for " + s.session_key + ": " + e);
            continue;
          }

          if (!forceKillResp || !forceKillResp.ok) {
            agentdesk.log.error("[idle-kill] force-kill API failed for " + s.session_key + ": " + JSON.stringify(forceKillResp));
            continue;
          }

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

      forceKillIdleSessions(idleSessions, 60, "idle 60분 경과 (active_dispatch_id 없음)");
      forceKillIdleSessions(safetySessions, 180, "idle 180분 경과 (safety TTL)");
    };
};
