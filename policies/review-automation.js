/* giant-file-exemption: reason=review-lifecycle-pending-split ticket=#1078 */
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
var REVIEW_LOOP_CHURN_WINDOW_MS = 1800000;
var REVIEW_LOOP_CHURN_THRESHOLD = 3;

function sendDiscordReview(target, content, bot) {
  agentdesk.message.queue(target, content, bot || "announce", "system");
}

function notifyPmdPendingDecision(cardId, reason) {
  escalate(cardId, reason);
}

function emitQualityEvent(event) {
  if (agentdesk.quality && typeof agentdesk.quality.emit === "function") {
    agentdesk.quality.emit(event);
  }
}

function emitReviewVerdictQualityEvent(cardId, verdict, result, options, eventType) {
  if (!(agentdesk.quality && typeof agentdesk.quality.emit === "function")) return;
  var opts = options || {};
  var rows = agentdesk.db.query(
    "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  var agentId = rows.length > 0 ? (rows[0].assigned_agent_id || null) : null;
  var dispatchId = opts.review_dispatch_id || null;
  emitQualityEvent({
    event_type: eventType,
    source_event_id: dispatchId || cardId,
    correlation_id: dispatchId || cardId,
    agent_id: agentId,
    card_id: cardId,
    dispatch_id: dispatchId,
    payload: {
      verdict: verdict,
      notes_present: !!(result && (result.notes || result.feedback)),
      source: dispatchId ? "review_dispatch" : "review_verdict"
    }
  });
}

function reviewLoopFingerprintInfo(cardId) {
  // #751: Prefer the latest completed work dispatch's head_sha. It is
  // updated immediately on every implementation/rework completion. The
  // pr_tracking.head_sha value is only refreshed by the CI recovery
  // polling path (ci-recovery) and can lag multiple
  // rework cycles behind during fast review/rework loops, causing the
  // fingerprint to stay stale and trip the loop guard on genuinely
  // different heads.
  var latestWorkTarget = loadLatestCompletedWorkTarget(cardId);
  var headSha = latestWorkTarget && latestWorkTarget.head_sha
    ? String(latestWorkTarget.head_sha)
    : null;
  if (!headSha) {
    var tracking = loadPrTracking(cardId);
    if (tracking && tracking.head_sha) {
      headSha = String(tracking.head_sha);
    }
  }
  // #2051 Finding 10 (P2): When head_sha cannot be resolved (new card before
  // first commit, transient `git rev-parse` failure, etc.), the legacy code
  // baked the literal "unknown-head" into the fingerprint. Three rapid
  // review re-entries with no resolvable head then tripped
  // REVIEW_LOOP_CHURN_THRESHOLD even though each round was on a genuinely
  // different commit. Now: when the SHA is unknown we use a non-collapsing
  // fingerprint so each entry counts as its own — the guard only fires when
  // we can _confirm_ the same head is being reviewed repeatedly.
  if (!headSha) {
    return {
      head_sha: "unknown-head",
      // Per-entry timestamp guarantees the fingerprint mismatches the prior
      // record, resetting `enter_count` instead of falsely accumulating.
      fingerprint: String(cardId) + "::review::unknown-head::" + loopGuardNowIso()
    };
  }
  return {
    head_sha: headSha,
    fingerprint: String(cardId) + "::review::" + headSha
  };
}

function recordReviewLoopEntry(cardId, newRound) {
  var info = reviewLoopFingerprintInfo(cardId);
  var prior = loadLoopGuardRecord(cardId, "review_churn");
  var nowMs = loopGuardNowMs();
  var nowIso = loopGuardNowIso();
  var priorFirstSeenMs = Number(prior.first_seen_ms || 0);
  var withinWindow =
    prior &&
    prior.fingerprint === info.fingerprint &&
    priorFirstSeenMs > 0 &&
    (nowMs - priorFirstSeenMs) <= REVIEW_LOOP_CHURN_WINDOW_MS;
  var enterCount = withinWindow ? (Number(prior.enter_count || 0) + 1) : 1;
  return replaceLoopGuardRecord(cardId, "review_churn", {
    status: enterCount >= REVIEW_LOOP_CHURN_THRESHOLD ? "threshold_reached" : "tracking",
    fingerprint: info.fingerprint,
    head_sha: info.head_sha,
    enter_count: enterCount,
    threshold: REVIEW_LOOP_CHURN_THRESHOLD,
    window_ms: REVIEW_LOOP_CHURN_WINDOW_MS,
    review_round: newRound,
    first_seen_ms: withinWindow ? priorFirstSeenMs : nowMs,
    last_seen_ms: nowMs,
    first_seen_at: withinWindow ? (prior.first_seen_at || nowIso) : nowIso,
    last_seen_at: nowIso,
    escalation_reason: withinWindow ? (prior.escalation_reason || null) : null,
    escalated_at: withinWindow ? (prior.escalated_at || null) : null
  }, LOOP_GUARD_TTL_SEC);
}

var reviewAutomation = {
  name: "review-automation",
  priority: 50,

  // #5716: bounded sweep for create-pr rows stranded by the removed retry loop.
  onTick5min: function() {
    try { sweepStrandedPrCreateFailures(); } catch (e) { agentdesk.log.warn("[review] sweep failed: " + e); }
  },

  // typed-facade-slice:start review-entry
  // ── Review Enter — counter-model review trigger ───────────
  onReviewEnter: function(payload) {
    var card = agentdesk.cards.get(payload.card_id);
    if (!card) return;
    var cfg = agentdesk.pipeline.resolveForCard(card.id);
    var terminalState = agentdesk.pipeline.terminalState(cfg);
    if (agentdesk.pipeline.isTerminal(card.status, cfg)) {
      agentdesk.reviewState.sync(card.id, "idle");
      agentdesk.kanban.setReviewStatus(card.id, null, { blocked_reason: null });
      agentdesk.log.info("[review] Card " + card.id + " already terminal — skipping OnReviewEnter");
      return;
    }
    var entry = agentdesk.review.entryContext(card.id);
    if (!entry) return;

    // #128: If card entered review with awaiting_dod (DoD incomplete),
    // skip review dispatch — timeouts.js [D] will escalate to dilemma_pending after 15 min
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

    // #2051 Finding 21 (P2): max-round guard MUST run BEFORE
    // `agentdesk.review.recordEntry`. The bridge commits the new
    // review_round to the DB immediately, so escalating afterwards left the
    // card permanently above the cap — every subsequent reopen would re-enter,
    // bump the round again, and re-escalate without ever doing real work.
    // Checking here keeps review_round at its previous value when the cap is
    // hit, so an operator-driven reopen can resume the round normally.
    var maxRoundsGuard = Number(agentdesk.config.get("max_review_rounds") || 3);
    if (shouldAdvanceRound && newRound > maxRoundsGuard) {
      escalateToManualIntervention(
        card.id,
        "Max review rounds (" + maxRoundsGuard + ") exceeded — PM decision needed",
        {
          review: true,
          reviewStateSync: { review_round: currentRound },
          skipEscalate: true
        }
      );
      agentdesk.log.warn(
        "[review] Max review rounds (" + maxRoundsGuard + ") reached for " + card.id +
        " before recordEntry — review_round retained at R" + currentRound + " for reopen recovery"
      );
      notifyDeadlockManager(
        "⚠️ [Review Deadlock] " +
          (card.github_issue_number ? ("#" + card.github_issue_number + " ") : "") +
          card.id + "\n" +
          "card_id: " + card.id + "\n" +
          "agent: " + card.assigned_agent_id + "\n" +
          "review round " + newRound + " exceeded max " + maxRoundsGuard,
        "review-automation"
      );
      return;
    }

    agentdesk.review.recordEntry(
      card.id,
      shouldAdvanceRound
        ? {review_round: newRound, exclude_status: terminalState}
        : {exclude_status: terminalState}
    );

    var reviewLoopState = recordReviewLoopEntry(card.id, newRound);
    if (Number(reviewLoopState.enter_count || 0) >= REVIEW_LOOP_CHURN_THRESHOLD) {
      var shortSha = String(reviewLoopState.head_sha || "unknown").substring(0, 12);
      var churnReason =
        "Review loop guard: same head re-entered review " +
        reviewLoopState.enter_count + " times within " +
        Math.round(REVIEW_LOOP_CHURN_WINDOW_MS / 60000) + "m (head " + shortSha + ")";
      replaceLoopGuardRecord(card.id, "review_churn", {
        status: "escalated",
        fingerprint: reviewLoopState.fingerprint,
        head_sha: reviewLoopState.head_sha,
        enter_count: reviewLoopState.enter_count,
        threshold: REVIEW_LOOP_CHURN_THRESHOLD,
        window_ms: REVIEW_LOOP_CHURN_WINDOW_MS,
        review_round: newRound,
        first_seen_ms: reviewLoopState.first_seen_ms,
        last_seen_ms: reviewLoopState.last_seen_ms,
        first_seen_at: reviewLoopState.first_seen_at,
        last_seen_at: reviewLoopState.last_seen_at,
        escalation_reason: churnReason,
        escalated_at: loopGuardNowIso()
      }, LOOP_GUARD_TTL_SEC);
      escalateToManualIntervention(card.id, churnReason, {
        review: true,
        reviewStateSync: { review_round: newRound },
        skipEscalate: true
      });
      agentdesk.log.warn(
        "[review] Loop guard escalated " + card.id +
        " after repeated same-head review re-entry (" + reviewLoopState.enter_count + ")"
      );
      notifyDeadlockManager(
        "⚠️ [Review Loop Guard] " +
          (card.github_issue_number ? ("#" + card.github_issue_number + " ") : "") +
          card.id + "\n" +
          "card_id: " + card.id + "\n" +
          "agent: " + card.assigned_agent_id + "\n" +
          "head_sha: " + shortSha + "\n" +
          "review re-entry count: " + reviewLoopState.enter_count,
        "review-automation"
      );
      return;
    }

    if (!shouldAdvanceRound) {
      agentdesk.log.info(
        "[review] Reusing review round R" + currentRound + " for " + card.id +
        " (completed work dispatches=" + completedWorkCount + ")"
      );
    }
    agentdesk.kanban.setReviewStatus(card.id, "reviewing", {
      review_entered_at: "now",
      blocked_reason: null,
      exclude_status: terminalState
    });

    // #117: Update canonical card_review_state
    agentdesk.reviewState.sync(card.id, "reviewing", { review_round: newRound });

    // Guard: don't create review dispatch if implementation/rework is still active.
    // The card has already entered review canonically, so preserve the fresh
    // round/state while deferring only the counter-model dispatch creation.
    if (agentdesk.review.hasActiveWork(card.id)) {
      agentdesk.log.info("[review] Card " + card.id + " has active work dispatch — deferring review dispatch creation");
      return;
    }

    // Check review round limit — exceed → dilemma_pending with deadlock-manager notification
    var maxRounds = agentdesk.config.get("max_review_rounds") || 3;
    if (newRound > maxRounds) {
      escalateToManualIntervention(card.id, "Max review rounds (" + maxRounds + ") exceeded — PM decision needed", {
        review: true,
        reviewStateSync: { review_round: newRound },
        skipEscalate: true
      });
      agentdesk.log.warn("[review] Max review rounds (" + maxRounds + ") reached for " + card.id + " → dilemma_pending");
      notifyDeadlockManager(
        "⚠️ [Review Deadlock] " +
          (card.github_issue_number ? ("#" + card.github_issue_number + " ") : "") +
          card.id + "\n" +
          "card_id: " + card.id + "\n" +
          "agent: " + card.assigned_agent_id + "\n" +
          "review round " + newRound + " exceeded max " + maxRounds,
        "review-automation"
      );
      return;
    }
    // Create review dispatch (targets same agent — counter channel picks it up)
    // #245: Log agent_id for diagnostics — "project-agentdesk-cdx" phantom agent was traced here
    agentdesk.log.info("[review] Creating review dispatch: card=" + card.id + " agent=" + card.assigned_agent_id + " round=" + newRound);
    try {
      var latestWorkDispatch = loadLatestCompletedWorkDispatch(card.id);
      var reviewDispatchContext = buildReviewDispatchContext(latestWorkDispatch);
      var noopReviewContext = buildNoopReviewContext(latestWorkDispatch);
      if (noopReviewContext) {
        reviewDispatchContext = mergeObjects(reviewDispatchContext, noopReviewContext);
        agentdesk.log.info(
          "[review] Card " + card.id + " entering noop_verification mode from " + latestWorkDispatch.id
        );
      }
      var reviewDispatchId = agentdesk.dispatch.create(
        card.id,
        card.assigned_agent_id,
        "review",
        "[Review R" + newRound + "] " + card.id,
        reviewDispatchContext
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
      "SELECT id, kanban_card_id, dispatch_type, result, context FROM task_dispatches WHERE id = ?",
      [payload.dispatch_id]
    );
    if (dispatches.length === 0) return;
    var dispatch = dispatches[0];

    // #198/#211: create-pr dispatch completed — canonicalize PR tracking, then wait for CI
    if (dispatch.dispatch_type === "create-pr") {
      // #743: Stale guard. Every create-pr dispatch carries a
      // dispatch_generation stamp in its context. Compare with the current
      // generation on pr_tracking. Mismatch = this completion belongs to a
      // prior lifecycle (card was reopened, reseeded, etc.) — noop.
      var stampCtx = parseJsonObject(dispatch.context);
      var stampGen = stampCtx.dispatch_generation;
      if (!stampGen) {
        // #743: Missing stamp means this dispatch was created before the
        // generation-stamp contract (pre-v8 rollout). The zero-inflight gate
        // should make this impossible, but if we observe one, reseed the
        // tracking row so the retry loop can start fresh.
        agentdesk.log.warn(
          "[review] create-pr completion for card " + dispatch.kanban_card_id +
          " has no dispatch_generation stamp — reseeding (legacy recovery)"
        );
        try {
          agentdesk.reviewAutomation.reseedPrTracking(dispatch.kanban_card_id);
        } catch (e) {
          agentdesk.log.error("[review] reseedPrTracking failed: " + e);
        }
        return;
      }
      var cardMeta = agentdesk.db.query(
        "SELECT repo_id, github_issue_url FROM kanban_cards WHERE id = ?",
        [dispatch.kanban_card_id]
      );
      var tracking = loadPrTracking(dispatch.kanban_card_id);
      if (!tracking || tracking.dispatch_generation !== stampGen) {
        agentdesk.log.info(
          "[review] stale create-pr completion for card " + dispatch.kanban_card_id +
          " (stamp=" + stampGen +
          ", current=" + (tracking ? tracking.dispatch_generation : "<no tracking>") +
          ") — noop"
        );
        return;
      }
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
        // #5716 slice B: do NOT pre-seed the cause with a separate upsert. recordPrCreateFailure writes
        // last_error AND retry_count in one transaction; a crash between an upsert and that op left a
        // recorded failure at retry_count = 0, which the sweep never sees. That upsert's last_error was
        // overwritten by the same op microseconds later anyway, and since branch/head_sha are assigned
        // without COALESCE it also clobbered them with the very nulls that led here.
        //
        // Lifecycle guard: only force terminal if the card is still in a
        // PR-pending state. A delayed create-pr failure that arrives
        // AFTER the card has been reopened for rework must not overwrite
        // the newer workflow (terminal transitions cancel live dispatches
        // and clear review state).
        agentdesk.log.warn("[review] Create-PR completed but canonical tracking is incomplete for card " + dispatch.kanban_card_id);
        if (isCardEligibleForPrFailureTerminalize(dispatch.kanban_card_id)) {
          markPrCreateFailed(dispatch.kanban_card_id, "missing_canonical_tracking", stampGen);
        } else {
          recordPrCreateFailureForSweep(dispatch.kanban_card_id, "missing_canonical_tracking", stampGen);
        }
        return;
      }

      var pr = findOpenPrByTrackedBranch(repoId, branch);
      if (!pr) {
        // #5716 slice B: single-op failure record — see the note above.
        agentdesk.log.warn("[review] Create-PR completed but no open PR was found for card " + dispatch.kanban_card_id + " branch " + branch);
        if (isCardEligibleForPrFailureTerminalize(dispatch.kanban_card_id)) {
          markPrCreateFailed(dispatch.kanban_card_id, "no_open_pr_found", stampGen);
        } else {
          recordPrCreateFailureForSweep(dispatch.kanban_card_id, "no_open_pr_found", stampGen);
        }
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
      var completeCfg = agentdesk.pipeline.resolveForCard(dispatch.kanban_card_id);
      var completeTerminalState = agentdesk.pipeline.terminalState(completeCfg);
      var lifecycleRows = agentdesk.db.query(
        "SELECT status FROM kanban_cards WHERE id = ?",
        [dispatch.kanban_card_id]
      );
      var currentStatus = lifecycleRows.length > 0 ? lifecycleRows[0].status : null;
      // Find review state by scanning for the state with a review_passed gated
      // outbound transition, rather than assuming a fixed 2-hop shape from kickoff.
      //
      // #2051 Finding 14 (P2): when a custom pipeline declares multiple
      // `review_passed` gated transitions (e.g. peer-review → senior-review →
      // done), prefer the transition whose `from` matches the card's current
      // status. The legacy "first match" path used the first declaration
      // which could point at the wrong review state, mis-routing the card.
      var completeReviewState = null;
      var completeReviewPassTarget = completeTerminalState;
      if (completeCfg && completeCfg.transitions) {
        if (currentStatus) {
          for (var tiPref = 0; tiPref < completeCfg.transitions.length; tiPref++) {
            var trPref = completeCfg.transitions[tiPref];
            if (
              trPref.type === "gated" &&
              trPref.gates &&
              trPref.gates.indexOf("review_passed") >= 0 &&
              trPref.from === currentStatus
            ) {
              completeReviewState = trPref.from;
              completeReviewPassTarget = trPref.to;
              break;
            }
          }
        }
        if (!completeReviewState) {
          for (var ti = 0; ti < completeCfg.transitions.length; ti++) {
            var tr = completeCfg.transitions[ti];
            if (tr.type === "gated" && tr.gates && tr.gates.indexOf("review_passed") >= 0) {
              completeReviewState = tr.from;
              completeReviewPassTarget = tr.to;
              break;
            }
          }
        }
      }
      if (!completeReviewState) {
        // Fallback: 2-hop walk from kickoff (legacy behavior)
        var completeInitialState = agentdesk.pipeline.kickoffState(completeCfg);
        var completeInProgressState = agentdesk.pipeline.nextGatedTarget(completeInitialState, completeCfg);
        completeReviewState = agentdesk.pipeline.nextGatedTarget(completeInProgressState, completeCfg);
        completeReviewPassTarget = agentdesk.pipeline.nextGatedTargetWithGate(completeReviewState, "review_passed", completeCfg) || completeTerminalState;
      }
      var statusEligible =
        currentStatus === completeReviewState ||
        currentStatus === completeReviewPassTarget ||
        (currentStatus === completeTerminalState && completeReviewPassTarget === completeTerminalState);

      if (!statusEligible) {
        agentdesk.log.info(
          "[review] Create-PR completed for card " + dispatch.kanban_card_id +
          " → wait-ci on PR #" + pr.number +
          " (status transition skipped; current status " + currentStatus + " is outside review lifecycle)"
        );
        return;
      }

      if (currentStatus === completeReviewState) {
        agentdesk.kanban.setStatus(dispatch.kanban_card_id, completeReviewPassTarget, true);
      }
      // Set blocked_reason AFTER setStatus — terminal transitions clear
      // blocked_reason as part of cleanup, so this must come last.
      agentdesk.db.execute(
        "UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = ?",
        [dispatch.kanban_card_id]
      );
      agentdesk.log.info(
        "[review] Create-PR completed for card " + dispatch.kanban_card_id +
        " → wait-ci on PR #" + pr.number +
        " (card lifecycle " + currentStatus + " → " + completeReviewPassTarget + ")"
      );
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

    // review-decision dispatches must never spawn another review-decision followup.
    // If they finish without an explicit verdict, leave resolution to manual/API paths.
    if (dispatch.dispatch_type === "review-decision" && !verdict) {
      agentdesk.log.info("[review] review-decision completed without explicit verdict — skipping follow-up dispatch. dispatch=" + dispatch.id);
      return;
    }

    // Legacy fallback: if a review dispatch somehow arrives completed without an
    // explicit verdict, create a review-decision dispatch so the original agent
    // can inspect the review comments and decide the outcome.
    if (!verdict && result.auto_completed && dispatch.dispatch_type === "review") {
      var card = agentdesk.cards.get(dispatch.kanban_card_id);
      // Guard: skip dispatch creation for terminal cards — prevents stale review loops after dismiss
      if (card && agentdesk.pipeline.isTerminal(card.status, cfg)) {
        agentdesk.log.info("[review] Card " + dispatch.kanban_card_id + " already terminal — skipping review-decision dispatch");
        return;
      }
      if (card && card.assigned_agent_id) {
        var issueNum = card.github_issue_number || "?";
        // #2051 Finding 26 (P2): dedupe pending review-decision dispatches.
        // If the previous round's review-decision is still pending/dispatched
        // (typically because nobody has responded yet), creating another one
        // every time a fresh review auto-completes leads to a pile of
        // un-answered decision dispatches per card. Skip creation when an
        // active one already exists for the same card.
        var pendingDecisionRows = agentdesk.db.query(
          "SELECT id FROM task_dispatches " +
          "WHERE kanban_card_id = ? AND dispatch_type = 'review-decision' " +
          "AND status IN ('pending', 'dispatched') LIMIT 1",
          [dispatch.kanban_card_id]
        );
        if (pendingDecisionRows.length > 0) {
          agentdesk.log.info(
            "[review] Card " + dispatch.kanban_card_id +
            " already has an active review-decision dispatch (" +
            pendingDecisionRows[0].id + ") — skipping duplicate creation"
          );
          return;
        }
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
    processVerdict(dispatch.kanban_card_id, verdict, result, {
      review_dispatch_id: dispatch.dispatch_type === "review" ? dispatch.id : null
    });
  },

  // ── Review Verdict — from /api/reviews/verdict ─────────────
  onReviewVerdict: function(payload) {
    if (!payload.card_id || !payload.verdict) return;
    processVerdict(payload.card_id, payload.verdict, payload, {
      review_dispatch_id: payload.dispatch_id || null
    });
  }
};

// #197: Check if PR has .rs file changes (skip condition for e2e-test)
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
  if (typeof raw === "object") return raw;
  if (typeof raw !== "string") return {};
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

function loadLatestCompletedWorkDispatch(cardId) {
  var rows = agentdesk.db.query(
    "SELECT id, dispatch_type, result, context FROM task_dispatches " +
    "WHERE kanban_card_id = ? " +
    "AND dispatch_type IN ('implementation', 'rework') " +
    "AND status = 'completed' " +
    "ORDER BY COALESCE(completed_at, updated_at) DESC, rowid DESC LIMIT 1",
    [cardId]
  );
  if (rows.length === 0) return null;

  return {
    id: rows[0].id,
    dispatch_type: rows[0].dispatch_type,
    result: parseJsonObject(rows[0].result),
    context: parseJsonObject(rows[0].context)
  };
}

function buildNoopReviewContext(workDispatch) {
  if (!workDispatch || !workDispatch.result) return null;

  var result = workDispatch.result;
  if (result.work_outcome !== "noop" && result.completed_without_changes !== true) {
    return null;
  }

  var noopReason = firstPresent(
    result.noop_reason,
    result.notes,
    result.summary,
    result.feedback
  );
  var reviewContext = {
    review_mode: "noop_verification",
    noop_reason: noopReason || "noop 사유가 제공되지 않았습니다.",
    noop_work_outcome: result.work_outcome || "noop",
    noop_result: result
  };
  if (workDispatch.id) {
    reviewContext.parent_dispatch_id = workDispatch.id;
  }
  return reviewContext;
}

function mergeObjects(base, overlay) {
  var merged = {};
  var key;
  base = base || {};
  overlay = overlay || {};
  for (key in base) {
    if (Object.prototype.hasOwnProperty.call(base, key)) merged[key] = base[key];
  }
  for (key in overlay) {
    if (Object.prototype.hasOwnProperty.call(overlay, key)) merged[key] = overlay[key];
  }
  return merged;
}

function normalizeSlotIndex(value) {
  if (typeof value === "number" && isFinite(value) && Math.floor(value) === value && value >= 0) {
    return value;
  }
  if (typeof value === "string" && value.trim() !== "") {
    var parsed = Number(value);
    if (isFinite(parsed) && Math.floor(parsed) === parsed && parsed >= 0) {
      return parsed;
    }
  }
  return null;
}

function buildReviewDispatchContext(workDispatch) {
  var reviewContext = {};
  if (!workDispatch) return reviewContext;
  if (workDispatch.id) {
    reviewContext.parent_dispatch_id = workDispatch.id;
  }
  var result = workDispatch.result || {};
  var workContext = workDispatch.context || {};
  if (workContext.entry_id) {
    reviewContext.entry_id = workContext.entry_id;
  }
  var slotIndex = normalizeSlotIndex(workContext.slot_index);
  if (slotIndex !== null) {
    reviewContext.slot_index = slotIndex;
  }
  var reviewedCommit = firstPresent(
    result.completed_commit,
    result.reviewed_commit,
    result.head_sha,
    workContext.completed_commit,
    workContext.reviewed_commit,
    workContext.head_sha
  );
  if (reviewedCommit) {
    reviewContext.reviewed_commit = reviewedCommit;
  }
  var worktreePath = firstPresent(
    result.completed_worktree_path,
    result.worktree_path,
    workContext.completed_worktree_path,
    workContext.worktree_path
  );
  if (worktreePath) {
    reviewContext.worktree_path = worktreePath;
  }
  var branch = firstPresent(
    result.completed_branch,
    result.worktree_branch,
    result.branch,
    workContext.completed_branch,
    workContext.worktree_branch,
    workContext.branch
  );
  if (branch) {
    reviewContext.branch = branch;
  }
  var targetRepo = firstPresent(result.target_repo, workContext.target_repo);
  if (targetRepo) {
    reviewContext.target_repo = targetRepo;
  }
  return reviewContext;
}

function loadLatestCompletedWorkTarget(cardId) {
  var latestWork = loadLatestCompletedWorkDispatch(cardId);
  if (!latestWork) return null;

  var result = latestWork.result || {};
  var context = latestWork.context || {};
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

function isMainlineBranch(branch) {
  return branch === "main" || branch === "master" || branch === "origin/main" || branch === "origin/master";
}

function gitCommandOutput(repoPath, args) {
  if (!repoPath) return null;
  try {
    var output = agentdesk.exec("git", ["-C", repoPath].concat(args));
    if (output && String(output).indexOf("ERROR") === 0) return null;
    return String(output || "");
  } catch (e) {
    return null;
  }
}

function gitRefExists(repoPath, ref) {
  var output = gitCommandOutput(repoPath, ["rev-parse", "--verify", ref]);
  return output !== null && output.trim() !== "";
}

function gitCommandOk(repoPath, args) {
  if (!repoPath) return false;
  try {
    var output = agentdesk.exec("git", ["-C", repoPath].concat(args));
    return !(output && String(output).indexOf("ERROR") === 0);
  } catch (e) {
    return false;
  }
}

function workTargetAlreadyOnRemoteMain(workTarget) {
  if (!workTarget || !workTarget.head_sha || !workTarget.worktree_path) return false;
  if (workTarget.branch && !isMainlineBranch(workTarget.branch)) return false;
  var remoteRefs = ["origin/main", "origin/master"];
  for (var i = 0; i < remoteRefs.length; i++) {
    var ref = remoteRefs[i];
    if (!gitRefExists(workTarget.worktree_path, ref)) continue;
    if (gitCommandOk(workTarget.worktree_path, ["merge-base", "--is-ancestor", workTarget.head_sha, ref])) {
      return true;
    }
  }
  return false;
}

function loadPrTracking(cardId) {
  return prTracking.load(cardId);
}

function upsertPrTracking(cardId, repoId, worktreePath, branch, prNumber, headSha, state, lastError) {
  return prTracking.upsert(cardId, repoId, worktreePath, branch, prNumber, headSha, state, lastError);
}

// #701: Create-PR dispatch helper used by review-pass flow.
//
// Returns a structured result:
//   { status: "dispatched" }                      — create-pr dispatch queued
//   { status: "noop",  reason: "<code>" }         — no PR needed (safe-to-terminal)
//   { status: "error", reason: "<code>: <msg>" }  — expected to PR but couldn't
//
// Semantics:
//   - "noop" means there is nothing meaningful to PR: noop_verification review,
//     or the card carries no completed work target / no repo association.
//     These cases correspond to cards that never had an implementation worth
//     shipping; callers can safely move them to terminal.
//   - "error" means we have enough evidence that a PR was expected (work target
//     present, agent + repo resolvable) but the final step broke — typically
//     the `agentdesk.dispatch.create` throw, or a surprising metadata gap on
//     an already-tracked worktree. On dispatch failure the helper upserts
//     pr_tracking with last_error so the failure keeps its cause; callers must
//     mark it terminal + blocked via markPrCreateFailed, which hands off (#5716).
function attemptCreatePrDispatchForReviewPass(cardId, noopVerification) {
  if (noopVerification) {
    agentdesk.log.info("[review] Card " + cardId + " passed noop_verification — skipping create-pr dispatch");
    return { status: "noop", reason: "noop_verification" };
  }

  // No completed work dispatch → nothing to PR. This is how tests and
  // review-only cards reach review pass without an implementation; it is
  // not a failure, so callers should terminal the card.
  var latestWorkTarget = loadLatestCompletedWorkTarget(cardId);
  if (!latestWorkTarget) return { status: "noop", reason: "no_work_target" };

  if (workTargetAlreadyOnRemoteMain(latestWorkTarget)) {
    agentdesk.log.info("[review] Card " + cardId + " work target is already on origin mainline — skipping create-pr dispatch");
    return { status: "noop", reason: "already_on_remote_main" };
  }

  var prCardInfo = agentdesk.db.query(
    "SELECT assigned_agent_id, title, github_issue_number, repo_id, github_issue_url FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  // Card gone entirely is a genuine noop — nothing to track.
  if (prCardInfo.length === 0) return { status: "noop", reason: "card_missing" };

  // #701: A card with completed work AND a resolvable repo but NO assigned
  // agent is a genuine anomaly, not a benign noop — there is shippable
  // work but no one to drive the create-pr dispatch. Returning noop here
  // would silently drop the card to done with no PR and no retry row.
  // Seed pr_tracking with last_error so operators see the cause behind the
  // card's pr:create_failed marker. Callers must treat error → terminal
  // + blocked_reason via markPrCreateFailed.
  // #2051 Finding 7 (P2): `no_agent` is not a retryable failure — automated
  // retry will not assign an agent. The legacy path leaned on the 3-retry
  // escalate from `recordPrCreateFailure`, which meant 3 silent rounds before
  // the human alert and pollution of `retry_count` for the actual create-pr
  // retries that follow once the agent is assigned. Surface it to manual
  // intervention immediately and return `no_agent_escalated` so the caller
  // skips the normal retry-bump path.
  var precheckRepoId = prCardInfo[0].repo_id
    || extractRepoFromIssueUrl(prCardInfo[0].github_issue_url);
  if (!prCardInfo[0].assigned_agent_id) {
    if (precheckRepoId) {
      upsertPrTracking(
        cardId,
        precheckRepoId,
        latestWorkTarget.worktree_path,
        latestWorkTarget.branch,
        null,
        latestWorkTarget.head_sha,
        "create-pr",
        "no_assigned_agent_for_create_pr"
      );
    }
    try {
      escalateToManualIntervention(
        cardId,
        "Create-PR aborted: card has no assigned_agent_id — operator must assign before retry"
      );
    } catch (escErr) {
      agentdesk.log.warn(
        "[review] no_agent escalation to manual intervention failed for " + cardId + ": " + escErr
      );
    }
    return { status: "error", reason: "no_agent_escalated" };
  }

  var agentId = prCardInfo[0].assigned_agent_id;
  var repoId = precheckRepoId;
  if (!repoId) return { status: "noop", reason: "no_repo" };

  // We have a work target AND a repo, so a PR was expected. From here on,
  // inability to dispatch is a genuine error that the retry loop should see.
  if (!latestWorkTarget.branch) {
    // Seed pr_tracking with whatever we have so the retry loop can try again
    // once the branch recovers (e.g. worktree re-discovered).
    upsertPrTracking(
      cardId,
      repoId,
      latestWorkTarget.worktree_path,
      null,
      null,
      latestWorkTarget.head_sha,
      "create-pr",
      "missing_branch_at_handoff"
    );
    return { status: "error", reason: "missing_branch" };
  }

  var issueNum = prCardInfo[0].github_issue_number || "?";
  try {
    // #743: Atomic handoff — seeds pr_tracking (with fresh dispatch_generation
    // stamp), inserts stamped dispatch, and sets blocked_reason='pr:creating'
    // in a single transaction. Idempotent-reuses an existing active dispatch
    // per the C5 dedupe contract.
    var handoff = agentdesk.reviewAutomation.handoffCreatePr(cardId, {
      repo_id: repoId,
      worktree_path: latestWorkTarget.worktree_path,
      branch: latestWorkTarget.branch,
      head_sha: latestWorkTarget.head_sha,
      agent_id: agentId,
      title: "[PR 생성] #" + issueNum + " " + prCardInfo[0].title
    });
    agentdesk.log.info(
      "[review] Create-PR handoff for card " + cardId +
      " gen=" + handoff.generation +
      (handoff.reused ? " (reused existing dispatch)" : "")
    );
    return { status: "dispatched", generation: handoff.generation, reused: !!handoff.reused };
  } catch (e) {
    // handoff threw before any stamp was committed — the JS catch path
    // calls markPrCreateFailed(null stampGen) which seeds a retry row via
    // recordPrCreateFailure's INSERT-if-missing branch.
    agentdesk.log.warn("[review] handoffCreatePr failed for card " + cardId + ": " + e);
    return { status: "error", reason: "dispatch_failed: " + String(e) };
  }
}

// #701: Lifecycle guard for create-pr FAILURE completion paths. Returns
// true iff the card is still in a state where a create-pr failure should
// force terminal. If the card has moved on (e.g. already terminal with a
// non-review terminal target, or reopened for rework and now back in an
// in-progress state), a stale late-arriving create-pr failure must NOT
// retroactively terminalize — terminal transitions cancel active
// implementation/rework dispatches and clear review state. Callers still
// record the failure on pr_tracking (recordPrCreateFailureOnly) so the #5716
// stranded-row sweep hands the card off without terminalizing it.
function isCardEligibleForPrFailureTerminalize(cardId) {
  var cfg = agentdesk.pipeline.resolveForCard(cardId);
  var terminalState = agentdesk.pipeline.terminalState(cfg);
  var rows = agentdesk.db.query(
    "SELECT status FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (rows.length === 0) return false;
  var currentStatus = rows[0].status;
  // Walk the config to find the review state (the from-state of a
  // gated transition marked with review_passed) without hardcoding the
  // pipeline shape.
  var reviewState = null;
  var reviewPassTarget = terminalState;
  if (cfg && cfg.transitions) {
    for (var ti = 0; ti < cfg.transitions.length; ti++) {
      var tr = cfg.transitions[ti];
      if (tr.type === "gated" && tr.gates && tr.gates.indexOf("review_passed") >= 0) {
        reviewState = tr.from;
        reviewPassTarget = tr.to;
        break;
      }
    }
  }
  // Idempotent terminal: already-terminal cards are safe to "terminalize"
  // again (markPrCreateFailed is idempotent when status matches).
  // In-review and review-pass-target cards are the expected lifecycle.
  return currentStatus === terminalState
    || currentStatus === reviewState
    || currentStatus === reviewPassTarget;
}

// #5716: record a create-pr failure on pr_tracking WITHOUT terminalizing the
// card — retry_count > 0 is what the sweep looks for, so a caller that must
// skip the terminal transition still reaches a handoff.
function recordPrCreateFailureOnly(cardId, errorMsg, stampGen) {
  try {
    return agentdesk.reviewAutomation.recordPrCreateFailure(cardId, errorMsg, stampGen || "");
  } catch (e) {
    agentdesk.log.error("[review] recordPrCreateFailure threw for card " + cardId + ": " + e);
    return null;
  }
}

// #5716 r5: dedup namespace for a failure with no dispatch generation of its own — reading the row back
// returned the PREVIOUS, already-alerted generation, which swallowed the new alert for the 7-day TTL.
function preHandoffGeneration(prefix, errorMsg) {
  return "pre:" + prefix + String(errorMsg || "unknown").split(":")[0];
}

// #5716 r5: a record op that recorded nothing leaves retry_count at 0 (invisible to the sweep) and this branch stamps no blocked_reason either — hand off now instead of logging a record that did not happen.
function recordPrCreateFailureForSweep(cardId, errorMsg, stampGen) {
  if (recordPrCreateFailureOnly(cardId, errorMsg, stampGen)) {
    agentdesk.log.info("[review] card " + cardId + " has moved past the review lifecycle (likely reopened); create-pr failure recorded on pr_tracking for the #5716 sweep");
    return;
  }
  handOffPrCreateFailure(cardId, errorMsg, null, preHandoffGeneration("record_failed:", errorMsg));
}

// #5716: durable marker so a handed-off row leaves the sweep candidate set —
// without it ORDER BY updated_at / LIMIT 20 re-selects the same oldest rows
// forever and the 21st never lands. The next recordPrCreateFailure overwrites
// last_error and re-arms the row for that generation.
function markPrCreateHandedOff(cardId, errorMsg) {
  agentdesk.db.execute(
    "UPDATE pr_tracking SET last_error = ?, updated_at = datetime('now') " +
    "WHERE card_id = ? AND state IN ('create-pr', 'escalated')",
    ["handed-off:" + errorMsg, cardId]
  );
}

// #5716: escalate()/flushEscalations drops pending entries for terminal cards (loadManualInterventionState
// reports them inactive), so notifyDeadlockManager is the durable handoff. The kv key carries
// pr_tracking.dispatch_generation — the per-dispatch UUID — and NOT retry_count:
// seed_pg_pr_tracking_handoff_state, refresh_pg_pr_tracking_reuse_state_if_active and reseed_pr_tracking_pg
// all reset retry_count to 0, so every generation's FIRST failure is retry_count=1 and a card's second
// failure inside one kv TTL window collided with the first and went silent. The durable sweep marker is
// written only alongside a fresh alert: on a dedup hit the row deliberately stays a sweep candidate, so a
// colliding key (the ':?' fallback, used when no generation is recorded) returns after the TTL instead of
// being excluded forever. Returns true when the generation is settled (alerted now or already alerted);
// false ONLY when the alert was not delivered, which keeps the row in the sweep candidate set.
function handOffPrCreateFailure(cardId, errorMsg, retryCount, generation) {
  var dedupKey = "pr_create_handoff:" + cardId + ":" + (generation || preHandoffGeneration("", errorMsg));
  if (agentdesk.kv.get(dedupKey)) {
    // Still a sweep candidate (no marker ⇒ it self-heals once the kv TTL lapses), but bumped so it stops
    // pinning the head of ORDER BY updated_at LIMIT 20 — 20 such rows starved every newer stranded card.
    agentdesk.db.execute("UPDATE pr_tracking SET updated_at = datetime('now') WHERE card_id = ? AND dispatch_generation = ?", [cardId, generation || ""]);
    return "deduped";
  }
  var card = agentdesk.cards.get(cardId);
  var delivered = notifyDeadlockManager(
    "⚠️ [Create-PR Handoff] " + (card && card.github_issue_number ? ("#" + card.github_issue_number + " ") : "") + cardId +
      "\nagent: " + ((card && card.assigned_agent_id) || "(unassigned)") +
      "\nerror: " + errorMsg + " (retry_count=" + (retryCount == null ? "?" : retryCount) + ")" +
      "\nno automatic retry — an agent or operator must retry or close this card",
    "review-automation"
  );
  if (!delivered) {
    agentdesk.log.error("[review] Create-PR handoff alert for card " + cardId +
      " was NOT delivered (no alert channel, or the outbox enqueue failed) — leaving the row for the next sweep");
    return false;
  }
  agentdesk.kv.set(dedupKey, errorMsg, 604800);
  markPrCreateHandedOff(cardId, errorMsg);
  return true;
}

// #5716: bounded sweep for create-pr rows stranded by the removed retry loop.
// Candidacy lives on pr_tracking alone — retry_count > 0 is the durable "a
// failure was recorded" signal (handoffCreatePr resets it to 0 on each new
// dispatch; completion moves the row to 'wait-ci'). That covers what the old
// card-status/blocked_reason join missed — cards that never reached terminal,
// rows the Rust retry_count>=3 threshold moved to 'escalated', rows whose
// blocked_reason was lost to a crash before the marker UPDATE — and dropping
// the global terminalState equality covers per-card pipeline overrides and
// multi-terminal pipelines. The 30-day floor is a ROLLING window on updated_at, not a removal boundary: it
// caps the deploy-time page storm from historical failures, and it also expires a failure nobody could
// alert (no alert channel) 30 days after its last write — see the PR body.
function sweepStrandedPrCreateFailures() {
  var rows = agentdesk.db.query(
    "SELECT card_id, last_error, retry_count, dispatch_generation FROM pr_tracking " +
    "WHERE state IN ('create-pr', 'escalated') AND retry_count > 0 " +
    "AND COALESCE(last_error, '') NOT LIKE 'handed-off:%' " +
    "AND updated_at > datetime('now', '-30 days') " +
    "ORDER BY updated_at LIMIT 20",
    []
  );
  var handed = 0;
  var deduped = 0;
  for (var i = 0; i < rows.length; i++) {
    var err = rows[i].last_error || "stranded create-pr tracking row";
    var outcome = handOffPrCreateFailure(rows[i].card_id, err, rows[i].retry_count, rows[i].dispatch_generation);
    if (outcome === "deduped") deduped++; else if (outcome) handed++;
  }
  if (handed > 0) agentdesk.log.warn("[review] Handed off " + handed + " stranded create-pr card(s) (#5716)");
  if (deduped > 0) agentdesk.log.info("[review] " + deduped + " create-pr row(s) were already alerted for their generation — bumped behind newer candidates (#5716)");
  return handed;
}

// #701: Shared helper for PR-handoff errors. Moves the card to its
// configured terminal state AND stamps blocked_reason='pr:create_failed:...'
// so the failure is visible on the kanban. For genuine dispatch failures the
// helper upserts pr_tracking with last_error first, so the handoff has a cause.
//
// Ordering: setStatus(terminal) FIRST, then write blocked_reason. Terminal
// transitions clear blocked_reason as part of their cleanup (see the
// create-pr completion path elsewhere in this file which already documents
// this same requirement — writing blocked_reason before setStatus would
// see the marker get wiped immediately).
// #743: JS orchestration (C4 literal). Order is:
//   1. recordPrCreateFailure — stale-guards via stampGen (or skips guard on
//      null stamp); seeds retry row if the handoff tx rolled back; increments
//      retry_count and flips pr_tracking.state='escalated' at >= 3.
//   2. setStatus(terminal, force=true) — terminal transitions clear
//      blocked_reason, which is why step 3 must follow.
//   3. blocked_reason UPDATE — escalated vs normal failure marker.
//   4. handOffPrCreateFailure — agent/operator handoff on EVERY failure generation (the dedup key carries
//      pr_tracking.dispatch_generation, not retry_count, which every new dispatch resets to 0): #5716
//      removed processTrackedMergeQueue, the only consumer of terminal `state='create-pr'` rows, so
//      nothing else will retry this card.
//
// stampGen can be null/undefined for pre-handoff failures (e.g. the handoff
// bridge op threw); recordPrCreateFailure handles that by skipping the stale
// guard and INSERTing a retry row when missing.
function markPrCreateFailed(cardId, reason, stampGen) {
  var cfg = agentdesk.pipeline.resolveForCard(cardId);
  var terminalState = agentdesk.pipeline.terminalState(cfg);
  var errorMsg = reason || "unknown";

  // 1. Record failure — atomic retry_count++ and escalate decision.
  var result = recordPrCreateFailureOnly(cardId, errorMsg, stampGen);

  // Stale generation — the card has moved on, do not terminalize.
  if (result && result.noop) {
    agentdesk.log.info(
      "[review] markPrCreateFailed noop for card " + cardId +
      " — stale generation (stamp=" + (stampGen || "") + ")"
    );
    return;
  }

  // 2. Terminalize.
  agentdesk.kanban.setStatus(cardId, terminalState, true);

  // 3. Stamp blocked_reason AFTER setStatus (setStatus clears blocked_reason).
  var retryCount = result ? result.retry_count : null;
  var blockedReason = (result && result.escalated)
    ? "pr:create_failed_escalated:max_retries"
    : "pr:create_failed:" + errorMsg;
  agentdesk.db.execute(
    "UPDATE kanban_cards SET blocked_reason = ?, updated_at = datetime('now') WHERE id = ?",
    [blockedReason, cardId]
  );

  // 4. Agent/operator handoff — see the #5716 note above. stampGen IS the dispatch generation when the
  // caller has one. A pre-handoff failure passes none, and a record op that recorded nothing leaves the row
  // invisible to the sweep — both get their own namespace rather than the row's stale, already-alerted one.
  var handedOff = handOffPrCreateFailure(cardId, errorMsg, retryCount,
    result ? stampGen : preHandoffGeneration("record_failed:", errorMsg));

  agentdesk.log.warn("[review] Card " + cardId + " marked " + blockedReason + " → " + terminalState +
    " (retry_count=" + (retryCount == null ? "?" : retryCount) +
    ", handoff=" + (handedOff ? "agent/operator" : "UNDELIVERED — sweep will retry") + ")");
}

function findOpenPrByTrackedBranch(repoId, branch) {
  return prTracking.findOpenPrByBranch(repoId, branch);
}

function loadLatestReviewDispatchContext(cardId, dispatchId) {
  if (dispatchId) {
    var exactRows = agentdesk.db.query(
      "SELECT context FROM task_dispatches " +
      "WHERE id = ? AND kanban_card_id = ? AND dispatch_type = 'review' LIMIT 1",
      [dispatchId, cardId]
    );
    if (exactRows.length > 0) {
      return parseJsonObject(exactRows[0].context);
    }
  }

  // #2051 Finding 6 (P1): when no explicit dispatch_id is provided we used to
  // return the single newest review dispatch (pending/dispatched preferred,
  // otherwise the newest completed). That could pull a STALE round's context
  // (e.g. an R1 noop_verification record after R2 had moved on), making the
  // caller mis-classify the current verdict as `noop_verification` and skip
  // PR creation. Constrain candidates to the card's current `review_round`
  // by parsing the `review_round_at_dispatch` field that
  // `review_automation_ops` stamps into the dispatch context.
  var roundRows = agentdesk.db.query(
    "SELECT review_round FROM kanban_cards WHERE id = ? LIMIT 1",
    [cardId]
  );
  var currentRound = (roundRows.length > 0 && roundRows[0].review_round != null)
    ? Number(roundRows[0].review_round)
    : null;

  var rows = agentdesk.db.query(
    "SELECT context, status FROM task_dispatches " +
    "WHERE kanban_card_id = ? AND dispatch_type = 'review' " +
    "ORDER BY CASE WHEN status IN ('pending', 'dispatched') THEN 0 ELSE 1 END ASC, " +
    "COALESCE(completed_at, updated_at, created_at) DESC, rowid DESC LIMIT 10",
    [cardId]
  );
  if (rows.length === 0) return {};

  if (currentRound != null && isFinite(currentRound)) {
    for (var i = 0; i < rows.length; i++) {
      var ctxCandidate = parseJsonObject(rows[i].context);
      var ctxRound = (ctxCandidate && ctxCandidate.review_round_at_dispatch != null)
        ? Number(ctxCandidate.review_round_at_dispatch)
        : null;
      if (ctxRound != null && isFinite(ctxRound) && ctxRound === currentRound) {
        return ctxCandidate;
      }
    }
    // No round-matched dispatch found; fall through to legacy behaviour but
    // log so operators can spot stale-context misroutes during multi-round
    // flows.
    agentdesk.log.warn(
      "[review] loadLatestReviewDispatchContext: no review dispatch context matched card " +
      cardId + " review_round=" + currentRound + " — falling back to newest"
    );
  }

  return parseJsonObject(rows[0].context);
}

function processVerdict(cardId, verdict, result, options) {
  var opts = options || {};
  // Guard: skip processing for terminal cards — prevents stale dispatches from
  // re-triggering review state changes after dismiss.
  var cfg = agentdesk.pipeline.resolveForCard(cardId);
  var terminalState = agentdesk.pipeline.terminalState(cfg);
  var initialState = agentdesk.pipeline.kickoffState(cfg);
  var inProgressState = agentdesk.pipeline.nextGatedTarget(initialState, cfg);

  var card = agentdesk.cards.get(cardId);
  if (card && agentdesk.pipeline.isTerminal(card.status, cfg)) {
    agentdesk.log.info("[review] processVerdict skipped — card " + cardId + " already terminal");
    return;
  }

  var fallbackReviewState = agentdesk.pipeline.nextGatedTarget(inProgressState, cfg);
  var currentState = card ? card.status : null;
  var currentReviewPassTarget = currentState
    ? agentdesk.pipeline.nextGatedTargetWithGate(currentState, "review_passed", cfg)
    : null;
  var currentReviewReworkTarget = currentState
    ? agentdesk.pipeline.nextGatedTargetWithGate(currentState, "review_rework", cfg)
    : null;
  var reviewState = (currentReviewPassTarget || currentReviewReworkTarget)
    ? currentState
    : fallbackReviewState;
  var reviewPassTarget = agentdesk.pipeline.nextGatedTargetWithGate(reviewState, "review_passed", cfg) || terminalState;
  var reviewReworkTarget = agentdesk.pipeline.nextGatedTargetWithGate(reviewState, "review_rework", cfg) || inProgressState;

  var latestReviewContext = loadLatestReviewDispatchContext(cardId, opts.review_dispatch_id);
  var noopVerification = latestReviewContext.review_mode === "noop_verification";

  // #116: accept is NOT a counter-model verdict — it's an agent's review-decision action
  // (rework continuation). Only pass/approved route to done/next-stage.
  if (verdict === "pass" || verdict === "approved") {
    emitReviewVerdictQualityEvent(cardId, verdict, result, opts, "review_pass");
    // #2051 Finding 12 (P2): the two state-syncs below execute in separate
    // bridge calls (each commits its own SQL transaction). A throw from one
    // used to leave the card with `review_status = NULL` but
    // `card_review_state = reviewing`, which confused timeouts.js [D] /
    // review auto-accept paths. Wrap each call so we (1) emit a structured
    // warning when only one half succeeds, and (2) attempt a best-effort
    // re-sync so subsequent ticks see a consistent state. A true 2-table
    // transaction would need a dedicated Rust op (`review.commitVerdict`);
    // until then this best-effort guard at least surfaces the inconsistency.
    var reviewStatusOk = false;
    try {
      agentdesk.kanban.setReviewStatus(cardId, null, {suggestion_pending_at: null});
      reviewStatusOk = true;
    } catch (e) {
      agentdesk.log.error(
        "[review] Finding12: setReviewStatus(null) failed for " + cardId +
        " (verdict=" + verdict + "): " + e
      );
    }

    // #117: Update canonical card_review_state — review passed
    var reviewStateOk = false;
    try {
      agentdesk.reviewState.sync(cardId, "idle", { last_verdict: verdict });
      reviewStateOk = true;
    } catch (e) {
      agentdesk.log.error(
        "[review] Finding12: reviewState.sync(idle) failed for " + cardId +
        " (verdict=" + verdict + "): " + e
      );
      // Best-effort: if setReviewStatus succeeded but the canonical state
      // sync failed, leave a warning marker on the card so operators / the
      // next sweep can spot the drift instead of silently moving forward.
      if (reviewStatusOk) {
        try {
          agentdesk.db.execute(
            "UPDATE kanban_cards SET blocked_reason = COALESCE(blocked_reason, 'review:state_sync_failed'), " +
            "updated_at = datetime('now') WHERE id = ? AND blocked_reason IS NULL",
            [cardId]
          );
        } catch (markErr) {
          agentdesk.log.warn(
            "[review] Finding12: could not stamp state_sync_failed marker for " +
            cardId + ": " + markErr
          );
        }
      }
    }
    // If review_status update failed but reviewState.sync succeeded, the
    // canonical state is now `idle` but `review_status` may still read the
    // old value. A subsequent tick / reviewState.sync('idle') call will
    // converge; log it so the discrepancy is observable.
    if (!reviewStatusOk && reviewStateOk) {
      agentdesk.log.warn(
        "[review] Finding12: review_status NOT cleared for " + cardId +
        " — reviewState=idle but review_status update threw. " +
        "Subsequent tick should reconverge."
      );
    }

    // #701: PR creation is attempted in the review-pass branches that do NOT
    // queue a running pipeline stage — skip paths and the no-stage else branch.
    // It is intentionally NOT attempted on the non-skip e2e-test / normal-agent
    // dispatch paths: those keep ownership of the card via `pipeline_stage_id`
    // + `blocked_reason`, and letting pr_tracking enter `wait-ci` while those
    // pipelines are active would race ci-recovery with the pipeline stage
    // (ci-recovery overwrites `blocked_reason` to `ci:*`). Those pipelines
    // must trigger their own create-pr on completion via
    // agentdesk.reviewAutomation.attemptCreatePr.
    var prDispatched = false;

    // #701: noop_verification short-circuit. If the review passed on a
    // review whose work was "no changes needed", skip pipeline entry
    // entirely and go straight to terminal. Pipeline stages (e2e-test)
    // are meaningless for noop work, and without this short-circuit a
    // noop card would enter a non-skip pipeline and, on completion, the
    // stage would call agentdesk.reviewAutomation.attemptCreatePr(cardId)
    // which drops the noop_verification context, resulting in a real
    // create-pr dispatch being created for noop work (empty PRs, wasted
    // CI, possible auto-merge of "no changes").
    if (noopVerification) {
      agentdesk.log.info("[review] Card " + cardId + " noop_verification pass — skipping pipeline, going terminal directly");
      agentdesk.kanban.setStatus(cardId, reviewPassTarget, true);
      return;
    }

    // Review passed — check for next pipeline stage, otherwise terminal (#110)
    // Look for the next stage AFTER current pipeline_stage_id (stage_order based),
    // OR the first review_pass stage if card has no current pipeline stage.
    var cardInfo = agentdesk.cards.get(cardId);
    var nextStage = null;
    if (cardInfo && cardInfo.repo_id) {
      var repoId = cardInfo.repo_id;
      var currentStageId = cardInfo.pipeline_stage_id;

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
        // #701 DoD: always clear pipeline_stage_id on skip (including when the
        // card had no prior stage binding — still safe since NULL → NULL).
        agentdesk.db.execute(
          "UPDATE kanban_cards SET pipeline_stage_id = NULL, updated_at = datetime('now') WHERE id = ?",
          [cardId]
        );
        // #701: create-pr must fire here because the pipeline is being skipped
        // entirely — there is no e2e-test to create the PR later.
        // Safe relative to ci-recovery: the pipeline stage is cleared in the
        // same statement above, so the card has no active pipeline ownership.
        // All 3 outcomes go terminal; errors additionally stamp
        // blocked_reason='pr:create_failed:...' and hand off (#5716).
        var prResultNoRs = attemptCreatePrDispatchForReviewPass(cardId, noopVerification);
        prDispatched = (prResultNoRs.status === "dispatched");
        if (prResultNoRs.status === "dispatched") {
          agentdesk.log.info(
            "[review] Card " + cardId + " skipping pipeline stages (no .rs changes) — create-pr dispatched, awaiting CI/merge"
          );
        } else if (prResultNoRs.status === "error") {
          // #2051 Finding 7 (P2): `no_agent_escalated` already raised a manual
          // intervention upstream; skip the retry-bumping markPrCreateFailed.
          if (prResultNoRs.reason !== "no_agent_escalated") {
            markPrCreateFailed(cardId, prResultNoRs.reason);
          }
        } else {
          agentdesk.kanban.setStatus(cardId, reviewPassTarget, true);
          agentdesk.log.info(
            "[review] Card " + cardId + " skipping pipeline stages (no .rs changes) → " + reviewPassTarget
          );
        }
      } else {
        // Assign pipeline stage to card
        agentdesk.db.execute(
          "UPDATE kanban_cards SET pipeline_stage_id = ?, updated_at = datetime('now') WHERE id = ?",
          [nextStage.id, cardId]
        );
        agentdesk.log.info("[review] Card " + cardId + " passed review, entering pipeline stage: " + nextStage.stage_name);

        // #197: Counter-model stage (e2e-test) — dispatch only if DoD contains e2e item
        if (nextStage.provider === "counter") {
          // Skip e2e if DoD doesn't mention it — review pass goes straight to done
          var dodCheck = agentdesk.db.query(
            "SELECT description FROM kanban_cards WHERE id = ?", [cardId]
          );
          var dodText = (dodCheck.length > 0 && dodCheck[0].description) ? dodCheck[0].description.toLowerCase() : "";
          if (dodText.indexOf("e2e") === -1 && dodText.indexOf("end-to-end") === -1 && dodText.indexOf("end to end") === -1) {
            agentdesk.log.info("[review] Skipping e2e-test for card " + cardId + " — DoD has no e2e item");
            // Skip remaining pipeline stages and clear stage binding per #701 DoD.
            agentdesk.db.execute(
              "UPDATE kanban_cards SET pipeline_stage_id = NULL, blocked_reason = NULL, updated_at = datetime('now') WHERE id = ?",
              [cardId]
            );
            // #701: e2e skipped via DoD gate — same reasoning as the no_rs_changes
            // branch: the pipeline is cleared, so it's safe to seed pr_tracking.
            // noop → terminal; error → markPrCreateFailed (visible + retriable).
            var prResultE2eSkip = attemptCreatePrDispatchForReviewPass(cardId, noopVerification);
            prDispatched = (prResultE2eSkip.status === "dispatched");
            if (prResultE2eSkip.status === "error") {
              // #2051 Finding 7 (P2): suppress markPrCreateFailed if upstream
              // already escalated to manual intervention.
              if (prResultE2eSkip.reason !== "no_agent_escalated") {
                markPrCreateFailed(cardId, prResultE2eSkip.reason);
              }
            } else if (prResultE2eSkip.status === "noop") {
              var skipCfg = agentdesk.pipeline.resolveForCard(cardId);
              var skipTerminal = agentdesk.pipeline.terminalState(skipCfg);
              agentdesk.kanban.setStatus(cardId, skipTerminal, true);
            }
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
            escalateToManualIntervention(
              cardId,
              "Pipeline stage '" + nextStage.stage_name + "' has no assigned agent",
              { review: true }
            );
          }
        }
      }
    } else {
      // No more stages — clear pipeline_stage_id and mark terminal.
      if (cardInfo && cardInfo.pipeline_stage_id) {
        agentdesk.db.execute(
          "UPDATE kanban_cards SET pipeline_stage_id = NULL, updated_at = datetime('now') WHERE id = ?",
          [cardId]
        );
        agentdesk.log.info("[review] Card " + cardId + " completed all pipeline stages");
      }

      // #198/#211/#701: PR creation for the no-pipeline case (and for cards
      // that have completed all configured pipeline stages at review time).
      // noop → terminal (legacy behavior: no work / no repo / no agent =
      // nothing to PR). error → markPrCreateFailed (terminal + blocked_reason
      // so the card is visible AND handed to an agent/operator, #5716).
      var prResultNoStages = attemptCreatePrDispatchForReviewPass(cardId, noopVerification);
      prDispatched = (prResultNoStages.status === "dispatched");
      if (prResultNoStages.status === "dispatched") {
        agentdesk.log.info("[review] Card " + cardId + " passed review — create-pr dispatched, awaiting CI/merge");
      } else if (prResultNoStages.status === "error") {
        // #2051 Finding 7 (P2): no_agent_escalated already raised manual
        // intervention; do not bump retry_count via markPrCreateFailed.
        if (prResultNoStages.reason !== "no_agent_escalated") {
          markPrCreateFailed(cardId, prResultNoStages.reason);
        }
      } else {
        agentdesk.kanban.setStatus(cardId, reviewPassTarget, true);
        agentdesk.log.info("[review] Card " + cardId + " passed review → " + reviewPassTarget);
      }
    }

  } else if (verdict === "improve" || verdict === "reject" || verdict === "rework") {
    emitReviewVerdictQualityEvent(cardId, verdict, result, opts, "review_fail");
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
        escalateToManualIntervention(cardId, "리뷰 피드백 없이 2회 이상 연속 " + verdict + " — 유사성 검사 불가, PM 판단 필요", {
          review: true,
          reviewStateSync: { last_verdict: verdict }
        });
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

    if (noopVerification) {
      var noopCardInfo = agentdesk.db.query(
        "SELECT assigned_agent_id, title, github_issue_number FROM kanban_cards WHERE id = ?",
        [cardId]
      );
      var noopAssignedAgent = noopCardInfo.length > 0 ? noopCardInfo[0].assigned_agent_id : null;
      var noopTitle = noopCardInfo.length > 0 ? (noopCardInfo[0].title || cardId) : cardId;
      var noopIssueNum = noopCardInfo.length > 0 ? (noopCardInfo[0].github_issue_number || "?") : "?";
      var noopReason = latestReviewContext.noop_reason || "(noop 사유 없음)";
      var noopReworkPrompt = "[Noop Rework] #" + noopIssueNum + " " + noopTitle +
        "\n\n기존 noop 판단이 리뷰에서 반려되었습니다. 실제 구현이 필요합니다." +
        "\n- 리뷰 verdict: " + verdict +
        "\n- noop 사유: " + summarizeFindingForPrompt(noopReason) +
        "\n- 리뷰 피드백: " + summarizeFindingForPrompt(newNotes || "(없음)") +
        "\n\nGitHub 이슈 본문과 리뷰 피드백을 기준으로 필요한 코드를 구현하세요.";

      if (noopAssignedAgent) {
        try {
          var noopReworkDispatchId = agentdesk.dispatch.create(
            cardId,
            noopAssignedAgent,
            "rework",
            noopReworkPrompt,
            {
              review_mode: "noop_verification",
              noop_reason: noopReason
            }
          );
          agentdesk.log.info("[review] noop_verification " + verdict + " → rework dispatch " + noopReworkDispatchId + " for " + cardId);
          agentdesk.reviewState.sync(cardId, "rework_pending", { last_verdict: verdict });
          agentdesk.kanban.setReviewStatus(cardId, "rework_pending", {exclude_status: terminalState});
          agentdesk.kanban.setStatus(cardId, reviewReworkTarget);
          return;
        } catch (e) {
          agentdesk.log.warn("[review] noop_verification rework dispatch failed for " + cardId + ": " + e + " — falling back to suggestion_pending");
        }
      } else {
        agentdesk.log.warn("[review] noop_verification " + verdict + " on " + cardId + " has no assigned agent — falling back to suggestion_pending");
      }
    }

    if (repeatedFindings && assignedAgent) {
      // Already tried session reset after approach change → escalate to PM
      if (sessionResetRound) {
        agentdesk.log.warn("[review] #420 Session reset already attempted at R" + sessionResetRound +
          ", findings still repeat at R" + currentRound + " → dilemma_pending");
        escalateToManualIntervention(cardId, "세션 리셋 후에도 동일 finding 반복 (R" + sessionResetRound + "→R" + currentRound + ") — PM 판단 필요", {
          review: true,
          reviewStateSync: { last_verdict: verdict }
        });
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
            {
              reset_provider_state: true,
              recreate_tmux: false,
              force_new_session: true
            }
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

if (typeof agentdesk !== "undefined" && agentdesk && typeof agentdesk.registerPolicy === "function") {
  agentdesk.registerPolicy(reviewAutomation);
}

// #701: Expose the create-pr dispatch helper so pipeline stage policies can
// hand cards back into the PR/CI flow after non-skip pipeline stages
// (e2e-test, normal agent) complete. Without this, cards finishing pipeline
// stages reach terminal state with no PR or tracking row, and ci-recovery
// never gets a chance to close the loop.
//
// Returns the structured { status, reason? } object from
// attemptCreatePrDispatchForReviewPass. Callers MUST distinguish
// "dispatched" / "noop" / "error" — see the helper's docstring.
// #743: Rust ops (handoffCreatePr, recordPrCreateFailure, reseedPrTracking)
// are registered onto agentdesk.reviewAutomation before policies load. Merge
// the JS helpers onto the same object rather than overwriting it.
agentdesk.reviewAutomation = agentdesk.reviewAutomation || {};
agentdesk.reviewAutomation.attemptCreatePr = function(cardId) {
  return attemptCreatePrDispatchForReviewPass(cardId, false);
};
agentdesk.reviewAutomation.markPrCreateFailed = function(cardId, reason, stampGen) {
  markPrCreateFailed(cardId, reason, stampGen);
};

if (typeof module !== "undefined" && module.exports) {
  module.exports = {
    policy: reviewAutomation,
    __test: {
      buildNoopReviewContext: buildNoopReviewContext,
      processVerdict: processVerdict,
      setNormalSuggestionPending: setNormalSuggestionPending,
      loadLatestReviewDispatchContext: loadLatestReviewDispatchContext
    }
  };
}
