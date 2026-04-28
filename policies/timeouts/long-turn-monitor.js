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
      // Notify at every configured-minute multiple while storing the last
      // alerted threshold minute to avoid repeating the same bucket.
      var configuredAlertInterval = parseInt(agentdesk.config.get("long_turn_alert_interval_min"), 10);
      var ALERT_INTERVAL_MINUTES = configuredAlertInterval > 0 ? configuredAlertInterval : 30;
      var WATCHDOG_EXTENSION_MINUTES = 60;
      var WATCHDOG_EXTENSION_COOLDOWN_MINUTES = 20;
      var WATCHDOG_EXTENSION_RECENT_PROGRESS_MINUTES = 5;
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
          var currentThreshold = Math.floor(elapsedMin / ALERT_INTERVAL_MINUTES) * ALERT_INTERVAL_MINUTES;
          if (currentThreshold < ALERT_INTERVAL_MINUTES) continue; // under 30min, skip
          // Check if we already alerted at this tier
          var tierKey = "long_turn_tier:" + inf.provider + ":" + inf.channel_id;
          var lastTier = agentdesk.db.query("SELECT value FROM kv_meta WHERE key = ?", [tierKey]);
          var lastAlertedThreshold = -1;
          if (lastTier.length > 0) {
            var rawLastTier = parseInt(lastTier[0].value, 10);
            if (!isNaN(rawLastTier)) {
              // Backward compatibility for pre-30-minute cadence values:
              // old tier index 0/1/2 meant 30/60/120 minutes.
              if (rawLastTier === 0) lastAlertedThreshold = 30;
              else if (rawLastTier === 1) lastAlertedThreshold = 60;
              else if (rawLastTier === 2) lastAlertedThreshold = 120;
              else lastAlertedThreshold = rawLastTier;
            }
          }
          var updatedAtMs = parseLocalTimestampMs(inf.updated_at);
          var updatedAgeMin = updatedAtMs > 0 ? (Date.now() - updatedAtMs) / 60000 : null;
          var recentProgress = updatedAgeMin !== null && updatedAgeMin <= WATCHDOG_EXTENSION_RECENT_PROGRESS_MINUTES;
          var extensionLine = "";
          var extensionKey = "long_turn_watchdog_extension:" + inf.provider + ":" + inf.channel_id;
          var lastExtensionRows = agentdesk.db.query("SELECT value FROM kv_meta WHERE key = ?", [extensionKey]);
          var lastExtensionAt = lastExtensionRows.length > 0 ? parseInt(lastExtensionRows[0].value, 10) : 0;
          var nowMs = Date.now();
          var extensionCooldownElapsed = !lastExtensionAt ||
            (nowMs - lastExtensionAt) >= WATCHDOG_EXTENSION_COOLDOWN_MINUTES * 60 * 1000;
          if (recentProgress) {
            if (extensionCooldownElapsed) {
              var extendResp = requestTurnWatchdogExtension(inf.channel_id, WATCHDOG_EXTENSION_MINUTES);
              if (extendResp.ok) {
                extensionLine = "\nwatchdog: +" + WATCHDOG_EXTENSION_MINUTES + "분 연장 요청 완료";
                agentdesk.db.execute(
                  "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
                  [extensionKey, "" + nowMs]
                );
                agentdesk.log.info("[long-turn] " + (inf.channel_name || inf.channel_id) +
                  " — recent inflight progress; extended watchdog +" + WATCHDOG_EXTENSION_MINUTES + "min");
              } else {
                extensionLine = "\nwatchdog: 연장 실패 — " + extendResp.error;
                agentdesk.log.warn("[long-turn] " + (inf.channel_name || inf.channel_id) +
                  " — recent inflight progress but watchdog extension failed: " + extendResp.error);
              }
            } else {
              extensionLine = "\nwatchdog: 최근 progress 확인, 최근 연장 cooldown 중";
              agentdesk.log.info("[long-turn] " + (inf.channel_name || inf.channel_id) +
                " — recent inflight progress; watchdog extension cooldown still active");
            }
          } else if (updatedAgeMin !== null) {
            extensionLine = "\nwatchdog: 최근 progress 없음 (" + Math.round(updatedAgeMin) + "분 전) — 연장 안 함";
          } else {
            extensionLine = "\nwatchdog: progress timestamp 없음 — 연장 안 함";
          }
          if (currentThreshold <= lastAlertedThreshold) continue; // already alerted at this threshold or higher
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
            "경과: " + Math.round(elapsedMin) + "분 (" + currentThreshold + "분 단계)\n" +
            "provider: " + (inf.provider || "?") +
            extensionLine
          );
          agentdesk.db.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
            [tierKey, "" + currentThreshold]
          );
          agentdesk.log.warn("[long-turn] " + (inf.channel_name || inf.channel_id) + " — " + Math.round(elapsedMin) + "min (" + currentThreshold + "min threshold)");
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
        var extensionKeys = agentdesk.db.query("SELECT key FROM kv_meta WHERE key LIKE 'long_turn_watchdog_extension:%'");
        for (var ek = 0; ek < extensionKeys.length; ek++) {
          var eParts = extensionKeys[ek].key.split(":");
          var eProvider = eParts[1];
          var eChannel = eParts[2];
          var extensionStillActive = false;
          for (var ei = 0; ei < inflights.length; ei++) {
            if (inflights[ei].provider === eProvider && inflights[ei].channel_id === eChannel) {
              extensionStillActive = true; break;
            }
          }
          if (!extensionStillActive) {
            agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [extensionKeys[ek].key]);
          }
        }
      } catch(de) {
        agentdesk.log.warn("[long-turn] inflight scan error: " + de);
      }
    };
};
