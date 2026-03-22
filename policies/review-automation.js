/**
 * review-automation.js — ADK Policy: Review Lifecycle
 * priority: 50
 *
 * Hooks:
 *   onReviewEnter       — 카운터모델 리뷰 디스패치 생성
 *   onDispatchCompleted — review/decision dispatch 완료 → verdict 처리
 *   onReviewVerdict     — 외부 API verdict 수신 처리
 */

function sendDiscordReview(target, content, bot) {
  try {
    var port = agentdesk.config.get("server_port") || 8791;
    agentdesk.http.post("http://127.0.0.1:" + port + "/api/send", {
      target: target, content: content, bot: bot || "announce"
    });
  } catch (e) {
    agentdesk.log.warn("[review] Discord send failed: " + e);
  }
}

var reviewAutomation = {
  name: "review-automation",
  priority: 50,

  // ── Review Enter — counter-model review trigger ───────────
  onReviewEnter: function(payload) {
    var cards = agentdesk.db.query(
      "SELECT id, repo_id, assigned_agent_id, review_round, deferred_dod_json FROM kanban_cards WHERE id = ?",
      [payload.card_id]
    );
    if (cards.length === 0) return;
    var card = cards[0];

    // Check if review is enabled — if not, route to PM decision (not silent done)
    var reviewEnabled = agentdesk.config.get("review_enabled");
    if (reviewEnabled === "false" || reviewEnabled === false) {
      agentdesk.kanban.setStatus(card.id, "pending_decision");
      agentdesk.db.execute(
        "UPDATE kanban_cards SET blocked_reason = 'Review disabled — PM decision needed to proceed' WHERE id = ?",
        [card.id]
      );
      agentdesk.db.execute(
        "UPDATE kanban_cards SET review_status = NULL WHERE id = ?",
        [card.id]
      );
      agentdesk.log.info("[review] Review disabled, card " + card.id + " → pending_decision");
      return;
    }

    // Increment review round
    var newRound = (card.review_round || 0) + 1;
    agentdesk.db.execute(
      "UPDATE kanban_cards SET review_round = ?, review_status = 'reviewing', updated_at = datetime('now') WHERE id = ?",
      [newRound, card.id]
    );

    // Check review round limit — exceed → pending_decision with PMD notification
    var maxRounds = agentdesk.config.get("max_review_rounds") || 3;
    if (newRound > maxRounds) {
      agentdesk.kanban.setStatus(card.id, "pending_decision");
      agentdesk.db.execute(
        "UPDATE kanban_cards SET review_status = 'dilemma_pending', blocked_reason = ? WHERE id = ?",
        ["Max review rounds (" + maxRounds + ") exceeded — PM decision needed", card.id]
      );
      agentdesk.log.warn("[review] Max review rounds (" + maxRounds + ") reached for " + card.id + " → pending_decision");
      return;
    }

    // Counter-model review: send to alternate channel (Claude↔Codex pair)
    var counterModelEnabled = agentdesk.config.get("counter_model_review_enabled");
    if (counterModelEnabled === false || counterModelEnabled === "false") {
      agentdesk.kanban.setStatus(card.id, "pending_decision");
      agentdesk.db.execute(
        "UPDATE kanban_cards SET blocked_reason = 'Counter-model review disabled — PM decision needed' WHERE id = ?",
        [card.id]
      );
      agentdesk.db.execute(
        "UPDATE kanban_cards SET review_status = NULL WHERE id = ?",
        [card.id]
      );
      agentdesk.log.info("[review] Counter-model disabled, card " + card.id + " → pending_decision");
      return;
    }

    if (!card.assigned_agent_id) return;

    // Get agent's alternate channel (CDX for Claude agents, CC for Codex)
    var agentRow = agentdesk.db.query(
      "SELECT discord_channel_id, discord_channel_alt FROM agents WHERE id = ?",
      [card.assigned_agent_id]
    );
    if (agentRow.length === 0 || !agentRow[0].discord_channel_alt) {
      // No alt channel → PM decision (not silent done skip)
      agentdesk.kanban.setStatus(card.id, "pending_decision");
      agentdesk.db.execute(
        "UPDATE kanban_cards SET blocked_reason = 'No alt channel for counter-model review — PM decision needed' WHERE id = ?",
        [card.id]
      );
      agentdesk.db.execute(
        "UPDATE kanban_cards SET review_status = NULL WHERE id = ?",
        [card.id]
      );
      agentdesk.log.info("[review] No counter channel for " + card.assigned_agent_id + " → pending_decision");
      return;
    }

    var counterChannelId = agentRow[0].discord_channel_alt;

    // Create review dispatch (targets same agent — counter channel picks it up)
    try {
      var reviewDispatchId = agentdesk.dispatch.create(
        card.id,
        card.assigned_agent_id,
        "review",
        "[Review R" + newRound + "] " + card.id
      );
      agentdesk.log.info("[review] Counter-model review dispatched: " + reviewDispatchId);
      // Discord notification is handled by the Rust handler (async send_dispatch_to_discord)
      // to avoid ureq deadlock on tokio runtime.
    } catch (e) {
      agentdesk.log.warn("[review] Review dispatch failed: " + e);
    }
  },

  // ── Dispatch Completed — review/decision verdict ──────────
  onDispatchCompleted: function(payload) {
    var dispatches = agentdesk.db.query(
      "SELECT id, kanban_card_id, dispatch_type, result FROM task_dispatches WHERE id = ?",
      [payload.dispatch_id]
    );
    if (dispatches.length === 0) return;
    var dispatch = dispatches[0];

    // Only handle review-type dispatches
    if (dispatch.dispatch_type !== "review" && dispatch.dispatch_type !== "review-decision") return;
    if (!dispatch.kanban_card_id) return;

    var result = null;
    try { result = JSON.parse(dispatch.result || "{}"); } catch(e) { result = {}; }
    var verdict = result.verdict || result.decision;

    // When a review dispatch is auto-completed on session idle without an explicit
    // verdict, default to "pass". The reviewer would have sent an "improve" verdict
    // or kept the session active if blocking issues were found.
    if (!verdict && result.auto_completed) {
      verdict = "pass";
      agentdesk.log.info("[review] Auto-completed review dispatch " + dispatch.id + " — defaulting verdict to pass");
    }

    if (!verdict) {
      agentdesk.log.info("[review] No verdict in dispatch " + dispatch.id + " result, waiting for API verdict");
      return;
    }

    processVerdict(dispatch.kanban_card_id, verdict, result);
  },

  // ── Review Verdict — from /api/review-verdict ─────────────
  onReviewVerdict: function(payload) {
    if (!payload.card_id || !payload.verdict) return;
    processVerdict(payload.card_id, payload.verdict, payload);
  }
};

function processVerdict(cardId, verdict, result) {
  if (verdict === "pass" || verdict === "accept" || verdict === "approved") {
    agentdesk.db.execute(
      "UPDATE kanban_cards SET review_status = NULL, updated_at = datetime('now') WHERE id = ?",
      [cardId]
    );

    // Review passed — check pipeline stages, otherwise done
    var stages = agentdesk.db.query(
      "SELECT id, stage_name, agent_override_id FROM pipeline_stages WHERE repo_id = (SELECT repo_id FROM kanban_cards WHERE id = ?) AND trigger_after = 'review_pass' ORDER BY stage_order ASC LIMIT 1",
      [cardId]
    );
    if (stages.length > 0) {
      var stage = stages[0];
      // Assign pipeline stage to card
      agentdesk.db.execute(
        "UPDATE kanban_cards SET pipeline_stage_id = ?, updated_at = datetime('now') WHERE id = ?",
        [stage.id, cardId]
      );
      agentdesk.log.info("[review] Card " + cardId + " passed review, entering pipeline stage: " + stage.stage_name);

      // Create dispatch for the pipeline stage if agent is assigned
      var stageAgent = stage.agent_override_id;
      if (!stageAgent) {
        // Fall back to card's assigned agent
        var cardAgent = agentdesk.db.query("SELECT assigned_agent_id FROM kanban_cards WHERE id = ?", [cardId]);
        stageAgent = (cardAgent.length > 0 && cardAgent[0].assigned_agent_id) ? cardAgent[0].assigned_agent_id : null;
      }
      if (stageAgent) {
        try {
          agentdesk.dispatch.create(
            cardId,
            stageAgent,
            "implementation",
            "[Pipeline: " + stage.stage_name + "] " + cardId
          );
          agentdesk.log.info("[review] Pipeline dispatch created for stage " + stage.stage_name);
        } catch (e) {
          agentdesk.log.warn("[review] Pipeline dispatch failed: " + e);
        }
      } else {
        // No agent — route to PM decision
        agentdesk.kanban.setStatus(cardId, "pending_decision");
        agentdesk.db.execute(
          "UPDATE kanban_cards SET blocked_reason = ? WHERE id = ?",
          ["Pipeline stage '" + stage.stage_name + "' has no assigned agent", cardId]
        );
      }
    } else {
      agentdesk.kanban.setStatus(cardId, "done");
      agentdesk.log.info("[review] Card " + cardId + " passed review → done");
    }

  } else if (verdict === "improve" || verdict === "reject" || verdict === "rework") {
    // Store review notes
    if (result.notes || result.feedback) {
      agentdesk.db.execute(
        "UPDATE kanban_cards SET review_notes = ? WHERE id = ?",
        [result.notes || result.feedback, cardId]
      );
    }

    // Set review_status to suggestion_pending — agent must decide: accept/dispute/dismiss
    agentdesk.db.execute(
      "UPDATE kanban_cards SET review_status = 'suggestion_pending', updated_at = datetime('now') WHERE id = ?",
      [cardId]
    );
    agentdesk.log.info("[review] Card " + cardId + " needs review decision → suggestion_pending");

    // Notification to original agent's primary channel is handled by Rust
    // (dispatched_sessions.rs / dispatches.rs sends async Discord message after OnDispatchCompleted)
    // Rework dispatch is NOT auto-created — agent decides after reading review comments.

  } else {
    agentdesk.log.warn("[review] Unknown verdict '" + verdict + "' for card " + cardId);
  }
}

agentdesk.registerPolicy(reviewAutomation);
