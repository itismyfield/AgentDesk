module.exports = function attachLongTurnMonitor(timeouts, helpers) {
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

  timeouts._section_L = function() {
      // ─── [L] Inflight 장시간 턴 감지 (#130) ──────────────────
      // heartbeat와 독립 — inflight 파일의 started_at 기반 단계별 알림.
      // Prevents alarm fatigue while still notifying at key thresholds.
      var ALERT_THRESHOLDS = [30, 60, 120]; // minutes
      try {
        var inflights = agentdesk.inflight.list();
        for (var li = 0; li < inflights.length; li++) {
          var inf = inflights[li];
          if (!inf.started_at) continue;
          // Stale inflight check: skip cleanup here — let InflightCleanupGuard handle it.
          // Previous approach (checking working sessions) caused false positives because
          // DB session status can lag behind actual tmux state.
          var startedAtMs = parseLocalTimestampMs(inf.started_at);
          if (startedAtMs <= 0) continue;
          var elapsedMin = (Date.now() - startedAtMs) / 60000;
          // Find the highest threshold that elapsed time exceeds
          var currentTier = -1;
          for (var t = ALERT_THRESHOLDS.length - 1; t >= 0; t--) {
            if (elapsedMin >= ALERT_THRESHOLDS[t]) { currentTier = t; break; }
          }
          if (currentTier < 0) continue; // under 30min, skip
          // Check if we already alerted at this tier
          var tierKey = "long_turn_tier:" + inf.provider + ":" + inf.channel_id;
          var lastTier = agentdesk.db.query("SELECT value FROM kv_meta WHERE key = ?", [tierKey]);
          var lastAlertedTier = lastTier.length > 0 ? parseInt(lastTier[0].value, 10) : -1;
          if (currentTier <= lastAlertedTier) continue; // already alerted at this tier or higher
          // Resolve agent_id: prefer dispatch target, fallback to channel owner (#130)
          var agentId = "?";
          if (inf.dispatch_id) {
            var dispRow = agentdesk.db.query(
              "SELECT to_agent_id FROM task_dispatches WHERE id = ? LIMIT 1",
              [inf.dispatch_id]
            );
            if (dispRow.length > 0 && dispRow[0].to_agent_id) {
              agentId = dispRow[0].to_agent_id;
            }
          }
          if (agentId === "?") {
            // #304: search all channel columns for reverse lookup
            var agentRows = agentdesk.db.query(
              "SELECT id FROM agents WHERE discord_channel_id = ? OR discord_channel_alt = ? OR discord_channel_cc = ? OR discord_channel_cdx = ? LIMIT 1",
              [inf.channel_id, inf.channel_id, inf.channel_id, inf.channel_id]
            );
            if (agentRows.length > 0) agentId = agentRows[0].id;
          }
          sendDeadlockAlert(
            "⚠️ [장시간 턴] " + (inf.channel_name || inf.channel_id) + "\n" +
            "agent_id: " + agentId + "\n" +
            "session_key: " + (inf.session_key || "?") + "\n" +
            "dispatch_id: " + (inf.dispatch_id || "?") + "\n" +
            "tmux: " + (inf.tmux_session_name || "?") + "\n" +
            "경과: " + Math.round(elapsedMin) + "분 (" + ALERT_THRESHOLDS[currentTier] + "분 단계)\n" +
            "provider: " + (inf.provider || "?")
          );
          agentdesk.db.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
            [tierKey, "" + currentTier]
          );
          agentdesk.log.warn("[long-turn] " + (inf.channel_name || inf.channel_id) + " — " + Math.round(elapsedMin) + "min (tier " + currentTier + ")");
        }
        // Clean up tier keys for inflights that no longer exist
        var tierKeys = agentdesk.db.query("SELECT key FROM kv_meta WHERE key LIKE 'long_turn_tier:%'");
        for (var tk = 0; tk < tierKeys.length; tk++) {
          var parts = tierKeys[tk].key.split(":");
          var tkProvider = parts[1];
          var tkChannel = parts[2];
          var stillActive = false;
          for (var si = 0; si < inflights.length; si++) {
            if (inflights[si].provider === tkProvider && inflights[si].channel_id === tkChannel) {
              stillActive = true; break;
            }
          }
          if (!stillActive) {
            agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [tierKeys[tk].key]);
          }
        }
        // Also clean up old cooldown keys
        var oldKeys = agentdesk.db.query("SELECT key FROM kv_meta WHERE key LIKE 'long_turn_alert:%'");
        for (var ok = 0; ok < oldKeys.length; ok++) {
          agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [oldKeys[ok].key]);
        }
      } catch(de) {
        agentdesk.log.warn("[long-turn] inflight scan error: " + de);
      }
    };
};
