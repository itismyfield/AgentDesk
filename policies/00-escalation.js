var ESCALATION_COOLDOWN_SEC = 300;
var ESCALATION_PENDING_TTL_SEC = 600;

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
    "SELECT title, github_issue_number FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (cards.length === 0) return cardId;
  if (cards[0].github_issue_number) {
    return "#" + cards[0].github_issue_number + " " + cards[0].title;
  }
  return cards[0].title || cardId;
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
    var entry;
    try { entry = JSON.parse(rows[i].value); } catch (e) { continue; }
    agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [rows[i].key]);

    var cooldownKey = "pm_decision_sent:" + cardId;
    var cooldownRows = agentdesk.db.query("SELECT value FROM kv_meta WHERE key = ?", [cooldownKey]);
    if (cooldownRows.length > 0) {
      var sentAt = parseInt(cooldownRows[0].value, 10) || 0;
      var now = Math.floor(Date.now() / 1000);
      if (now - sentAt < ESCALATION_COOLDOWN_SEC) {
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
      if (entry && Array.isArray(entry.reasons)) {
        escalate(cardId, entry.reasons);
      }
      continue;
    }

    agentdesk.db.execute(
      "INSERT OR REPLACE INTO kv_meta (key, value, expires_at) VALUES (?, ?, datetime('now', '+' || ? || ' seconds'))",
      [cooldownKey, String(Math.floor(Date.now() / 1000)), String(ESCALATION_COOLDOWN_SEC)]
    );
  }
}
