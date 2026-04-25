/* giant-file-exemption: reason=pre-existing-monitoring-aggregator ticket=#1078 */
// #515: Suppress repeated monitor alerts for the same issue/card and only
// re-notify after a longer interval if the problem is still unresolved.
var ESCALATION_COOLDOWN_SEC = 600;
var ESCALATION_PENDING_TTL_SEC = 600;
var LOOP_GUARD_TTL_SEC = 604800;
var BENIGN_BLOCKED_REASON_PREFIXES = [
  "ci:waiting",
  "ci:running",
  "ci:rerunning",
  "ci:rework",
  // #743: create-pr dispatch in flight — benign progress state
  "pr:creating"
];

function isBenignBlockedReason(reason) {
  if (!reason) return false;
  for (var i = 0; i < BENIGN_BLOCKED_REASON_PREFIXES.length; i++) {
    if (String(reason).indexOf(BENIGN_BLOCKED_REASON_PREFIXES[i]) === 0) {
      return true;
    }
  }
  return false;
}

function manualInterventionFingerprint(status, reviewStatus, blockedReason) {
  if (reviewStatus === "dilemma_pending") {
    return "review:dilemma_pending";
  }
  if (blockedReason && !isBenignBlockedReason(blockedReason)) {
    return "blocked:" + blockedReason;
  }
  return null;
}

function loadManualInterventionState(cardId) {
  var cards = agentdesk.db.query(
    "SELECT status, review_status, blocked_reason FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (cards.length === 0) return null;
  var card = cards[0];
  var cfg = agentdesk.pipeline.getConfig();
  if (agentdesk.pipeline.isTerminal(card.status, cfg)) {
    return {
      status: card.status,
      review_status: card.review_status,
      blocked_reason: card.blocked_reason,
      fingerprint: null,
      active: false
    };
  }
  var fingerprint = manualInterventionFingerprint(card.status, card.review_status, card.blocked_reason);
  return {
    status: card.status,
    review_status: card.review_status,
    blocked_reason: card.blocked_reason,
    fingerprint: fingerprint,
    active: !!fingerprint
  };
}

function escalateToManualIntervention(cardId, reason, options) {
  var state = loadManualInterventionState(cardId);
  if (!state) return;

  var opts = options || {};
  if (state.status === "review" || opts.review === true) {
    agentdesk.kanban.setReviewStatus(cardId, "dilemma_pending", {
      blocked_reason: reason,
      suggestion_pending_at: null,
      awaiting_dod_at: null
    });
    agentdesk.reviewState.sync(cardId, "dilemma_pending", opts.reviewStateSync || {});
  } else {
    agentdesk.db.execute(
      "UPDATE kanban_cards SET blocked_reason = ?, updated_at = datetime('now') WHERE id = ?",
      [reason, cardId]
    );
  }

  agentdesk.log.warn("[manual-intervention] Card " + cardId + " requires manual decision: " + reason);
  if (!opts.skipEscalate) {
    escalate(cardId, reason);
  }
}

function getConfiguredChannelTarget(configKey, purpose) {
  var ch = agentdesk.config.get(configKey);
  if (!ch) {
    agentdesk.log.warn("[notify] No " + configKey + " configured, skipping " + purpose);
    return null;
  }
  return "channel:" + ch;
}

function getHumanAlertChannel() {
  return getConfiguredChannelTarget("kanban_human_alert_channel_id", "human alert");
}

function notifyHumanAlert(message, source) {
  var target = getHumanAlertChannel();
  if (!target) return false;
  agentdesk.message.queue(target, message, "notify", source || "system");
  return true;
}

function getDeadlockManagerChannel() {
  return getConfiguredChannelTarget("deadlock_manager_channel_id", "deadlock alert");
}

function notifyDeadlockManager(message, source) {
  var target = getDeadlockManagerChannel();
  if (target) {
    agentdesk.message.queue(target, message, "announce", source || "system");
    return true;
  }
  return notifyHumanAlert(message, source || "system");
}

function loopGuardNowIso() {
  return new Date().toISOString();
}

function loopGuardNowMs() {
  return new Date().getTime();
}

function loadLoopGuardJson(raw) {
  if (!raw) return null;
  try {
    var parsed = JSON.parse(raw);
    return parsed && typeof parsed === "object" ? parsed : null;
  } catch (e) {
    return null;
  }
}

function loadCardMetadata(cardId) {
  var rows = agentdesk.db.query(
    "SELECT metadata FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (rows.length === 0 || !rows[0].metadata) return {};
  return loadLoopGuardJson(rows[0].metadata) || {};
}

function writeCardMetadata(cardId, metadata) {
  agentdesk.db.execute(
    "UPDATE kanban_cards SET metadata = ?, updated_at = datetime('now') WHERE id = ?",
    [JSON.stringify(metadata || {}), cardId]
  );
}

function mergeObjectPatch(base, patch) {
  var next = {};
  var key;
  var source = (base && typeof base === "object") ? base : {};
  for (key in source) {
    if (Object.prototype.hasOwnProperty.call(source, key)) {
      next[key] = source[key];
    }
  }
  var updates = (patch && typeof patch === "object") ? patch : {};
  for (key in updates) {
    if (Object.prototype.hasOwnProperty.call(updates, key)) {
      next[key] = updates[key];
    }
  }
  return next;
}

function mergeLoopGuardMetadata(cardId, scope, patch) {
  var meta = loadCardMetadata(cardId);
  if (!meta.loop_guard || typeof meta.loop_guard !== "object") {
    meta.loop_guard = {};
  }
  var current = meta.loop_guard[scope];
  if (!current || typeof current !== "object") {
    current = {};
  }
  meta.loop_guard[scope] = mergeObjectPatch(current, patch || {});
  writeCardMetadata(cardId, meta);
  return meta.loop_guard[scope];
}

function loopGuardKvKey(cardId, scope) {
  return "loop_guard:" + scope + ":" + cardId;
}

function loadLoopGuardRecord(cardId, scope) {
  return loadLoopGuardJson(agentdesk.kv.get(loopGuardKvKey(cardId, scope))) || {};
}

function saveLoopGuardRecord(cardId, scope, patch, ttlSec) {
  var next = mergeObjectPatch(loadLoopGuardRecord(cardId, scope), patch || {});
  if (!next.updated_at) {
    next.updated_at = loopGuardNowIso();
  }
  agentdesk.kv.set(
    loopGuardKvKey(cardId, scope),
    JSON.stringify(next),
    ttlSec || LOOP_GUARD_TTL_SEC
  );
  mergeLoopGuardMetadata(cardId, scope, next);
  return next;
}

function replaceLoopGuardRecord(cardId, scope, record, ttlSec) {
  var next = mergeObjectPatch({}, record || {});
  if (!next.updated_at) {
    next.updated_at = loopGuardNowIso();
  }
  agentdesk.kv.set(
    loopGuardKvKey(cardId, scope),
    JSON.stringify(next),
    ttlSec || LOOP_GUARD_TTL_SEC
  );
  mergeLoopGuardMetadata(cardId, scope, next);
  return next;
}

function clearLoopGuardRecord(cardId, scope, metadataPatch) {
  agentdesk.kv.delete(loopGuardKvKey(cardId, scope));
  if (metadataPatch && typeof metadataPatch === "object") {
    mergeLoopGuardMetadata(cardId, scope, metadataPatch);
  }
}

function escalationServerPort() {
  return agentdesk.config.get("server_port");
}

function escalationApiUrl(path) {
  var port = escalationServerPort();
  if (!port) return null;
  return "http://127.0.0.1:" + port + path;
}

function escalationCardTitle(cardId) {
  var cards = agentdesk.db.query(
    "SELECT github_issue_number FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (cards.length === 0) return cardId;
  if (cards[0].github_issue_number) {
    return "#" + cards[0].github_issue_number + " (" + cardId + ")";
  }
  return cardId;
}

function parseCooldownRecord(raw) {
  if (!raw) return null;
  try {
    var parsed = JSON.parse(raw);
    if (parsed && typeof parsed === "object") {
      return {
        sent_at: parseInt(parsed.sent_at, 10) || 0,
        status: parsed.status ? String(parsed.status) : null
      };
    }
  } catch (e) {}
  return {
    sent_at: parseInt(raw, 10) || 0,
    status: null
  };
}

function enqueueEscalation(cardId, reason) {
  if (!cardId || !reason) return;
  var pendingKey = "pm_pending:" + cardId;
  var existing = agentdesk.db.query("SELECT value FROM kv_meta WHERE key = ?", [pendingKey]);
  var entry;
  if (existing.length > 0) {
    try { entry = JSON.parse(existing[0].value); } catch (e) { entry = null; }
  }
  if (!entry) {
    entry = { title: escalationCardTitle(cardId), reasons: [] };
  }
  if (!entry.title) {
    entry.title = escalationCardTitle(cardId);
  }
  if (entry.reasons.indexOf(reason) === -1) {
    entry.reasons.push(reason);
  }
  agentdesk.db.execute(
    "INSERT OR REPLACE INTO kv_meta (key, value, expires_at) VALUES (?, ?, datetime('now', '+' || ? || ' seconds'))",
    [pendingKey, JSON.stringify(entry), String(ESCALATION_PENDING_TTL_SEC)]
  );
}

function escalate(cardId, reasons) {
  if (Array.isArray(reasons)) {
    for (var i = 0; i < reasons.length; i++) {
      enqueueEscalation(cardId, reasons[i]);
    }
    return;
  }
  enqueueEscalation(cardId, reasons);
}

function flushEscalations() {
  var apiUrl = escalationApiUrl("/api/internal/escalation/emit");
  if (!apiUrl) {
    agentdesk.log.warn("[escalation] server_port missing — cannot flush pending escalations");
    return;
  }

  var rows = agentdesk.db.query("SELECT key, value FROM kv_meta WHERE key LIKE 'pm_pending:%'");
  for (var i = 0; i < rows.length; i++) {
    var cardId = rows[i].key.substring("pm_pending:".length);
    var state = loadManualInterventionState(cardId);
    var cooldownKey = "pm_decision_sent:" + cardId;
    if (!state) {
      agentdesk.db.execute("DELETE FROM kv_meta WHERE key IN (?1, ?2)", [rows[i].key, cooldownKey]);
      continue;
    }
    if (!state.active) {
      agentdesk.db.execute("DELETE FROM kv_meta WHERE key IN (?1, ?2)", [rows[i].key, cooldownKey]);
      continue;
    }

    var entry;
    try { entry = JSON.parse(rows[i].value); } catch (e) {
      agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [rows[i].key]);
      continue;
    }

    var cooldownRows = agentdesk.db.query("SELECT value FROM kv_meta WHERE key = ?", [cooldownKey]);
    if (cooldownRows.length > 0) {
      var cooldownRecord = parseCooldownRecord(cooldownRows[0].value);
      var sentAt = cooldownRecord ? cooldownRecord.sent_at : 0;
      var now = Math.floor(Date.now() / 1000);
      var sameAlertState = !cooldownRecord || !cooldownRecord.status || cooldownRecord.status === state.fingerprint;
      if (sameAlertState && now - sentAt < ESCALATION_COOLDOWN_SEC) {
        agentdesk.log.info("[escalation] cooldown skip for " + cardId + " (" + (now - sentAt) + "s)");
        continue;
      }
    }

    var resp = agentdesk.http.post(apiUrl, {
      card_id: cardId,
      reasons: entry && Array.isArray(entry.reasons) ? entry.reasons : []
    });
    if (!resp || resp.error) {
      agentdesk.log.warn("[escalation] emit failed for " + cardId + ": " + (resp && resp.error ? resp.error : "unknown error"));
      continue;
    }

    var sentAt = Math.floor(Date.now() / 1000);
    agentdesk.db.execute(
      "INSERT OR REPLACE INTO kv_meta (key, value, expires_at) VALUES (?, ?, datetime('now', '+' || ? || ' seconds'))",
      [cooldownKey, JSON.stringify({ sent_at: sentAt, status: state.fingerprint }), String(ESCALATION_COOLDOWN_SEC)]
    );
    agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [rows[i].key]);
  }
}
