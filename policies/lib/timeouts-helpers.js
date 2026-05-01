/* giant-file-exemption: reason=shared-helpers-bundle ticket=#1078 */
/** @module policies/lib/timeouts-helpers */
function sendDeadlockAlert(message) {
  return notifyDeadlockManager(message, "timeouts");
}

// Shared constant used by sections [A] and [J]
var MAX_DISPATCH_RETRIES = 10;

// Helper: read timeout config as SQL interval string
function getTimeoutInterval(key, fallbackMinutes) {
  var val = parseInt(agentdesk.config.get(key), 10);
  if (!val || val <= 0) val = fallbackMinutes;
  return "-" + val + " minutes";
}

function latestCardActivityExpr(cardAlias, dispatchAlias) {
  return "MAX(COALESCE(" + dispatchAlias + ".created_at, ''), COALESCE(" + cardAlias + ".updated_at, ''), COALESCE(" + cardAlias + ".started_at, ''))";
}

function parseLocalTimestampMs(value) {
  if (!value || typeof value !== "string") return 0;
  var trimmed = value.trim();
  var m = /^(\d{4})-(\d{2})-(\d{2})[ T](\d{2}):(\d{2}):(\d{2})$/.exec(trimmed);
  if (m) {
    return new Date(
      parseInt(m[1], 10),
      parseInt(m[2], 10) - 1,
      parseInt(m[3], 10),
      parseInt(m[4], 10),
      parseInt(m[5], 10),
      parseInt(m[6], 10)
    ).getTime();
  }
  var parsed = Date.parse(trimmed);
  return isNaN(parsed) ? 0 : parsed;
}

function normalizedText(value) {
  if (value === null || value === undefined) return null;
  var trimmed = String(value).trim();
  return trimmed ? trimmed : null;
}

function parseSessionTmuxName(sessionKey) {
  var raw = normalizedText(sessionKey);
  if (!raw) return null;
  var idx = raw.lastIndexOf(":");
  return idx >= 0 ? normalizedText(raw.substring(idx + 1)) : raw;
}

function parseSessionChannelName(sessionKey, provider) {
  var tmuxName = parseSessionTmuxName(sessionKey);
  if (!tmuxName) return null;
  var prefix = "AgentDesk-" + (normalizedText(provider) || "") + "-";
  var channelName = tmuxName.indexOf(prefix) === 0
    ? tmuxName.substring(prefix.length)
    : tmuxName.replace(/^AgentDesk-[^-]+-/, "");
  if (channelName.slice(-4) === "-dev") {
    channelName = channelName.substring(0, channelName.length - 4);
  }
  return normalizedText(channelName);
}

function parseParentChannelName(channelName) {
  var raw = normalizedText(channelName);
  if (!raw) return null;
  var match = /^(.*)-t\d{15,}$/.exec(raw);
  return match ? normalizedText(match[1]) : raw;
}

function parseSessionThreadId(sessionKey, provider) {
  var channelName = parseSessionChannelName(sessionKey, provider);
  var match = channelName ? /-t(\d{15,})$/.exec(channelName) : null;
  return match ? match[1] : null;
}

function loadAgentDirectory() {
  return agentdesk.db.query(
    "SELECT id, name, name_ko, discord_channel_id, discord_channel_alt, discord_channel_cc, discord_channel_cdx FROM agents"
  );
}

function agentDisplayName(agent) {
  if (!agent) return null;
  return normalizedText(agent.name_ko) || normalizedText(agent.name) || normalizedText(agent.id);
}

function findAgentById(agents, agentId) {
  var target = normalizedText(agentId);
  if (!target) return null;
  for (var i = 0; i < agents.length; i++) {
    if (normalizedText(agents[i].id) === target) return agents[i];
  }
  return null;
}

function channelMatchesCandidate(candidate, channel) {
  var left = normalizedText(candidate);
  var right = normalizedText(channel);
  if (!left || !right) return false;
  return left === right || left.indexOf(right) === 0 || right.indexOf(left) === 0;
}

function findAgentByChannelValue(agents, channelValue) {
  for (var i = 0; i < agents.length; i++) {
    var agent = agents[i];
    var channels = [
      agent.discord_channel_id,
      agent.discord_channel_alt,
      agent.discord_channel_cc,
      agent.discord_channel_cdx
    ];
    for (var c = 0; c < channels.length; c++) {
      if (channelMatchesCandidate(channelValue, channels[c])) {
        return agent;
      }
    }
  }
  return null;
}

function buildChannelTarget(rawChannel) {
  var channel = normalizedText(rawChannel);
  return channel ? ("channel:" + channel) : null;
}

function resolveAgentNotifyTarget(agent, provider) {
  if (!agent) return null;
  var normalizedProviderValue = normalizedText(provider);
  var candidates = [];
  if (normalizedProviderValue === "claude") {
    candidates.push(agent.discord_channel_cc);
  } else if (normalizedProviderValue === "codex") {
    candidates.push(agent.discord_channel_cdx);
  }
  candidates.push(agent.discord_channel_id);
  candidates.push(agent.discord_channel_alt);
  candidates.push(agent.discord_channel_cc);
  candidates.push(agent.discord_channel_cdx);
  for (var i = 0; i < candidates.length; i++) {
    var target = buildChannelTarget(candidates[i]);
    if (target) return target;
  }
  return null;
}

function lookupDispatchTargetAgentId(dispatchId) {
  var target = normalizedText(dispatchId);
  if (!target) return null;
  var rows = agentdesk.db.query(
    "SELECT to_agent_id FROM task_dispatches WHERE id = ? LIMIT 1",
    [target]
  );
  return rows.length > 0 ? normalizedText(rows[0].to_agent_id) : null;
}

function lookupThreadTargetAgentId(threadId) {
  var target = normalizedText(threadId);
  if (!target) return null;
  var rows = agentdesk.db.query(
    "SELECT to_agent_id FROM task_dispatches " +
    "WHERE thread_id = ? AND to_agent_id IS NOT NULL AND TRIM(to_agent_id) != '' " +
    "ORDER BY created_at DESC LIMIT 1",
    [target]
  );
  return rows.length > 0 ? normalizedText(rows[0].to_agent_id) : null;
}

function resolveSessionAgentContext(sessionRow, agents) {
  var storedAgentId = normalizedText(sessionRow.agent_id);
  var resolvedAgent = findAgentById(agents, storedAgentId);
  var dispatchAgentId = lookupDispatchTargetAgentId(sessionRow.active_dispatch_id);
  if (!resolvedAgent && dispatchAgentId) {
    resolvedAgent = findAgentById(agents, dispatchAgentId);
  }

  var threadChannelId = normalizedText(sessionRow.thread_channel_id) ||
    parseSessionThreadId(sessionRow.session_key, sessionRow.provider);
  if (!resolvedAgent && threadChannelId) {
    resolvedAgent = findAgentByChannelValue(agents, threadChannelId);
    if (!resolvedAgent) {
      var threadAgentId = lookupThreadTargetAgentId(threadChannelId);
      if (threadAgentId) {
        resolvedAgent = findAgentById(agents, threadAgentId);
      }
    }
  }

  var sessionChannelName = parseParentChannelName(
    parseSessionChannelName(sessionRow.session_key, sessionRow.provider)
  );
  if (!resolvedAgent && sessionChannelName) {
    resolvedAgent = findAgentByChannelValue(agents, sessionChannelName);
  }

  var resolvedAgentId = resolvedAgent
    ? normalizedText(resolvedAgent.id)
    : (storedAgentId || null);
  var resolvedLabel = agentDisplayName(resolvedAgent) ||
    sessionChannelName ||
    parseSessionTmuxName(sessionRow.session_key) ||
    "unknown-session";
  var notifyTarget = resolvedAgent
    ? resolveAgentNotifyTarget(resolvedAgent, sessionRow.provider)
    : null;

  return {
    agent_id: resolvedAgentId,
    agent_label: resolvedLabel,
    thread_channel_id: threadChannelId,
    session_channel_name: sessionChannelName,
    notify_target: notifyTarget
  };
}

function backfillMissingSessionAgentIds(agents) {
  var rows = agentdesk.db.query(
    "SELECT session_key, agent_id, provider, active_dispatch_id, thread_channel_id " +
    "FROM sessions " +
    "WHERE provider IN ('claude', 'codex', 'qwen') " +
    "AND (agent_id IS NULL OR TRIM(agent_id) = '')"
  );
  for (var i = 0; i < rows.length; i++) {
    var resolved = resolveSessionAgentContext(rows[i], agents);
    if (!resolved.agent_id) continue;
    agentdesk.db.execute(
      "UPDATE sessions SET agent_id = ? WHERE session_key = ? AND (agent_id IS NULL OR TRIM(agent_id) = '')",
      [resolved.agent_id, rows[i].session_key]
    );
  }
}

function findRecentInflightForSession(sessionKey, tmuxName) {
  var inflights = [];
  try {
    inflights = agentdesk.inflight.list() || [];
  } catch(e) {
    throw e;
  }
  var best = null;
  var bestUpdatedAt = 0;
  for (var i = 0; i < inflights.length; i++) {
    var inf = inflights[i];
    if (!inf) continue;
    if (isSyntheticMissingInflightReattachPlaceholder(inf)) continue;
    var sessionMatch = !!sessionKey && inf.session_key === sessionKey;
    var tmuxMatch = !!tmuxName && inf.tmux_session_name === tmuxName;
    if (!sessionMatch && !tmuxMatch) continue;
    var updatedAtMs = parseLocalTimestampMs(inf.updated_at);
    if (!best || updatedAtMs >= bestUpdatedAt) {
      best = inf;
      bestUpdatedAt = updatedAtMs;
    }
  }
  return best;
}

function inspectInflightProgress(sessionKey, tmuxName, recentWindowMin, maxTurnMin) {
  var inflight;
  try {
    inflight = findRecentInflightForSession(sessionKey, tmuxName);
  } catch (e) {
    inflight = null; // revert to null on throw so that the rest of inspectInflightProgress works
  }
  if (!inflight) {
    return {
      inflight: null,
      recent: false,
      updated_age_min: null,
      turn_age_min: null,
      channel_id: null,
      max_turn_reached: false
    };
  }
  var nowMs = Date.now();
  var updatedAtMs = parseLocalTimestampMs(inflight.updated_at);
  var startedAtMs = parseLocalTimestampMs(inflight.started_at);
  var updatedAgeMin = updatedAtMs > 0 ? (nowMs - updatedAtMs) / 60000 : null;
  var turnAgeMin = startedAtMs > 0 ? (nowMs - startedAtMs) / 60000 : null;
  return {
    inflight: inflight,
    recent: updatedAgeMin !== null && updatedAgeMin <= recentWindowMin,
    updated_age_min: updatedAgeMin,
    turn_age_min: turnAgeMin,
    channel_id: inflight.channel_id || null,
    max_turn_reached: turnAgeMin !== null && turnAgeMin >= maxTurnMin
  };
}

function isZeroPlaceholderId(value) {
  return value === 0 || value === "0";
}

function isSyntheticMissingInflightReattachPlaceholder(inf) {
  if (!inf) return false;
  return inf.rebind_origin === true &&
    inf.session_id == null &&
    isZeroPlaceholderId(inf.request_owner_user_id) &&
    isZeroPlaceholderId(inf.user_msg_id) &&
    inf.any_tool_used === false &&
    inf.has_post_tool_text === false;
}

function requestTurnWatchdogExtension(channelId, extendMinutes) {
  if (!channelId) return { ok: false, error: "channel_id missing" };
  var apiPort = agentdesk.config.get("server_port");
  if (!apiPort) return { ok: false, error: "server_port missing" };
  var extendSecs = Math.max(1, Math.round(extendMinutes * 60));
  var url = "http://127.0.0.1:" + apiPort +
    "/api/turns/" + encodeURIComponent(channelId) + "/extend-timeout";
  var resp = agentdesk.http.post(url, { extend_secs: extendSecs });
  if (!resp || resp.error) {
    return { ok: false, error: resp && resp.error ? resp.error : "unknown error" };
  }
  return resp;
}

function _queuePMDecision(cardId, title, reason) {
  escalate(cardId, reason);
}

function _flushPMDecisions() {
  flushEscalations();
}

module.exports = {
  sendDeadlockAlert: sendDeadlockAlert,
  MAX_DISPATCH_RETRIES: MAX_DISPATCH_RETRIES,
  getTimeoutInterval: getTimeoutInterval,
  latestCardActivityExpr: latestCardActivityExpr,
  parseLocalTimestampMs: parseLocalTimestampMs,
  normalizedText: normalizedText,
  parseSessionTmuxName: parseSessionTmuxName,
  parseSessionChannelName: parseSessionChannelName,
  parseParentChannelName: parseParentChannelName,
  parseSessionThreadId: parseSessionThreadId,
  loadAgentDirectory: loadAgentDirectory,
  agentDisplayName: agentDisplayName,
  findAgentById: findAgentById,
  channelMatchesCandidate: channelMatchesCandidate,
  findAgentByChannelValue: findAgentByChannelValue,
  buildChannelTarget: buildChannelTarget,
  resolveAgentNotifyTarget: resolveAgentNotifyTarget,
  lookupDispatchTargetAgentId: lookupDispatchTargetAgentId,
  lookupThreadTargetAgentId: lookupThreadTargetAgentId,
  resolveSessionAgentContext: resolveSessionAgentContext,
  backfillMissingSessionAgentIds: backfillMissingSessionAgentIds,
  isSyntheticMissingInflightReattachPlaceholder: isSyntheticMissingInflightReattachPlaceholder,
  findRecentInflightForSession: findRecentInflightForSession,
  inspectInflightProgress: inspectInflightProgress,
  requestTurnWatchdogExtension: requestTurnWatchdogExtension,
  _queuePMDecision: _queuePMDecision,
  _flushPMDecisions: _flushPMDecisions
};
