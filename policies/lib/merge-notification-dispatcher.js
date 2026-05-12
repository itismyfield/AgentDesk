/** @module policies/lib/merge-notification-dispatcher
 *
 * #1078: Extracted from merge-automation.js as part of the policy modularization pass.
 *
 * Discord/notification surfaces for merge-automation: Codex review summaries,
 * merge-failure announcements, and conflict notifications routed to the
 * agent's primary or active channel. Dedup keys are namespaced and TTL-gated
 * via `agentdesk.kv`.
 *
 * Depends on the global `agentdesk` surface (db/kv/message/agents/log) — the
 * test harness injects mocks through the same global.
 */

var _mergeTextUtils = require("./merge-text-utils");
var _sanitizeKvKeyPart = _mergeTextUtils.sanitizeKvKeyPart;
var _summarizeInlineText = _mergeTextUtils.summarizeInlineText;

var CODEX_NOTIFICATION_TTL_SECONDS = 6 * 60 * 60;

function codexNotificationDedupKey(repo, prNumber, reviewId, kind) {
  return "codex_review_notified:" +
    _sanitizeKvKeyPart(kind) + ":" +
    _sanitizeKvKeyPart(repo) + ":" +
    _sanitizeKvKeyPart(prNumber) + ":" +
    _sanitizeKvKeyPart(reviewId);
}

function mergeGuardDedupKey(repo, prNumber, reviewId) {
  return "codex_merge_guard:" +
    _sanitizeKvKeyPart(repo) + ":" +
    _sanitizeKvKeyPart(prNumber) + ":" +
    _sanitizeKvKeyPart(reviewId);
}

function resolveCodexNotificationTarget(card) {
  if (!card) return null;

  try {
    var unified = agentdesk.db.query(
      "SELECT r.unified_thread_channel_id FROM auto_queue_entries e " +
      "JOIN auto_queue_runs r ON r.id = e.run_id " +
      "WHERE e.kanban_card_id = ? AND r.unified_thread_channel_id IS NOT NULL " +
      "ORDER BY r.created_at DESC LIMIT 1",
      [card.id]
    );
    if (unified.length > 0 && unified[0].unified_thread_channel_id) {
      return unified[0].unified_thread_channel_id;
    }
  } catch (e) {}

  if (card.active_thread_id) return card.active_thread_id;

  if (card.assigned_agent_id) {
    var sessions = agentdesk.db.query(
      "SELECT thread_channel_id FROM sessions WHERE agent_id = ? AND thread_channel_id IS NOT NULL " +
      "ORDER BY last_heartbeat DESC LIMIT 1",
      [card.assigned_agent_id]
    );
    if (sessions.length > 0 && sessions[0].thread_channel_id) {
      return sessions[0].thread_channel_id;
    }

    var primary = agentdesk.agents.resolvePrimaryChannel(card.assigned_agent_id);
    if (primary) return primary;
  }

  return null;
}

function buildCodexReviewMessage(pr, snapshot, followUpIssue, mergeGuarded) {
  var lines = [];
  if (snapshot.hasBlocking) {
    lines.push("⚠️ PR #" + pr.number + " Codex 리뷰: unresolved P1/P2 " + snapshot.blockingComments.length + "건");
    if (snapshot.blockingFiles.length > 0) {
      lines.push("파일: " + snapshot.blockingFiles.join(", "));
    }
    for (var i = 0; i < snapshot.blockingComments.length && i < 3; i++) {
      var c = snapshot.blockingComments[i];
      lines.push("- " + c.path + ":" + c.line + " " + c.body);
    }
    if (snapshot.blockingComments.length > 3) {
      lines.push("- 외 " + (snapshot.blockingComments.length - 3) + "건");
    }
    if (followUpIssue) {
      var ref = followUpIssue.number ? "#" + followUpIssue.number : "생성 완료";
      lines.push("follow-up 이슈를 생성했습니다: " + ref);
      if (followUpIssue.url) {
        lines.push(followUpIssue.url);
      }
    } else if (mergeGuarded) {
      lines.push("merge를 차단했습니다.");
    }
  } else {
    lines.push("✅ PR #" + pr.number + " Codex 리뷰 통과");
    lines.push("blocking inline comment 없음");
  }
  return lines.join("\n");
}

function notifyCodexReview(card, pr, snapshot, kind, followUpIssue, mergeGuarded) {
  var target = resolveCodexNotificationTarget(card);
  if (!target) return;

  var dedupKey = codexNotificationDedupKey(pr.repo || "", pr.number, snapshot.triggerReviewId || snapshot.latestReviewId, kind);
  if (agentdesk.kv.get(dedupKey)) return;

  agentdesk.message.queue(
    target,
    buildCodexReviewMessage(pr, snapshot, followUpIssue, mergeGuarded),
    "announce",
    "merge-automation"
  );
  agentdesk.kv.set(dedupKey, "true", CODEX_NOTIFICATION_TTL_SECONDS);
}

/**
 * Notify the assigned agent's channel that an auto-merge attempt failed.
 * `loadCardContext` is injected by the caller (still resides in merge-automation.js)
 * so this module stays free of card-resolution policy code.
 */
function notifyMergeFailure(cardId, prNumber, repo, reason, loadCardContext) {
  if (!cardId) return;

  var dedupKey = "merge_failure_notified:" + cardId + ":" + prNumber;
  if (agentdesk.kv.get(dedupKey)) return;

  var card = loadCardContext(cardId);
  if (!card) return;

  var target = card.active_thread_id;
  if (!target && card.assigned_agent_id) {
    target = agentdesk.agents.resolvePrimaryChannel(card.assigned_agent_id);
  }
  if (!target) {
    var pmdChannel = agentdesk.config.get("kanban_manager_channel_id");
    if (pmdChannel) {
      target = "channel:" + pmdChannel;
    }
  }
  if (!target) return;

  var titleRef = card.github_issue_number
    ? ("#" + card.github_issue_number + " " + (card.title || card.id))
    : (card.title || card.id);
  agentdesk.message.queue(
    target,
    "⚠️ " + titleRef + "\n" +
      "PR #" + prNumber + " auto-merge failed in `" + repo + "`.\n" +
      "Reason: " + _summarizeInlineText(reason) + "\n" +
      "수동 확인이 필요합니다.",
    "announce",
    "merge-automation"
  );
  agentdesk.kv.set(dedupKey, "true", 7200);
}

function notifyAgentMainChannel(agentId, prNum, title) {
  var kvKey = "conflict_notified:" + prNum;
  if (agentdesk.kv.get(kvKey)) return;

  // #304: resolve primary channel via centralized resolver
  var mainCh = agentdesk.agents.resolvePrimaryChannel(agentId);
  if (mainCh) {
    agentdesk.message.queue(
      mainCh,
      "⚠️ PR #" + prNum + " (" + title + ") has merge conflicts with main. Please rebase.",
      "announce",
      "merge-automation"
    );
  }
  agentdesk.kv.set(kvKey, "true", 7200); // 2h TTL
}

module.exports = {
  CODEX_NOTIFICATION_TTL_SECONDS: CODEX_NOTIFICATION_TTL_SECONDS,
  codexNotificationDedupKey: codexNotificationDedupKey,
  mergeGuardDedupKey: mergeGuardDedupKey,
  resolveCodexNotificationTarget: resolveCodexNotificationTarget,
  buildCodexReviewMessage: buildCodexReviewMessage,
  notifyCodexReview: notifyCodexReview,
  notifyMergeFailure: notifyMergeFailure,
  notifyAgentMainChannel: notifyAgentMainChannel
};
