/** @module policies/lib/kanban-notifications
 *
 * #1078: Extracted from kanban-rules.js as part of the policy modularization pass.
 *
 * Provides Discord/quality notification helpers used by the core kanban
 * lifecycle hooks:
 *   - sendDiscordNotification: thin wrapper over agentdesk.message.queue
 *   - emitQualityEvent: optional emit on agentdesk.quality.emit
 *   - _loadCardAlertContext / _formatCardAlertLabel: card label hydration
 *   - notifyCardOwner: routes alerts to the assigned agent or escalates to human
 *
 * The helpers intentionally depend on the global `agentdesk.*` surface and the
 * global `notifyHumanAlert` (injected by the runtime / test harness).
 */

function sendDiscordNotification(target, content, bot) {
  agentdesk.message.queue(target, content, bot || "announce", "system");
}

function emitQualityEvent(event) {
  if (agentdesk.quality && typeof agentdesk.quality.emit === "function") {
    agentdesk.quality.emit(event);
  }
}

function _loadCardAlertContext(cardId) {
  var card = agentdesk.cards.get(cardId);
  if (!card) return null;
  return {
    card_id: cardId,
    assigned_agent_id: card.assigned_agent_id || null,
    title: card.title || cardId,
    github_issue_number: card.github_issue_number || null
  };
}

function _formatCardAlertLabel(card) {
  if (!card) return null;
  if (card.github_issue_number) {
    return "#" + card.github_issue_number + " " + (card.title || card.card_id);
  }
  return card.title || card.card_id;
}

function notifyCardOwner(cardId, reason, source) {
  var card = _loadCardAlertContext(cardId);
  var src = source || "system";
  if (!card) {
    agentdesk.log.warn("[notify] Card not found for owner notification: " + cardId);
    return notifyHumanAlert("⚠️ 카드 " + cardId + "\n" + reason, src);
  }

  var message = "⚠️ " + _formatCardAlertLabel(card) + "\n" + reason;
  if (!card.assigned_agent_id) {
    agentdesk.log.warn("[notify] Card " + cardId + " has no assigned agent — escalating to human");
    return notifyHumanAlert(message + "\n담당 에이전트가 없어 사람이 확인해야 합니다.", src);
  }

  var target = agentdesk.agents.resolvePrimaryChannel(card.assigned_agent_id);
  if (!target) {
    agentdesk.log.warn(
      "[notify] No primary channel for assigned agent " + card.assigned_agent_id +
      " on card " + cardId + " — escalating to human"
    );
    return notifyHumanAlert(message + "\n담당 에이전트 채널을 찾지 못해 사람이 확인해야 합니다.", src);
  }

  agentdesk.message.queue(target, message, "announce", src);
  return true;
}

module.exports = {
  sendDiscordNotification: sendDiscordNotification,
  emitQualityEvent: emitQualityEvent,
  _loadCardAlertContext: _loadCardAlertContext,
  _formatCardAlertLabel: _formatCardAlertLabel,
  notifyCardOwner: notifyCardOwner
};
