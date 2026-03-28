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
  agentdesk.message.queue(target, content, bot || "announce", "system");
}

function notifyPmdPendingDecision(cardId, reason) {
  var cards = agentdesk.db.query(
    "SELECT title, github_issue_number, github_issue_url, assigned_agent_id FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (cards.length === 0) return;
  var card = cards[0];
  var issueNum = card.github_issue_number || "?";
  var issueUrl = card.github_issue_url || "";
  var msg = "PM 판단 필요 — #" + issueNum + " " + card.title +
    "\n\n사유: " + reason +
    (issueUrl ? "\nGitHub: " + issueUrl : "") +
    "\n\n/api/pm-decision API로 처리해주세요. (resume/rework/dismiss/requeue)";

  // Send to PMD channel — find pmd_channel from agents or use config
  var pmdChannel = agentdesk.config.get("pmd_channel_id");
  if (!pmdChannel) {
    // Fallback: find agent with 'pmd' in id
    var pmdAgents = agentdesk.db.query(
      "SELECT discord_channel_id FROM agents WHERE id LIKE '%pmd%' LIMIT 1"
    );
    if (pmdAgents.length > 0) pmdChannel = pmdAgents[0].discord_channel_id;
  }
  if (pmdChannel) {
    sendDiscordReview("channel:" + pmdChannel, msg, "notify");
  }
}

var reviewAutomation = {
  name: "review-automation",
  priority: 50,

  // ── Review Enter — counter-model review trigger ───────────
  onReviewEnter: function(payload) {
    var cards = agentdesk.db.query(
      "SELECT id, repo_id, assigned_agent_id, review_round, review_status, deferred_dod_json FROM kanban_cards WHERE id = ?",
      [payload.card_id]
    );
    if (cards.length === 0) return;
    var card = cards[0];
    var cfg = agentdesk.pipeline.resolveForCard(card.id);
    var pendingState = agentdesk.pipeline.forceOnlyTargets(agentdesk.pipeline.nextGatedTarget(agentdesk.pipeline.kickoffState(cfg), cfg), cfg)[0];

    // #128: If card entered review with awaiting_dod (DoD incomplete),
    // skip review dispatch — timeouts.js [D] will escalate to pending_decision after 15 min
    if (card.review_status === "awaiting_dod") {
      agentdesk.log.info("[review] Card " + card.id + " is awaiting_dod — skipping review dispatch");
      return;
    }

    // Check if review is enabled — if not, route to PM decision (not silent done)
    var reviewEnabled = agentdesk.config.get("review_enabled");
    if (reviewEnabled === "false" || reviewEnabled === false) {
      agentdesk.kanban.setStatus(card.id, pendingState);
      agentdesk.db.execute(
        "UPDATE kanban_cards SET blocked_reason = 'Review disabled — PM decision needed to proceed' WHERE id = ?",
        [card.id]
      );
      agentdesk.kanban.setReviewStatus(card.id, null, {suggestion_pending_at: null});
      // #117: sync canonical review state
      agentdesk.reviewState.sync(card.id, "idle");
      agentdesk.log.info("[review] Review disabled, card " + card.id + " → " + pendingState);
      notifyPmdPendingDecision(card.id, "리뷰 비활성화 — PM 판단 필요");
      return;
    }

    // Increment review round (AND status != terminal guards against race with concurrent dismiss)
    var terminalState = agentdesk.pipeline.terminalState(cfg);
    var newRound = (card.review_round || 0) + 1;
    agentdesk.db.execute(
      "UPDATE kanban_cards SET review_round = ?, updated_at = datetime('now') WHERE id = ? AND status != ?",
      [newRound, card.id, terminalState]
    );
    agentdesk.kanban.setReviewStatus(card.id, "reviewing", {review_entered_at: "now", exclude_status: terminalState});

    // #117: Update canonical card_review_state
    agentdesk.reviewState.sync(card.id, "reviewing", { review_round: newRound });

    // Check review round limit — exceed → pending_decision with PMD notification
    var maxRounds = agentdesk.config.get("max_review_rounds") || 3;
    if (newRound > maxRounds) {
      agentdesk.kanban.setStatus(card.id, pendingState);
      agentdesk.kanban.setReviewStatus(card.id, "dilemma_pending", {blocked_reason: "Max review rounds (" + maxRounds + ") exceeded — PM decision needed"});
      // #117: sync canonical review state
      agentdesk.reviewState.sync(card.id, "dilemma_pending", { review_round: newRound });
      agentdesk.log.warn("[review] Max review rounds (" + maxRounds + ") reached for " + card.id + " → " + pendingState);
      notifyPmdPendingDecision(card.id, "리뷰 라운드 상한(" + maxRounds + "회) 초과");
      return;
    }

    // Counter-model review: send to alternate channel (Claude↔Codex pair)
    var counterModelEnabled = agentdesk.config.get("counter_model_review_enabled");
    if (counterModelEnabled === false || counterModelEnabled === "false") {
      agentdesk.kanban.setStatus(card.id, pendingState);
      agentdesk.db.execute(
        "UPDATE kanban_cards SET blocked_reason = 'Counter-model review disabled — PM decision needed' WHERE id = ?",
        [card.id]
      );
      agentdesk.kanban.setReviewStatus(card.id, null, {suggestion_pending_at: null});
      // #117: sync canonical review state
      agentdesk.reviewState.sync(card.id, "idle");
      agentdesk.log.info("[review] Counter-model disabled, card " + card.id + " → " + pendingState);
      notifyPmdPendingDecision(card.id, "카운터모델 리뷰 비활성화 — PM 판단 필요");
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
      agentdesk.kanban.setStatus(card.id, pendingState);
      agentdesk.db.execute(
        "UPDATE kanban_cards SET blocked_reason = 'No alt channel for counter-model review — PM decision needed' WHERE id = ?",
        [card.id]
      );
      agentdesk.kanban.setReviewStatus(card.id, null, {suggestion_pending_at: null});
      // #117: sync canonical review state
      agentdesk.reviewState.sync(card.id, "idle");
      agentdesk.log.info("[review] No counter channel for " + card.assigned_agent_id + " → " + pendingState);
      notifyPmdPendingDecision(card.id, "카운터모델 alt 채널 없음 — PM 판단 필요");
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
    var cfg = agentdesk.pipeline.resolveForCard(dispatch.kanban_card_id);

    var result = null;
    try { result = JSON.parse(dispatch.result || "{}"); } catch(e) { result = {}; }
    var verdict = result.verdict || result.decision;

    agentdesk.log.info("[review-debug] onDispatchCompleted: dispatch=" + dispatch.id + " type=" + dispatch.dispatch_type + " verdict=" + verdict + " auto_completed=" + result.auto_completed + " result=" + JSON.stringify(result).substring(0, 200));

    // When a review-decision dispatch is auto-completed, do NOT create another
    // review-decision — that causes an infinite loop.  Only "review" type
    // dispatches should spawn review-decision followups.
    if (!verdict && result.auto_completed && dispatch.dispatch_type === "review-decision") {
      agentdesk.log.info("[review] review-decision auto-completed without verdict — skipping (no infinite loop). dispatch=" + dispatch.id);
      return;
    }

    // When a review dispatch is auto-completed on session idle without an explicit
    // verdict, create a review-decision dispatch to the original agent so they
    // check the review comments and decide the verdict (agent-in-the-loop).
    if (!verdict && result.auto_completed) {
      var cards = agentdesk.db.query(
        "SELECT assigned_agent_id, title, github_issue_number, status FROM kanban_cards WHERE id = ?",
        [dispatch.kanban_card_id]
      );
      // Guard: skip dispatch creation for terminal cards — prevents stale review loops after dismiss
      if (cards.length > 0 && agentdesk.pipeline.isTerminal(cards[0].status, cfg)) {
        agentdesk.log.info("[review] Card " + dispatch.kanban_card_id + " already terminal — skipping review-decision dispatch");
        return;
      }
      if (cards.length > 0 && cards[0].assigned_agent_id) {
        var card = cards[0];
        var issueNum = card.github_issue_number || "?";
        try {
          agentdesk.dispatch.create(
            dispatch.kanban_card_id,
            card.assigned_agent_id,
            "review-decision",
            "[Review Decision] #" + issueNum + " " + card.title
          );
          agentdesk.log.info("[review] Auto-completed review has no verdict — dispatched review-decision to " + card.assigned_agent_id + " for #" + issueNum);
        } catch (e) {
          agentdesk.log.warn("[review] Failed to create review-decision dispatch: " + e);
        }
      }
      return;
    }

    if (!verdict) {
      agentdesk.log.info("[review] No verdict in dispatch " + dispatch.id + " result, waiting for API verdict");
      return;
    }

    agentdesk.log.info("[review-debug] CALLING processVerdict: card=" + dispatch.kanban_card_id + " verdict=" + verdict);
    processVerdict(dispatch.kanban_card_id, verdict, result);
  },

  // ── Review Verdict — from /api/review-verdict ─────────────
  onReviewVerdict: function(payload) {
    if (!payload.card_id || !payload.verdict) return;
    processVerdict(payload.card_id, payload.verdict, payload);
  }
};

// #118: Tokenize text into normalized words for similarity comparison
function tokenize(text) {
  if (!text) return [];
  return text.toLowerCase().replace(/[^a-z0-9가-힣\s]/g, " ").split(/\s+/).filter(function(w) { return w.length > 1; });
}

// #118: Jaccard similarity between two texts (word-level)
function findingsSimilar(textA, textB) {
  var tokensA = tokenize(textA);
  var tokensB = tokenize(textB);
  if (tokensA.length === 0 || tokensB.length === 0) return false;

  var setA = {};
  for (var i = 0; i < tokensA.length; i++) setA[tokensA[i]] = true;
  var setB = {};
  for (var j = 0; j < tokensB.length; j++) setB[tokensB[j]] = true;

  var intersection = 0;
  var unionKeys = {};
  for (var k in setA) { unionKeys[k] = true; if (setB[k]) intersection++; }
  for (var k2 in setB) { unionKeys[k2] = true; }

  var unionSize = 0;
  for (var k3 in unionKeys) unionSize++;

  var similarity = unionSize > 0 ? intersection / unionSize : 0;
  agentdesk.log.info("[review] #118 Finding similarity: " + similarity.toFixed(3) + " (threshold: 0.5)");
  return similarity >= 0.5;
}

// #118: Normal suggestion_pending flow — extracted to avoid duplication
function setNormalSuggestionPending(cardId, verdict) {
  var spCfg = agentdesk.pipeline.resolveForCard(cardId);
  var spTerminal = agentdesk.pipeline.terminalState(spCfg);
  agentdesk.kanban.setReviewStatus(cardId, "suggestion_pending", {suggestion_pending_at: "now", exclude_status: spTerminal});
  agentdesk.log.info("[review] Card " + cardId + " needs review decision → suggestion_pending");

  agentdesk.reviewState.sync(cardId, "suggestion_pending", { last_verdict: verdict });
}

function processVerdict(cardId, verdict, result) {
  // Guard: skip processing for terminal cards — prevents stale dispatches from
  // re-triggering review state changes after dismiss.
  var cfg = agentdesk.pipeline.resolveForCard(cardId);
  var terminalState = agentdesk.pipeline.terminalState(cfg);
  var initialState = agentdesk.pipeline.kickoffState(cfg);
  var inProgressState = agentdesk.pipeline.nextGatedTarget(initialState, cfg);
  var reviewState = agentdesk.pipeline.nextGatedTarget(inProgressState, cfg);
  var reviewPassTarget = agentdesk.pipeline.nextGatedTargetWithGate(reviewState, "review_passed", cfg) || terminalState;
  var reviewReworkTarget = agentdesk.pipeline.nextGatedTargetWithGate(reviewState, "review_rework", cfg) || inProgressState;
  var forceTargets = agentdesk.pipeline.forceOnlyTargets(inProgressState, cfg);
  var pendingState = forceTargets[0];

  var cardCheck = agentdesk.db.query(
    "SELECT status FROM kanban_cards WHERE id = ?", [cardId]
  );
  if (cardCheck.length > 0 && agentdesk.pipeline.isTerminal(cardCheck[0].status, cfg)) {
    agentdesk.log.info("[review] processVerdict skipped — card " + cardId + " already terminal");
    return;
  }

  // #116: accept is NOT a counter-model verdict — it's an agent's review-decision action
  // (rework continuation). Only pass/approved route to done/next-stage.
  if (verdict === "pass" || verdict === "approved") {
    agentdesk.kanban.setReviewStatus(cardId, null, {suggestion_pending_at: null});

    // #117: Update canonical card_review_state — review passed
    agentdesk.reviewState.sync(cardId, "idle", { last_verdict: verdict });

    // Review passed — check for next pipeline stage, otherwise terminal (#110)
    // Look for the next stage AFTER current pipeline_stage_id (stage_order based),
    // OR the first review_pass stage if card has no current pipeline stage.
    var cardInfo = agentdesk.db.query(
      "SELECT pipeline_stage_id, repo_id FROM kanban_cards WHERE id = ?",
      [cardId]
    );
    var nextStage = null;
    if (cardInfo.length > 0 && cardInfo[0].repo_id) {
      var repoId = cardInfo[0].repo_id;
      var currentStageId = cardInfo[0].pipeline_stage_id;

      if (currentStageId) {
        // Has current stage — find next stage by stage_order
        var currentStageInfo = agentdesk.db.query(
          "SELECT stage_order FROM pipeline_stages WHERE id = ?",
          [currentStageId]
        );
        if (currentStageInfo.length > 0) {
          var stages = agentdesk.db.query(
            "SELECT id, stage_name, agent_override_id FROM pipeline_stages WHERE repo_id = ? AND stage_order > ? ORDER BY stage_order ASC LIMIT 1",
            [repoId, currentStageInfo[0].stage_order]
          );
          if (stages.length > 0) nextStage = stages[0];
        }
      } else {
        // No current stage — check for first review_pass triggered stage
        var stages = agentdesk.db.query(
          "SELECT id, stage_name, agent_override_id FROM pipeline_stages WHERE repo_id = ? AND trigger_after = 'review_pass' ORDER BY stage_order ASC LIMIT 1",
          [repoId]
        );
        if (stages.length > 0) nextStage = stages[0];
      }
    }

    if (nextStage) {
      // Assign pipeline stage to card
      agentdesk.db.execute(
        "UPDATE kanban_cards SET pipeline_stage_id = ?, updated_at = datetime('now') WHERE id = ?",
        [nextStage.id, cardId]
      );
      agentdesk.log.info("[review] Card " + cardId + " passed review, entering pipeline stage: " + nextStage.stage_name);

      // Create dispatch for the pipeline stage if agent is assigned
      var stageAgent = nextStage.agent_override_id;
      if (!stageAgent) {
        var cardAgent = agentdesk.db.query("SELECT assigned_agent_id FROM kanban_cards WHERE id = ?", [cardId]);
        stageAgent = (cardAgent.length > 0 && cardAgent[0].assigned_agent_id) ? cardAgent[0].assigned_agent_id : null;
      }
      if (stageAgent) {
        try {
          agentdesk.dispatch.create(
            cardId,
            stageAgent,
            "implementation",
            "[Pipeline: " + nextStage.stage_name + "] " + cardId
          );
          agentdesk.log.info("[review] Pipeline dispatch created for stage " + nextStage.stage_name);
        } catch (e) {
          agentdesk.log.warn("[review] Pipeline dispatch failed: " + e);
        }
      } else {
        agentdesk.kanban.setStatus(cardId, pendingState);
        agentdesk.db.execute(
          "UPDATE kanban_cards SET blocked_reason = ? WHERE id = ?",
          ["Pipeline stage '" + nextStage.stage_name + "' has no assigned agent", cardId]
        );
      }
    } else {
      // No more stages — clear pipeline_stage_id and mark terminal
      if (cardInfo.length > 0 && cardInfo[0].pipeline_stage_id) {
        agentdesk.db.execute(
          "UPDATE kanban_cards SET pipeline_stage_id = NULL, updated_at = datetime('now') WHERE id = ?",
          [cardId]
        );
        agentdesk.log.info("[review] Card " + cardId + " completed all pipeline stages");
      }
      agentdesk.kanban.setStatus(cardId, reviewPassTarget);
      agentdesk.log.info("[review] Card " + cardId + " passed review → " + reviewPassTarget);
    }

  } else if (verdict === "improve" || verdict === "reject" || verdict === "rework") {
    var newNotes = result.notes || result.feedback || "";

    // #118: Detect repeated findings — if same issues recur across rounds,
    // switch approach instead of repeating the same rework.
    var cardInfo118 = agentdesk.db.query(
      "SELECT c.review_notes, c.review_round, c.assigned_agent_id, c.title, c.github_issue_number, " +
      "rs.approach_change_round FROM kanban_cards c " +
      "LEFT JOIN card_review_state rs ON rs.card_id = c.id WHERE c.id = ?",
      [cardId]
    );
    var prevNotes = (cardInfo118.length > 0) ? (cardInfo118[0].review_notes || "") : "";
    var currentRound = (cardInfo118.length > 0) ? (cardInfo118[0].review_round || 0) : 0;
    var approachChangeRound = (cardInfo118.length > 0) ? cardInfo118[0].approach_change_round : null;
    var assignedAgent = (cardInfo118.length > 0) ? cardInfo118[0].assigned_agent_id : null;
    var cardTitle = (cardInfo118.length > 0) ? cardInfo118[0].title : "";
    var issueNum = (cardInfo118.length > 0) ? (cardInfo118[0].github_issue_number || "?") : "?";

    var repeatedFindings = false;
    if (currentRound >= 2 && prevNotes && newNotes) {
      repeatedFindings = findingsSimilar(prevNotes, newNotes);
    }

    // Store review notes (overwrite previous)
    if (newNotes) {
      agentdesk.db.execute(
        "UPDATE kanban_cards SET review_notes = ? WHERE id = ?",
        [newNotes, cardId]
      );
    }

    if (repeatedFindings && assignedAgent) {
      // Already tried approach change → escalate to PM
      if (approachChangeRound) {
        agentdesk.log.warn("[review] #118 Approach change already attempted at R" + approachChangeRound +
          ", findings still repeat at R" + currentRound + " → " + pendingState);
        agentdesk.kanban.setStatus(cardId, pendingState);
        agentdesk.kanban.setReviewStatus(cardId, "dilemma_pending", {blocked_reason: "접근 전환 후에도 동일 finding 반복 (R" + approachChangeRound + "→R" + currentRound + ") — PM 판단 필요"});
        agentdesk.reviewState.sync(cardId, "dilemma_pending", { last_verdict: verdict });
        notifyPmdPendingDecision(cardId, "접근 전환 후에도 동일 finding 반복 — R" + approachChangeRound + "에서 접근 전환했으나 R" + currentRound + "에서 같은 문제 재발");
        return;
      }

      // First repeated finding → trigger approach change dispatch
      agentdesk.log.info("[review] #118 Repeated findings detected at R" + currentRound + " — triggering approach change");

      var approachPrompt = "[Approach Change R" + currentRound + "] #" + issueNum + " " + cardTitle +
        "\n\n이전 접근이 " + currentRound + "회 연속 같은 리뷰 지적을 받았습니다." +
        "\n기존 방식과 다른 접근으로 해결하세요." +
        "\n\n반복된 finding:\n" + newNotes;

      try {
        var dispatchId = agentdesk.dispatch.create(
          cardId,
          assignedAgent,
          "rework",
          approachPrompt
        );
        agentdesk.log.info("[review] #118 Approach-change rework dispatch created: " + dispatchId);

        // Record approach change round
        agentdesk.reviewState.sync(cardId, "rework_pending", { last_verdict: verdict, approach_change_round: currentRound });

        // Transition card to rework target for rework
        agentdesk.kanban.setReviewStatus(cardId, "rework_pending", {exclude_status: terminalState});
        agentdesk.kanban.setStatus(cardId, reviewReworkTarget);
      } catch (e) {
        agentdesk.log.warn("[review] #118 Approach-change dispatch failed: " + e + " — falling back to suggestion_pending");
        // Fall through to normal suggestion_pending below
        setNormalSuggestionPending(cardId, verdict);
      }
      return;
    }

    // Normal path: suggestion_pending — agent must decide: accept/dispute/dismiss
    setNormalSuggestionPending(cardId, verdict);

    // Notification to original agent's primary channel is handled by Rust
    // (dispatched_sessions.rs / dispatches.rs sends async Discord message after OnDispatchCompleted)
    // Rework dispatch is NOT auto-created — agent decides after reading review comments.

  } else {
    agentdesk.log.warn("[review] Unknown verdict '" + verdict + "' for card " + cardId);
  }
}

agentdesk.registerPolicy(reviewAutomation);
