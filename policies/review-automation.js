/**
 * review-automation.js — ADK Policy: Review Lifecycle
 * priority: 50
 *
 * Hooks:
 *   onReviewEnter       — 카운터모델 리뷰 디스패치 생성
 *   onDispatchCompleted — review/decision dispatch 완료 → verdict 처리
 *   onReviewVerdict     — 외부 API verdict 수신 처리
 */

var prTracking = agentdesk.prTracking;

function sendDiscordReview(target, content, bot) {
  agentdesk.message.queue(target, content, bot || "announce", "system");
}

function notifyPmdPendingDecision(cardId, reason) {
  escalate(cardId, reason);
}

var reviewAutomation = {
  name: "review-automation",
  priority: 50,

  // typed-facade-slice:start review-entry
  // ── Review Enter — counter-model review trigger ───────────
  onReviewEnter: function(payload) {
    var card = agentdesk.cards.get(payload.card_id);
    if (!card) return;
    var entry = agentdesk.review.entryContext(card.id);
    if (!entry) return;
    var cfg = agentdesk.pipeline.resolveForCard(card.id);
    var terminalState = agentdesk.pipeline.terminalState(cfg);
    var pendingState = agentdesk.pipeline.forceOnlyTargets(agentdesk.pipeline.nextGatedTarget(agentdesk.pipeline.kickoffState(cfg), cfg), cfg)[0];

    // #128: If card entered review with awaiting_dod (DoD incomplete),
    // skip review dispatch — timeouts.js [D] will escalate to pending_decision after 15 min
    if (card.review_status === "awaiting_dod") {
      agentdesk.log.info("[review] Card " + card.id + " is awaiting_dod — skipping review dispatch");
      return;
    }

    // Check if review is enabled — if not, complete immediately
    var reviewEnabled = agentdesk.config.get("review_enabled");
    if (reviewEnabled === "false" || reviewEnabled === false) {
      agentdesk.kanban.setStatus(card.id, terminalState, true);
      agentdesk.kanban.setReviewStatus(card.id, null, {blocked_reason: null});
      agentdesk.log.info("[review] Review disabled, card " + card.id + " → " + terminalState);
      return;
    }

    if (!card.assigned_agent_id) return;

    // Single-provider agents auto-approve after entering review because
    // there is no alternate provider/channel to dispatch a review to.
    var counterChannelId = agentdesk.agents.resolveCounterModelChannel(card.assigned_agent_id);
    if (!counterChannelId) {
      agentdesk.kanban.setStatus(card.id, terminalState, true);
      agentdesk.kanban.setReviewStatus(card.id, null, {blocked_reason: null});
      agentdesk.log.info("[review] No alternate review channel for " + card.id + " → " + terminalState);
      return;
    }

    // #335: Only advance review_round when new implementation/rework actually
    // finished since the previous round. Reopen loops without fresh work should
    // reuse the existing round instead of consuming it.
    var currentRound = Number(entry.current_round || 0);
    var completedWorkCount = Number(entry.completed_work_count || 0);
    var shouldAdvanceRound = !!entry.should_advance_round;
    var newRound = Number(entry.next_round || currentRound);
    agentdesk.review.recordEntry(
      card.id,
      shouldAdvanceRound
        ? {review_round: newRound, exclude_status: terminalState}
        : {exclude_status: terminalState}
    );
    if (!shouldAdvanceRound) {
      agentdesk.log.info(
        "[review] Reusing review round R" + currentRound + " for " + card.id +
        " (completed work dispatches=" + completedWorkCount + ")"
      );
    }
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
    // Create review dispatch (targets same agent — counter channel picks it up)
    // #245: Log agent_id for diagnostics — "project-agentdesk-cdx" phantom agent was traced here
    agentdesk.log.info("[review] Creating review dispatch: card=" + card.id + " agent=" + card.assigned_agent_id + " round=" + newRound);
    try {
      var reviewDispatchId = agentdesk.dispatch.create(
        card.id,
        card.assigned_agent_id,
        "review",
        "[Review R" + newRound + "] " + card.id
      );
      agentdesk.log.info("[review] Counter-model review dispatched: " + reviewDispatchId + " to " + card.assigned_agent_id);
      // Discord notification is handled by the Rust handler (async send_dispatch_to_discord)
      // to avoid ureq deadlock on tokio runtime.
    } catch (e) {
      agentdesk.log.warn("[review] Review dispatch failed: " + e);
    }
  },
  // typed-facade-slice:end review-entry

  // ── Dispatch Completed — review/decision verdict ──────────
  onDispatchCompleted: function(payload) {
    var dispatches = agentdesk.db.query(
      "SELECT id, kanban_card_id, dispatch_type, result FROM task_dispatches WHERE id = ?",
      [payload.dispatch_id]
    );
    if (dispatches.length === 0) return;
    var dispatch = dispatches[0];

    // #198/#211: create-pr dispatch completed — canonicalize PR tracking, then wait for CI
    if (dispatch.dispatch_type === "create-pr") {
      var cardMeta = agentdesk.db.query(
        "SELECT repo_id, github_issue_url FROM kanban_cards WHERE id = ?",
        [dispatch.kanban_card_id]
      );
      var tracking = loadPrTracking(dispatch.kanban_card_id);
      var latestWork = loadLatestCompletedWorkTarget(dispatch.kanban_card_id);
      var repoId = (tracking && tracking.repo_id)
        || (cardMeta.length > 0 ? (cardMeta[0].repo_id || extractRepoFromIssueUrl(cardMeta[0].github_issue_url)) : null);
      var worktreePath = (tracking && tracking.worktree_path)
        || (latestWork && latestWork.worktree_path);
      var branch = (tracking && tracking.branch)
        || (latestWork && latestWork.branch);
      var trackedSha = (tracking && tracking.head_sha)
        || (latestWork && latestWork.head_sha);

      if (!repoId || !branch) {
        upsertPrTracking(
          dispatch.kanban_card_id,
          repoId,
          worktreePath,
          branch,
          tracking ? tracking.pr_number : null,
          trackedSha,
          "create-pr",
          "create-pr completed without canonical repo/branch"
        );
        agentdesk.db.execute(
          "UPDATE kanban_cards SET blocked_reason = 'pr:create_failed' WHERE id = ?",
          [dispatch.kanban_card_id]
        );
        agentdesk.log.warn("[review] Create-PR completed but canonical tracking is incomplete for card " + dispatch.kanban_card_id);
        return;
      }

      var pr = findOpenPrByTrackedBranch(repoId, branch);
      if (!pr) {
        upsertPrTracking(
          dispatch.kanban_card_id,
          repoId,
          worktreePath,
          branch,
          tracking ? tracking.pr_number : null,
          trackedSha,
          "create-pr",
          "create-pr completed but no open PR found for branch " + branch
        );
        agentdesk.db.execute(
          "UPDATE kanban_cards SET blocked_reason = 'pr:create_failed' WHERE id = ?",
          [dispatch.kanban_card_id]
        );
        agentdesk.log.warn("[review] Create-PR completed but no open PR was found for card " + dispatch.kanban_card_id + " branch " + branch);
        return;
      }

      upsertPrTracking(
        dispatch.kanban_card_id,
        repoId,
        worktreePath,
        pr.branch || branch,
        pr.number,
        pr.sha || trackedSha,
        "wait-ci",
        null
      );
      agentdesk.db.execute(
        "UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = ?",
        [dispatch.kanban_card_id]
      );
      agentdesk.log.info("[review] Create-PR completed for card " + dispatch.kanban_card_id + " → wait-ci on PR #" + pr.number);
      return;
    }

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

    // Legacy fallback: if a review dispatch somehow arrives completed without an
    // explicit verdict, create a review-decision dispatch so the original agent
    // can inspect the review comments and decide the outcome.
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

// #197: Check if PR has .rs file changes (skip condition for dev-deploy/e2e-test)
function hasRsChanges(cardId) {
  var card = agentdesk.db.query(
    "SELECT github_issue_number, repo_id FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (card.length === 0) return true;
  if (!card[0].github_issue_number || !card[0].repo_id) return true;

  try {
    var prOutput = agentdesk.exec("gh", [
      "pr", "list", "--repo", card[0].repo_id,
      "--state", "all", "--limit", "3",
      "--search", "#" + card[0].github_issue_number,
      "--json", "number"
    ]);
    var prs = JSON.parse(prOutput || "[]");
    if (prs.length === 0) return true;

    var filesOutput = agentdesk.exec("gh", [
      "api", "repos/" + card[0].repo_id + "/pulls/" + prs[0].number + "/files",
      "--jq", ".[].filename"
    ]);
    if (!filesOutput || filesOutput.indexOf("ERROR") === 0) return true;

    var files = filesOutput.split("\n");
    for (var i = 0; i < files.length; i++) {
      if (files[i].trim().endsWith(".rs")) return true;
    }
    return false;
  } catch (e) {
    agentdesk.log.warn("[review] #197 Failed to check .rs changes: " + e);
    return true;
  }
}

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

function summarizeFindingForPrompt(text) {
  var clean = (text || "").replace(/\s+/g, " ").trim();
  if (!clean) return "(없음)";
  if (clean.length > 280) return clean.slice(0, 277) + "...";
  return clean;
}

function buildSessionResetPrompt(issueNum, cardTitle, approachChangeRound, currentRound, prevNotes, newNotes) {
  return "[Session Reset R" + currentRound + "] #" + issueNum + " " + cardTitle +
    "\n\n접근 전환 후에도 동일 finding이 반복되었습니다. 이번 rework는 반드시 새 세션에서 시작합니다." +
    "\n이전에 기존 수정 접근과 R" + approachChangeRound + "의 접근 전환을 시도했지만 같은 문제를 해결하지 못했습니다." +
    "\n\n이전 실패 이력 요약:" +
    "\n- 기존 접근: 기존 구현/수정 흐름으로 대응했으나 동일 finding 반복" +
    "\n- 접근 전환 시도: R" + approachChangeRound + "에서 다른 접근을 요청했지만 R" + currentRound + "에서 같은 finding 재발" +
    "\n- 직전 리뷰 피드백: " + summarizeFindingForPrompt(prevNotes) +
    "\n- 현재 리뷰 피드백: " + summarizeFindingForPrompt(newNotes) +
    "\n\n세션이 새로 시작되므로 기존 해법을 답습하지 말고, 필요한 맥락을 다시 재구성한 뒤 완전히 다른 방향으로 접근하세요." +
    "\n반복된 finding:\n" + newNotes;
}

// #118: Normal suggestion_pending flow — extracted to avoid duplication
function setNormalSuggestionPending(cardId, verdict) {
  var spCfg = agentdesk.pipeline.resolveForCard(cardId);
  var spTerminal = agentdesk.pipeline.terminalState(spCfg);
  agentdesk.kanban.setReviewStatus(cardId, "suggestion_pending", {suggestion_pending_at: "now", exclude_status: spTerminal});
  agentdesk.log.info("[review] Card " + cardId + " needs review decision → suggestion_pending");

  agentdesk.reviewState.sync(cardId, "suggestion_pending", { last_verdict: verdict });
}

function parseJsonObject(raw) {
  if (!raw) return {};
  try {
    return JSON.parse(raw) || {};
  } catch (e) {
    return {};
  }
}

function firstPresent() {
  for (var i = 0; i < arguments.length; i++) {
    var value = arguments[i];
    if (value === null || value === undefined) continue;
    if (typeof value === "string" && value.trim() === "") continue;
    return value;
  }
  return null;
}

function extractRepoFromIssueUrl(url) {
  return prTracking.extractRepoFromIssueUrl(url);
}

function loadLatestCompletedWorkTarget(cardId) {
  var rows = agentdesk.db.query(
    "SELECT result, context FROM task_dispatches " +
    "WHERE kanban_card_id = ? " +
    "AND dispatch_type IN ('implementation', 'rework') " +
    "AND status = 'completed' " +
    "ORDER BY COALESCE(completed_at, updated_at) DESC, rowid DESC LIMIT 1",
    [cardId]
  );
  if (rows.length === 0) return null;

  var result = parseJsonObject(rows[0].result);
  var context = parseJsonObject(rows[0].context);
  var worktreePath = firstPresent(
    result.completed_worktree_path,
    result.worktree_path,
    context.completed_worktree_path,
    context.worktree_path
  );
  var branch = firstPresent(
    result.completed_branch,
    result.worktree_branch,
    result.branch,
    context.completed_branch,
    context.worktree_branch,
    context.branch
  );
  var headSha = firstPresent(
    result.completed_commit,
    result.reviewed_commit,
    context.completed_commit,
    context.reviewed_commit
  );

  if (!branch && worktreePath) {
    var branchResult = agentdesk.exec("git", ["-C", worktreePath, "branch", "--show-current"]);
    if (branchResult && branchResult.indexOf("ERROR") !== 0 && branchResult.trim()) {
      branch = branchResult.trim();
    }
  }
  if (!headSha && worktreePath) {
    var headResult = agentdesk.exec("git", ["-C", worktreePath, "rev-parse", "HEAD"]);
    if (headResult && headResult.indexOf("ERROR") !== 0 && headResult.trim()) {
      headSha = headResult.trim();
    }
  }

  if (!worktreePath && !branch && !headSha) return null;
  return {
    worktree_path: worktreePath,
    branch: branch,
    head_sha: headSha
  };
}

function loadPrTracking(cardId) {
  return prTracking.load(cardId);
}

function upsertPrTracking(cardId, repoId, worktreePath, branch, prNumber, headSha, state, lastError) {
  return prTracking.upsert(cardId, repoId, worktreePath, branch, prNumber, headSha, state, lastError);
}

function findOpenPrByTrackedBranch(repoId, branch) {
  return prTracking.findOpenPrByBranch(repoId, branch);
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
            "SELECT id, stage_name, agent_override_id, provider, skip_condition FROM pipeline_stages WHERE repo_id = ? AND stage_order > ? ORDER BY stage_order ASC LIMIT 1",
            [repoId, currentStageInfo[0].stage_order]
          );
          if (stages.length > 0) nextStage = stages[0];
        }
      } else {
        // No current stage — check for first review_pass triggered stage
        var stages = agentdesk.db.query(
          "SELECT id, stage_name, agent_override_id, provider, skip_condition FROM pipeline_stages WHERE repo_id = ? AND trigger_after = 'review_pass' ORDER BY stage_order ASC LIMIT 1",
          [repoId]
        );
        if (stages.length > 0) nextStage = stages[0];
      }
    }

    if (nextStage) {
      // #197: Check skip condition — no .rs changes → skip all pipeline stages
      if (nextStage.skip_condition === "no_rs_changes" && !hasRsChanges(cardId)) {
        if (cardInfo.length > 0 && cardInfo[0].pipeline_stage_id) {
          agentdesk.db.execute(
            "UPDATE kanban_cards SET pipeline_stage_id = NULL, updated_at = datetime('now') WHERE id = ?",
            [cardId]
          );
        }
        agentdesk.kanban.setStatus(cardId, reviewPassTarget, true);
        agentdesk.log.info("[review] Card " + cardId + " skipping pipeline stages (no .rs changes) → " + reviewPassTarget);
      } else {
        // Assign pipeline stage to card
        agentdesk.db.execute(
          "UPDATE kanban_cards SET pipeline_stage_id = ?, updated_at = datetime('now') WHERE id = ?",
          [nextStage.id, cardId]
        );
        agentdesk.log.info("[review] Card " + cardId + " passed review, entering pipeline stage: " + nextStage.stage_name);

        // #197: Self-hosted stage (dev-deploy) — queue for internal execution
        if (nextStage.provider === "self") {
          agentdesk.db.execute(
            "UPDATE kanban_cards SET blocked_reason = 'deploy:waiting' WHERE id = ?",
            [cardId]
          );
          agentdesk.kanban.setStatus(cardId, inProgressState);
          agentdesk.log.info("[review] Card " + cardId + " queued for self-hosted stage: " + nextStage.stage_name);
        }
        // #197: Counter-model stage (e2e-test) — dispatch only if DoD contains e2e item
        else if (nextStage.provider === "counter") {
          // Skip e2e if DoD doesn't mention it — review pass goes straight to done
          var dodCheck = agentdesk.db.query(
            "SELECT description FROM kanban_cards WHERE id = ?", [cardId]
          );
          var dodText = (dodCheck.length > 0 && dodCheck[0].description) ? dodCheck[0].description.toLowerCase() : "";
          if (dodText.indexOf("e2e") === -1 && dodText.indexOf("end-to-end") === -1 && dodText.indexOf("end to end") === -1) {
            agentdesk.log.info("[review] Skipping e2e-test for card " + cardId + " — DoD has no e2e item");
            // Skip remaining pipeline stages and go to done
            agentdesk.db.execute(
              "UPDATE kanban_cards SET pipeline_stage_id = NULL, blocked_reason = NULL, updated_at = datetime('now') WHERE id = ?",
              [cardId]
            );
            var skipCfg = agentdesk.pipeline.resolveForCard(cardId);
            var skipTerminal = agentdesk.pipeline.terminalState(skipCfg);
            agentdesk.kanban.setStatus(cardId, skipTerminal, true);
            return;
          }
          var counterCardInfo = agentdesk.db.query(
            "SELECT assigned_agent_id, title, github_issue_number FROM kanban_cards WHERE id = ?", [cardId]
          );
          if (counterCardInfo.length > 0 && counterCardInfo[0].assigned_agent_id) {
            var issueNum = counterCardInfo[0].github_issue_number || "?";
            try {
              agentdesk.dispatch.create(
                cardId, counterCardInfo[0].assigned_agent_id, "e2e-test",
                "[E2E Test] #" + issueNum + " " + counterCardInfo[0].title
              );
              agentdesk.log.info("[review] E2E test dispatch created for stage " + nextStage.stage_name);
            } catch (e) {
              agentdesk.log.warn("[review] E2E test dispatch failed: " + e);
            }
          }
        }
        // Normal agent dispatch
        else {
          var stageAgent = nextStage.agent_override_id;
          if (!stageAgent) {
            var cardAgent = agentdesk.db.query("SELECT assigned_agent_id FROM kanban_cards WHERE id = ?", [cardId]);
            stageAgent = (cardAgent.length > 0 && cardAgent[0].assigned_agent_id) ? cardAgent[0].assigned_agent_id : null;
          }
          if (stageAgent) {
            try {
              agentdesk.dispatch.create(
                cardId, stageAgent, "implementation",
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
        }
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

      // #198/#211: If the card completed work in a canonical worktree, create
      // a create-pr dispatch and seed pr_tracking before going terminal.
      var prDispatched = false;
      var latestWorkTarget = loadLatestCompletedWorkTarget(cardId);
      var prCardInfo = agentdesk.db.query(
        "SELECT assigned_agent_id, title, github_issue_number, repo_id, github_issue_url FROM kanban_cards WHERE id = ?",
        [cardId]
      );
      if (prCardInfo.length > 0 && prCardInfo[0].assigned_agent_id && latestWorkTarget) {
        var agentId = prCardInfo[0].assigned_agent_id;
        var repoId = prCardInfo[0].repo_id || extractRepoFromIssueUrl(prCardInfo[0].github_issue_url);
        if (repoId && latestWorkTarget.branch) {
          upsertPrTracking(
            cardId,
            repoId,
            latestWorkTarget.worktree_path,
            latestWorkTarget.branch,
            null,
            latestWorkTarget.head_sha,
            "create-pr",
            null
          );

          var issueNum = prCardInfo[0].github_issue_number || "?";
          try {
            agentdesk.dispatch.create(
              cardId,
              agentId,
              "create-pr",
              "[PR 생성] #" + issueNum + " " + prCardInfo[0].title,
              {
                worktree_path: latestWorkTarget.worktree_path,
                worktree_branch: latestWorkTarget.branch,
                branch: latestWorkTarget.branch
              }
            );
            prDispatched = true;
            agentdesk.log.info("[review] Create-PR dispatch created for tracked worktree card " + cardId);
          } catch (e) {
            upsertPrTracking(
              cardId,
              repoId,
              latestWorkTarget.worktree_path,
              latestWorkTarget.branch,
              null,
              latestWorkTarget.head_sha,
              "create-pr",
              String(e)
            );
            agentdesk.log.warn("[review] Create-PR dispatch failed: " + e + " — falling through to terminal");
          }
        }
      }

      if (!prDispatched) {
        agentdesk.kanban.setStatus(cardId, reviewPassTarget, true);
        agentdesk.log.info("[review] Card " + cardId + " passed review → " + reviewPassTarget);
      }
    }

  } else if (verdict === "improve" || verdict === "reject" || verdict === "rework") {
    var newNotes = result.notes || result.feedback || "";

    // #118: Detect repeated findings — if same issues recur across rounds,
    // switch approach instead of repeating the same rework.
    var cardInfo118 = agentdesk.db.query(
      "SELECT c.review_notes, c.review_round, c.assigned_agent_id, c.title, c.github_issue_number, " +
      "rs.approach_change_round, rs.session_reset_round FROM kanban_cards c " +
      "LEFT JOIN card_review_state rs ON rs.card_id = c.id WHERE c.id = ?",
      [cardId]
    );
    var prevNotes = (cardInfo118.length > 0) ? (cardInfo118[0].review_notes || "") : "";
    var currentRound = (cardInfo118.length > 0) ? (cardInfo118[0].review_round || 0) : 0;
    var approachChangeRound = (cardInfo118.length > 0) ? cardInfo118[0].approach_change_round : null;
    var sessionResetRound = (cardInfo118.length > 0) ? cardInfo118[0].session_reset_round : null;
    var assignedAgent = (cardInfo118.length > 0) ? cardInfo118[0].assigned_agent_id : null;
    var cardTitle = (cardInfo118.length > 0) ? cardInfo118[0].title : "";
    var issueNum = (cardInfo118.length > 0) ? (cardInfo118[0].github_issue_number || "?") : "?";

    var repeatedFindings = false;
    if (currentRound >= 2 && prevNotes && newNotes) {
      repeatedFindings = findingsSimilar(prevNotes, newNotes);
    }

    // Guard: empty notes on non-pass verdict — similarity check is blind.
    // If consecutive rounds have no notes, escalate — the review loop cannot
    // self-correct without feedback data.
    // #118: Check actual prior-round notes from review_tuning_outcomes to detect
    // truly consecutive empty notes (review_notes field persists old values).
    if (!newNotes && currentRound >= 2 && (verdict === "improve" || verdict === "reject")) {
      var priorRoundNotes = agentdesk.db.query(
        "SELECT notes FROM kanban_reviews WHERE card_id = ? AND round = ? ORDER BY created_at DESC LIMIT 1",
        [cardId, currentRound - 1]
      );
      var priorEmpty = priorRoundNotes.length === 0 || !priorRoundNotes[0].notes;
      if (priorEmpty) {
        agentdesk.log.warn("[review] #118 Empty notes for 2+ consecutive improve/reject rounds on " + cardId + " — escalating to PM");
        agentdesk.kanban.setStatus(cardId, pendingState);
        agentdesk.kanban.setReviewStatus(cardId, "dilemma_pending", {
          blocked_reason: "리뷰 피드백 없이 2회 이상 연속 " + verdict + " — 유사성 검사 불가, PM 판단 필요"
        });
        agentdesk.reviewState.sync(cardId, "dilemma_pending", { last_verdict: verdict });
        notifyPmdPendingDecision(cardId,
          "리뷰 피드백(notes) 없이 2회 이상 연속 " + verdict + " — " +
          "카운터모델이 notes 없이 verdict를 제출하여 반복 finding 검출 불가");
        return;
      }
      agentdesk.log.warn("[review] #118 Empty notes on " + verdict +
        " verdict at R" + currentRound + " for " + cardId +
        " — similarity check skipped (no new findings to compare)");
    }

    // Store review notes (overwrite previous)
    if (newNotes) {
      agentdesk.db.execute(
        "UPDATE kanban_cards SET review_notes = ? WHERE id = ?",
        [newNotes, cardId]
      );
    }

    if (repeatedFindings && assignedAgent) {
      // Already tried session reset after approach change → escalate to PM
      if (sessionResetRound) {
        agentdesk.log.warn("[review] #420 Session reset already attempted at R" + sessionResetRound +
          ", findings still repeat at R" + currentRound + " → " + pendingState);
        agentdesk.kanban.setStatus(cardId, pendingState);
        agentdesk.kanban.setReviewStatus(cardId, "dilemma_pending", {blocked_reason: "세션 리셋 후에도 동일 finding 반복 (R" + sessionResetRound + "→R" + currentRound + ") — PM 판단 필요"});
        agentdesk.reviewState.sync(cardId, "dilemma_pending", { last_verdict: verdict });
        notifyPmdPendingDecision(cardId, "세션 리셋 후에도 동일 finding 반복 — R" + sessionResetRound + "에서 세션 리셋을 시도했으나 R" + currentRound + "에서 같은 문제 재발");
        return;
      }

      // Repeated again after approach change → force a fresh provider session
      if (approachChangeRound) {
        agentdesk.log.info("[review] #420 Approach change at R" + approachChangeRound +
          " still repeated at R" + currentRound + " — triggering session reset rework");

        var resetPrompt = buildSessionResetPrompt(
          issueNum,
          cardTitle,
          approachChangeRound,
          currentRound,
          prevNotes,
          newNotes
        );

        try {
          var resetDispatchId = agentdesk.dispatch.create(
            cardId,
            assignedAgent,
            "rework",
            resetPrompt,
            { force_new_session: true }
          );
          agentdesk.log.info("[review] #420 Session-reset rework dispatch created: " + resetDispatchId);

          agentdesk.reviewState.sync(cardId, "rework_pending", {
            last_verdict: verdict,
            session_reset_round: currentRound
          });

          agentdesk.kanban.setReviewStatus(cardId, "rework_pending", {exclude_status: terminalState});
          agentdesk.kanban.setStatus(cardId, reviewReworkTarget);
        } catch (e) {
          agentdesk.log.warn("[review] #420 Session-reset dispatch failed: " + e + " — falling back to suggestion_pending");
          setNormalSuggestionPending(cardId, verdict);
        }
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
