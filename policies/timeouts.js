/**
 * timeouts.js — ADK Policy: Timeout & Stale Detection
 * priority: 100
 *
 * Hook: onTick (1분 간격 — Rust 서버에서 주기적으로 fire)
 *
 * [A] Requested 타임아웃 (requested_timeout_min, 기본 45분) → retry_count < 10이면 재시도 대기, ≥ 10이면 pending_decision
 * [B] In-Progress 스테일 (in_progress_stale_min, 기본 120분) → blocked
 * [C] 스테일 리뷰 (dispatch 완료인데 verdict 없음) → pending_decision
 * [D] DoD 대기 타임아웃 (15분) → pending_decision
 * [E] 자동-수용 결정 타임아웃 → auto-accept + rework
 * [F] 디스패치 큐 타임아웃 (100분) → 제거
 * [G] 스테일 디스패치 정리 (24시간) → failed
 * [H] Stale dispatched 큐 엔트리 진행
 * [I-0] 미전송 디스패치 알림 복구 (2분)
 * [J] Failed 디스패치 자동 재시도 (30초 쿨다운, ~60초 cadence, 최대 10회 + 즉시 Discord 알림)
 * [I] 턴 데드락 감지 + 자동 복구 (15분 주기, 최대 3회 연장 후 강제 중단 + 재디스패치)
 * [K] 고아 디스패치 복구 (5분) — in_progress 카드 + pending 디스패치 + 활성 세션 없음 → review 전이
 * [L] Inflight 장시간 턴 감지 (#130) — heartbeat와 독립, started_at 기반 15/30/60/120분 단계별 알림
 * [M] Workspace branch 보호 (5분) — 메인 repo가 wt/* 브랜치로 이탈하면 자동 복구 (#181)
 * [N] Orphan review 자동 복구 (1분) — review 상태인데 활성 review 계열 dispatch가 없으면 pending_decision
 * [O] Idle session TTL cleanup (5분) — idle 60분 tmux-backed 세션 force-kill + notify
 */

// Send notification via notify bot (system alerts, not agent communication)
function sendNotifyAlert(channelTarget, message) {
  if (!channelTarget) return;
  agentdesk.message.queue(channelTarget, message, "notify", "system");
}

// Get PMD channel for alerts
function getPMDChannel() {
  var ch = agentdesk.config.get("kanban_manager_channel_id");
  if (!ch) {
    agentdesk.log.warn("[notify] No kanban_manager_channel_id configured, skipping");
    return null;
  }
  return "channel:" + ch;
}

// Send deadlock alert via announce bot to deadlock-manager channel
function sendDeadlockAlert(message) {
  var ch = agentdesk.config.get("deadlock_manager_channel_id");
  if (!ch) {
    // Fallback to PMD channel via announce bot (actionable alert, not info-only)
    var pmd = getPMDChannel();
    if (pmd) agentdesk.message.queue(pmd, message, "announce", "system");
    return;
  }
  agentdesk.message.queue("channel:" + ch, message, "announce", "system");
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

// #231: PM Decision notification dedup — durable kv_meta buffer.
// Reasons are persisted to kv_meta (survives restart) and flushed
// in onTick (legacy, 5min) AFTER all tiered handlers to combine
// cross-tier reasons into one notification per card.
var PM_DECISION_COOLDOWN_SEC = 300;  // 5-min cross-tick cooldown
var PM_PENDING_TTL_SEC = 600;  // 10-min TTL for pending entries (auto-cleanup)

function _queuePMDecision(cardId, title, reason) {
  var pendingKey = "pm_pending:" + cardId;
  var existing = agentdesk.db.query("SELECT value FROM kv_meta WHERE key = ?", [pendingKey]);
  var entry;
  if (existing.length > 0) {
    try { entry = JSON.parse(existing[0].value); } catch(e) { entry = null; }
  }
  if (!entry) {
    entry = { title: title, reasons: [] };
  }
  // Deduplicate identical reasons
  if (entry.reasons.indexOf(reason) === -1) {
    entry.reasons.push(reason);
  }
  agentdesk.db.execute(
    "INSERT OR REPLACE INTO kv_meta (key, value, expires_at) VALUES (?, ?, datetime('now', '+' || ? || ' seconds'))",
    [pendingKey, JSON.stringify(entry), String(PM_PENDING_TTL_SEC)]
  );
}

function _flushPMDecisions() {
  var pmdCh = getPMDChannel();
  var rows = agentdesk.db.query("SELECT key, value FROM kv_meta WHERE key LIKE 'pm_pending:%'");
  for (var i = 0; i < rows.length; i++) {
    var cardId = rows[i].key.substring("pm_pending:".length);
    var entry;
    try { entry = JSON.parse(rows[i].value); } catch(e) { continue; }
    // Delete the pending entry first (consumed regardless of cooldown)
    agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [rows[i].key]);
    // Cross-tick cooldown: skip send if notified recently
    var cooldownKey = "pm_decision_sent:" + cardId;
    var cooldownRow = agentdesk.db.query("SELECT value FROM kv_meta WHERE key = ?", [cooldownKey]);
    if (cooldownRow.length > 0) {
      var sentAt = parseInt(cooldownRow[0].value, 10) || 0;
      var now = Math.floor(Date.now() / 1000);
      if (now - sentAt < PM_DECISION_COOLDOWN_SEC) {
        agentdesk.log.info("[PM dedup] Skipped notification for card " + cardId +
          " (" + entry.reasons.length + " reasons, cooldown " + (now - sentAt) + "s)");
        continue;
      }
    }
    // Send combined notification with all accumulated reasons
    if (!pmdCh) continue;
    var msg = "⚠️ [PM 결정 요청] " + entry.title + "\n카드가 pending_decision 상태입니다. PMD가 다음 조치를 결정해주세요.\n사유: " + entry.reasons.join("; ");
    agentdesk.message.queue(pmdCh, msg, "announce", "system");
    // Set cooldown with TTL
    agentdesk.db.execute(
      "INSERT OR REPLACE INTO kv_meta (key, value, expires_at) VALUES (?, ?, datetime('now', '+' || ? || ' seconds'))",
      [cooldownKey, String(Math.floor(Date.now() / 1000)), String(PM_DECISION_COOLDOWN_SEC)]
    );
  }
}

var timeouts = {
  name: "timeouts",
  priority: 100,

  // onTick: assigned after object literal (line ~1282) to flush PM decisions (#231)

  // ── Section methods (extracted from onTick for tiered execution) ──

  _section_R: function() {
    // ─── [R] Reconciliation: DB fallback dispatches that need hook chain ──
    // These dispatches were completed/failed via direct DB UPDATE (API retry exhausted).
    // We re-emit the OnDispatchCompleted payload so the full hook chain runs
    // (PM gate, DoD check, XP, review entry — same as normal complete_dispatch path).
    var reconcileKeys = agentdesk.db.query(
      "SELECT key, value FROM kv_meta WHERE key LIKE 'reconcile_dispatch:%'"
    );
    for (var r = 0; r < reconcileKeys.length; r++) {
      var dispatchId = reconcileKeys[r].value;
      agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [reconcileKeys[r].key]);
      agentdesk.log.info("[reconcile] Processing fallback dispatch " + dispatchId);
      // The dispatch is already completed/failed in DB.
      // Fire the same event that kanban-rules.js and review-automation.js listen to.
      // This is handled by the Rust engine — we can't re-emit hooks from JS.
      // Instead, call the same logic that onDispatchCompleted would:
      // 1. Read dispatch info
      var dispInfo = agentdesk.db.query(
        "SELECT id, kanban_card_id, to_agent_id, dispatch_type, chain_depth, status, result, context FROM task_dispatches WHERE id = ?",
        [dispatchId]
      );
      if (dispInfo.length === 0) continue;
      var di = dispInfo[0];
      if (!di.kanban_card_id) continue;
      if (di.status === "failed") {
        agentdesk.log.info("[reconcile] Dispatch " + dispatchId + " failed — no action needed");
        continue;
      }
      // 2. For completed dispatches, replay kanban-rules onDispatchCompleted logic
      var cards = agentdesk.db.query(
        "SELECT id, status, priority, assigned_agent_id, deferred_dod_json FROM kanban_cards WHERE id = ?",
        [di.kanban_card_id]
      );
      if (cards.length === 0) continue;
      var card = cards[0];
      var rCfg = agentdesk.pipeline.resolveForCard(card.id);
      var rInitial = agentdesk.pipeline.kickoffState(rCfg);
      var rInProgress = agentdesk.pipeline.nextGatedTarget(rInitial, rCfg);
      var rReview = agentdesk.pipeline.nextGatedTarget(rInProgress, rCfg);
      var rForce = agentdesk.pipeline.forceOnlyTargets(rInProgress, rCfg);
      var rPending = rForce[0];
      if (agentdesk.pipeline.isTerminal(card.status, rCfg)) continue;
      if (di.dispatch_type === "review" || di.dispatch_type === "review-decision") continue;
      if (di.dispatch_type === "rework") {
        agentdesk.kanban.setStatus(card.id, rReview);
        agentdesk.log.info("[reconcile] " + card.id + " rework done → " + rReview);
        continue;
      }
      // Implementation: run PM gate same as kanban-rules.js onDispatchCompleted
      var xpMap = { "low": 5, "medium": 10, "high": 18, "urgent": 30 };
      var xp = xpMap[card.priority] || 10;
      xp += Math.min(di.chain_depth || 0, 3) * 2;
      if (di.to_agent_id) {
        agentdesk.db.execute("UPDATE agents SET xp = xp + ? WHERE id = ?", [xp, di.to_agent_id]);
      }
      // Check skip_gate from dispatch context
      var dispatchContext = {};
      try { dispatchContext = JSON.parse(di.context || "{}"); } catch(e) {}
      var pmGateEnabled = agentdesk.config.get("pm_decision_gate_enabled");
      if (dispatchContext.skip_gate) {
        agentdesk.log.info("[reconcile] Skipped PM gate for card " + card.id + " (skip_gate flag)");
      } else if (pmGateEnabled !== false && pmGateEnabled !== "false") {
        var reasons = [];
        // Check 1: DoD completion
        // Format: { items: ["task1", "task2"], verified: ["task1"] }
        if (card.deferred_dod_json) {
          try {
            var dod = JSON.parse(card.deferred_dod_json);
            var items = dod.items || [];
            var verified = dod.verified || [];
            if (items.length > 0) {
              var unverified = 0;
              for (var di2 = 0; di2 < items.length; di2++) {
                if (verified.indexOf(items[di2]) === -1) unverified++;
              }
              if (unverified > 0) reasons.push("DoD 미완료: " + (items.length - unverified) + "/" + items.length);
            }
          } catch (e) {}
        }
        // Minimum work duration heuristic intentionally removed to keep PM
        // escalation aligned with objective failure states only. Replay logic
        // must match kanban-rules.js and avoid false positives from unified
        // thread / turn-bridge completions.
        if (reasons.length > 0) {
          var dodOnly = reasons.length === 1 && reasons[0].indexOf("DoD 미완료") === 0;
          if (dodOnly) {
            agentdesk.kanban.setStatus(card.id, rReview);
            agentdesk.kanban.setReviewStatus(card.id, "awaiting_dod", {awaiting_dod_at: "now"});
            // #117: sync canonical review state
            agentdesk.reviewState.sync(card.id, "awaiting_dod");
            agentdesk.log.warn("[reconcile] Card " + card.id + " → " + rReview + "(awaiting_dod): " + reasons[0]);
            continue;
          }
          agentdesk.kanban.setStatus(card.id, rPending);
          agentdesk.kanban.setReviewStatus(card.id, null, {suggestion_pending_at: null});
          // #117: sync canonical review state
          agentdesk.reviewState.sync(card.id, "idle");
          agentdesk.log.warn("[reconcile] Card " + card.id + " → " + rPending + ": " + reasons.join("; "));
          // #231: Queue deduped PM notification (flushed at tick end)
          var cardTitle2 = agentdesk.db.query("SELECT title FROM kanban_cards WHERE id = ?", [card.id]);
          var t2 = cardTitle2.length > 0 ? cardTitle2[0].title : card.id;
          for (var ri = 0; ri < reasons.length; ri++) {
            _queuePMDecision(card.id, t2, reasons[ri]);
          }
          continue;
        }
      }
      agentdesk.kanban.setStatus(card.id, rReview);
      agentdesk.log.info("[reconcile] " + card.id + " implementation done → " + rReview + " (via DB fallback)");
    }
  },

  _section_A: function() {
    // ─── [A] Requested 타임아웃 ─────────────────────
    // retry_count < 10이면 pending_decision 대신 failed만 마크 → [J]가 30초 후 재시도
    var aCfg = agentdesk.pipeline.getConfig();
    var aInitial = agentdesk.pipeline.kickoffState(aCfg);
    var aInProgress = agentdesk.pipeline.nextGatedTarget(aInitial, aCfg);
    var aForce = agentdesk.pipeline.forceOnlyTargets(aInitial, aCfg);
    var aPending = aForce[0];
    var requestedInterval = getTimeoutInterval("requested_timeout_min", 45);
    var staleRequested = agentdesk.db.query(
      "SELECT kc.id, kc.assigned_agent_id, kc.latest_dispatch_id, " +
      "COALESCE(td.retry_count, 0) as retry_count, td.dispatch_type " +
      "FROM kanban_cards kc " +
      "LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id " +
      "WHERE kc.status = ? AND kc.requested_at IS NOT NULL AND kc.requested_at < datetime('now', '" + requestedInterval + "')",
      [aInitial]
    );
    for (var i = 0; i < staleRequested.length; i++) {
      var rc = staleRequested[i];
      // #255: Skip cards without a dispatch — they are in preflight state,
      // waiting for auto-queue or tick to create a dispatch.
      if (!rc.latest_dispatch_id) {
        agentdesk.log.info("[timeout] Card " + rc.id + " in " + aInitial + " without dispatch — preflight, skipping timeout");
        continue;
      }
      // #256: Skip cards with consultation dispatch — consultation has its own
      // lifecycle via onDispatchCompleted; let it resolve naturally.
      if (rc.dispatch_type === "consultation") {
        agentdesk.log.info("[timeout] Card " + rc.id + " in " + aInitial + " with consultation dispatch — skipping timeout");
        continue;
      }
      // Dispatch를 failed로 — skip state changes if dispatch was already terminal
      if (rc.latest_dispatch_id) {
        var failResult = agentdesk.dispatch.markFailed(rc.latest_dispatch_id, "Timed out waiting for agent");
        if (failResult.rows_affected === 0) {
          agentdesk.log.info("[timeout] Card " + rc.id + " dispatch already terminal, skipping");
          continue;
        }
      }

      if (rc.retry_count < MAX_DISPATCH_RETRIES) {
        // 재시도 여유 있음 → card 상태 유지 (requested_at 갱신하여 [A] 재트리거 방지)
        // [J] 섹션에서 30초 후 자동 재시도
        agentdesk.db.execute(
          "UPDATE kanban_cards SET requested_at = datetime('now'), updated_at = datetime('now') WHERE id = ?",
          [rc.id]
        );
        agentdesk.log.warn("[timeout] Card " + rc.id + " requested timeout — retry " +
          rc.retry_count + "/" + MAX_DISPATCH_RETRIES + ", will auto-retry in 30s");
      } else {
        // 10회 재시도 소진 → aPending + PMD 알림
        agentdesk.kanban.setStatus(rc.id, aPending);
        agentdesk.db.execute(
          "UPDATE kanban_cards SET blocked_reason = 'Timed out waiting for agent (" + MAX_DISPATCH_RETRIES + " retries exhausted)' WHERE id = ?",
          [rc.id]
        );
        agentdesk.log.warn("[timeout] Card " + rc.id + " " + aInitial + " timeout → " + aPending + " (" + MAX_DISPATCH_RETRIES + " retries exhausted)");
        // #231: Queue deduped PM notification — PM must decide next action
        var cardInfo = agentdesk.db.query(
          "SELECT title FROM kanban_cards WHERE id = ?",
          [rc.id]
        );
        var cardTitle = (cardInfo.length > 0) ? cardInfo[0].title : rc.id;
        _queuePMDecision(rc.id, cardTitle, MAX_DISPATCH_RETRIES + " retries exhausted");
      }
    }
  },

  _section_B: function() {
    // ─── [B] In-Progress 스테일 ────────────────────
    var bCfg = agentdesk.pipeline.getConfig();
    var bInitial = agentdesk.pipeline.kickoffState(bCfg);
    var bInProgress = agentdesk.pipeline.nextGatedTarget(bInitial, bCfg);
    var bForce = agentdesk.pipeline.forceOnlyTargets(bInProgress, bCfg);
    var bBlocked = bForce.length > 1 ? bForce[1] : bForce[0];
    var inProgressInterval = getTimeoutInterval("in_progress_stale_min", 120);
    var staleInProgress = agentdesk.db.query(
      "SELECT kc.id FROM kanban_cards kc " +
      "LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id " +
      "WHERE kc.status = ? AND " + latestCardActivityExpr("kc", "td") + " < datetime('now', '" + inProgressInterval + "')",
      [bInProgress]
    );
    for (var j = 0; j < staleInProgress.length; j++) {
      agentdesk.kanban.setStatus(staleInProgress[j].id, bBlocked);
      var staleMin = parseInt(agentdesk.config.get("in_progress_stale_min"), 10) || 120;
      agentdesk.db.execute(
        "UPDATE kanban_cards SET blocked_reason = 'Stalled: no activity for " + staleMin + "+ min' WHERE id = ?",
        [staleInProgress[j].id]
      );
      agentdesk.log.warn("[timeout] Card " + staleInProgress[j].id + " " + bInProgress + " stale → " + bBlocked);
      // #231: Queue deduped PM notification — PM must unblock
      var stalledInfo = agentdesk.db.query(
        "SELECT title FROM kanban_cards WHERE id = ?",
        [staleInProgress[j].id]
      );
      var stalledTitle = (stalledInfo.length > 0) ? stalledInfo[0].title : staleInProgress[j].id;
      _queuePMDecision(staleInProgress[j].id, stalledTitle, staleMin + "분+ 활동 없음 → blocked");
    }
  },

  _section_C: function() {
    // ─── [C] 스테일 리뷰 (dispatch 완료인데 verdict 없음) ──
    var cCfg = agentdesk.pipeline.getConfig();
    var cInitial = agentdesk.pipeline.kickoffState(cCfg);
    var cInProgress = agentdesk.pipeline.nextGatedTarget(cInitial, cCfg);
    var cReview = agentdesk.pipeline.nextGatedTarget(cInProgress, cCfg);
    var cForce = agentdesk.pipeline.forceOnlyTargets(cInProgress, cCfg);
    var cPending = cForce[0];
    var staleReviews = agentdesk.db.query(
      "SELECT kc.id as card_id " +
      "FROM kanban_cards kc " +
      "JOIN task_dispatches td ON td.kanban_card_id = kc.id " +
      "WHERE kc.status = ? AND kc.review_status = 'reviewing' " +
      "AND td.dispatch_type = 'review' AND td.status IN ('completed', 'failed') " +
      "AND kc.review_entered_at IS NOT NULL AND kc.review_entered_at < datetime('now', '-30 minutes') " +
      "AND NOT EXISTS (SELECT 1 FROM task_dispatches td2 WHERE td2.kanban_card_id = kc.id " +
      "AND td2.dispatch_type IN ('review', 'review-decision') AND td2.status = 'pending')",
      [cReview]
    );
    for (var k = 0; k < staleReviews.length; k++) {
      agentdesk.kanban.setStatus(staleReviews[k].card_id, cPending);
      agentdesk.kanban.setReviewStatus(staleReviews[k].card_id, null, {suggestion_pending_at: null});
      // #117: sync canonical review state
      agentdesk.reviewState.sync(staleReviews[k].card_id, "idle");
      agentdesk.log.warn("[timeout] Stale review → pending_decision: card " + staleReviews[k].card_id);
      // #231: Queue deduped PM notification — PM must decide
      var staleRevInfo = agentdesk.db.query("SELECT title FROM kanban_cards WHERE id = ?", [staleReviews[k].card_id]);
      var staleRevTitle = (staleRevInfo.length > 0) ? staleRevInfo[0].title : staleReviews[k].card_id;
      _queuePMDecision(staleReviews[k].card_id, staleRevTitle, "stale review — dispatch 완료 30분+ verdict 없음 → pending_decision");
    }
  },

  _section_D: function() {
    // ─── [D] DoD 대기 타임아웃 (15분) ──────────────────────
    var dCfg = agentdesk.pipeline.getConfig();
    var dInitial = agentdesk.pipeline.kickoffState(dCfg);
    var dInProgress = agentdesk.pipeline.nextGatedTarget(dInitial, dCfg);
    var dReview = agentdesk.pipeline.nextGatedTarget(dInProgress, dCfg);
    var dForce = agentdesk.pipeline.forceOnlyTargets(dInProgress, dCfg);
    var dPending = dForce[0];
    var stuckDod = agentdesk.db.query(
      "SELECT id FROM kanban_cards " +
      "WHERE status = ? AND review_status = 'awaiting_dod' " +
      "AND awaiting_dod_at IS NOT NULL AND awaiting_dod_at < datetime('now', '-15 minutes')",
      [dReview]
    );
    for (var d = 0; d < stuckDod.length; d++) {
      agentdesk.kanban.setStatus(stuckDod[d].id, dPending);
      agentdesk.kanban.setReviewStatus(stuckDod[d].id, null, {suggestion_pending_at: null});
      // #117: sync canonical review state
      agentdesk.reviewState.sync(stuckDod[d].id, "idle");
      agentdesk.log.warn("[timeout] DoD await timeout → pending_decision: card " + stuckDod[d].id);
      // #231: Queue deduped PM notification
      var dodInfo = agentdesk.db.query("SELECT title FROM kanban_cards WHERE id = ?", [stuckDod[d].id]);
      var dodTitle = (dodInfo.length > 0) ? dodInfo[0].title : stuckDod[d].id;
      _queuePMDecision(stuckDod[d].id, dodTitle, "DoD 대기 15분 초과 → pending_decision");
    }
  },

  _section_E: function() {
    // ─── [E] 자동-수용 결정 타임아웃 (suggestion_pending 15분) ──
    // Auto-accept: same effect as manual review-decision accept
    // (status → rework target, review_status → rework_pending, create rework dispatch)
    var eCfg = agentdesk.pipeline.getConfig();
    var eInitial = agentdesk.pipeline.kickoffState(eCfg);
    var eInProgress = agentdesk.pipeline.nextGatedTarget(eInitial, eCfg);
    var eReview = agentdesk.pipeline.nextGatedTarget(eInProgress, eCfg);
    var eReworkTarget = agentdesk.pipeline.nextGatedTargetWithGate(eReview, "review_rework", eCfg) || eInProgress;
    var eForce = agentdesk.pipeline.forceOnlyTargets(eInProgress, eCfg);
    var ePending = eForce[0];
    var staleSuggestions = agentdesk.db.query(
      "SELECT id, assigned_agent_id, title FROM kanban_cards " +
      "WHERE review_status = 'suggestion_pending' " +
      "AND suggestion_pending_at IS NOT NULL AND suggestion_pending_at < datetime('now', '-15 minutes')"
    );
    for (var s = 0; s < staleSuggestions.length; s++) {
      var sc = staleSuggestions[s];
      if (sc.assigned_agent_id) {
        // Try dispatch creation FIRST — only transition on success
        try {
          agentdesk.dispatch.create(
            sc.id,
            sc.assigned_agent_id,
            "rework",
            "[Rework] " + (sc.title || sc.id)
          );
          // Dispatch succeeded — now transition to rework target + rework_pending
          agentdesk.kanban.setStatus(sc.id, eReworkTarget);
          agentdesk.kanban.setReviewStatus(sc.id, "rework_pending", {suggestion_pending_at: null});
          // #119: Record tuning outcome (auto-accept = true_positive) BEFORE transition clears last_verdict
          var reviewState = agentdesk.db.query(
            "SELECT review_round, last_verdict FROM card_review_state WHERE card_id = ?",
            [sc.id]
          );
          if (reviewState.length > 0) {
            var rs = reviewState[0];
            // Get finding categories from last completed review dispatch
            var lastReview = agentdesk.db.query(
              "SELECT result FROM task_dispatches WHERE kanban_card_id = ? AND dispatch_type = 'review' AND status = 'completed' ORDER BY rowid DESC LIMIT 1",
              [sc.id]
            );
            var findingCats = null;
            if (lastReview.length > 0 && lastReview[0].result) {
              try {
                var parsed = JSON.parse(lastReview[0].result);
                if (parsed.items) {
                  findingCats = JSON.stringify(parsed.items.map(function(it) { return it.category || "unknown"; }));
                }
              } catch(e) {}
            }
            agentdesk.db.execute(
              "INSERT INTO review_tuning_outcomes (card_id, dispatch_id, review_round, verdict, decision, outcome, finding_categories) " +
              "VALUES (?, NULL, ?, ?, 'auto_accept', 'true_positive', ?)",
              [sc.id, rs.review_round || null, rs.last_verdict || "unknown", findingCats]
            );
            agentdesk.log.info("[review-tuning] #119 recorded true_positive (auto-accept): card=" + sc.id);
            // #119: Trigger re-aggregation — other outcome paths (Rust) call
            // spawn_aggregate_if_needed directly; from JS we hit the HTTP API.
            try {
              var aggPort = agentdesk.config.get("server_port");
              if (aggPort) {
                agentdesk.http.post("http://127.0.0.1:" + aggPort + "/api/review-tuning/aggregate", {});
              }
            } catch (aggErr) {
              agentdesk.log.warn("[review-tuning] aggregate trigger failed (non-fatal): " + aggErr);
            }
          }
          // #117: sync canonical review state
          agentdesk.reviewState.sync(sc.id, "rework_pending", { last_decision: "auto_accept" });
          agentdesk.log.warn("[timeout] Auto-accepted suggestions for card " + sc.id + " — rework dispatch created");
        } catch (e) {
          // Dispatch failed — route to pending state instead
          agentdesk.kanban.setStatus(sc.id, ePending);
          agentdesk.db.execute(
            "UPDATE kanban_cards SET blocked_reason = 'Auto-accept rework dispatch failed: " + e + "' WHERE id = ?",
            [sc.id]
          );
          agentdesk.log.error("[timeout] Failed to create rework dispatch for " + sc.id + ": " + e + " → pending_decision");
        }
      } else {
        agentdesk.log.warn("[timeout] Auto-accepted card " + sc.id + " but no agent assigned — no rework dispatch");
      }
    }
  },

  _section_F: function() {
    // ─── [F] 디스패치 큐 타임아웃 (100분) ──────────────────
    agentdesk.db.execute(
      "DELETE FROM dispatch_queue WHERE queued_at < datetime('now', '-100 minutes')"
    );
  },

  _section_G: function() {
    // ─── [G] 스테일 디스패치 정리 (24시간) ──────────────────
    var gCfg = agentdesk.pipeline.getConfig();
    var gInitial = agentdesk.pipeline.kickoffState(gCfg);
    var gInProgress = agentdesk.pipeline.nextGatedTarget(gInitial, gCfg);
    var gForce = agentdesk.pipeline.forceOnlyTargets(gInProgress, gCfg);
    var gPending = gForce[0];
    var staleDispatches = agentdesk.db.query(
      "SELECT id, kanban_card_id FROM task_dispatches WHERE status IN ('pending','dispatched') AND created_at < datetime('now', '-24 hours')"
    );
    for (var sd = 0; sd < staleDispatches.length; sd++) {
      var sfResult = agentdesk.dispatch.markFailed(staleDispatches[sd].id, "Stale dispatch auto-failed after 24h");
      if (sfResult.rows_affected === 0) {
        agentdesk.log.info("[timeout] Dispatch " + staleDispatches[sd].id + " already terminal, skipping");
        continue;
      }
      if (staleDispatches[sd].kanban_card_id) {
        var card = agentdesk.kanban.getCard(staleDispatches[sd].kanban_card_id);
        if (card && !agentdesk.pipeline.isTerminal(card.status, gCfg)) {
          agentdesk.kanban.setStatus(staleDispatches[sd].kanban_card_id, gPending);
          agentdesk.db.execute(
            "UPDATE kanban_cards SET blocked_reason = 'Stale dispatch auto-failed after 24h' WHERE id = ?",
            [staleDispatches[sd].kanban_card_id]
          );
        }
      }
      agentdesk.log.warn("[timeout] Dispatch " + staleDispatches[sd].id + " stale 24h → failed");
    }
  },

  _section_H: function() {
    // ─── [H] Stale dispatched 큐 엔트리 진행 ───────────────
    var hCfg = agentdesk.pipeline.getConfig();
    var hInitial = agentdesk.pipeline.kickoffState(hCfg);
    var hInProgress = agentdesk.pipeline.nextGatedTarget(hInitial, hCfg);
    var staleQueueEntries = agentdesk.db.query(
      "SELECT dq.id FROM dispatch_queue dq " +
      "JOIN kanban_cards kc ON kc.id = dq.kanban_card_id " +
      "WHERE dq.status = 'dispatched' AND kc.status NOT IN (?, ?)",
      [hInitial, hInProgress]
    );
    for (var se = 0; se < staleQueueEntries.length; se++) {
      agentdesk.db.execute(
        "DELETE FROM dispatch_queue WHERE id = ?",
        [staleQueueEntries[se].id]
      );
    }
  },

  _section_I0: function() {
    // ─── [I-0] 미전송 디스패치 알림 복구 ──────────────────────
    // pending dispatch가 2분 이상 됐는데 알림이 안 갔을 수 있음 → 재전송
    var unnotifiedDispatches = agentdesk.db.query(
      "SELECT td.id, td.dispatch_type, td.to_agent_id, kc.title, kc.github_issue_url, kc.github_issue_number, td.kanban_card_id " +
      "FROM task_dispatches td " +
      "JOIN kanban_cards kc ON td.kanban_card_id = kc.id " +
      "WHERE td.status = 'pending' " +
      "AND td.created_at < datetime('now', '-2 minutes') " +
      "AND NOT EXISTS (SELECT 1 FROM kv_meta WHERE key = 'dispatch_notified:' || td.id) " +
      "AND NOT EXISTS (SELECT 1 FROM kv_meta WHERE key = 'dispatch_reserving:' || td.id) " +
      "AND NOT EXISTS (SELECT 1 FROM dispatch_outbox WHERE dispatch_id = td.id AND status IN ('pending', 'processing', 'failed'))"
    );
    for (var un = 0; un < unnotifiedDispatches.length; un++) {
      var ud = unnotifiedDispatches[un];

      // Re-enqueue into dispatch_outbox so the Rust outbox worker handles delivery
      // with proper two-phase guard and retry/backoff (#209).
      // Do NOT send directly via message.queue — that bypasses the delivery guarantee.
      agentdesk.db.execute(
        "INSERT INTO dispatch_outbox (dispatch_id, action, agent_id, card_id, title, status) " +
        "VALUES (?1, 'notify', ?2, ?3, ?4, 'pending')",
        [ud.id, ud.to_agent_id, ud.kanban_card_id || "", ud.title]
      );
      agentdesk.log.info("[notify-recovery] Dispatch " + ud.id + " re-enqueued to dispatch_outbox");
    }
  },

  _section_J: function() {
    // ─── [J] Failed 디스패치 자동 재시도 (30초 쿨다운, 최대 10회) ──
    // failed 상태의 디스패치 중 retry_count < 10이고 30초+ 경과한 것을 재시도.
    // 실제 cadence는 onTick 60초 간격이므로 ~60-90초.
    // 10분 윈도우 제거 — latest_dispatch_id 체크로 stale 방지 충분.
    var jCfg = agentdesk.pipeline.getConfig();
    var jInitial = agentdesk.pipeline.kickoffState(jCfg);
    var jInProgress = agentdesk.pipeline.nextGatedTarget(jInitial, jCfg);
    var jBlocked = agentdesk.pipeline.forceOnlyTargets(jInProgress, jCfg)[0];
    var failedForRetry = agentdesk.db.query(
      "SELECT td.id, td.kanban_card_id, td.to_agent_id, td.dispatch_type, td.title, " +
      "COALESCE(td.retry_count, 0) as retry_count, kc.github_issue_url, kc.github_issue_number " +
      "FROM task_dispatches td " +
      "JOIN kanban_cards kc ON kc.id = td.kanban_card_id " +
      "WHERE td.status = 'failed' " +
      "AND COALESCE(td.retry_count, 0) < " + MAX_DISPATCH_RETRIES + " " +
      "AND td.updated_at < datetime('now', '-30 seconds') " +
      "AND kc.latest_dispatch_id = td.id " +
      "AND kc.status IN (?, ?)",
      [jInitial, jInProgress]
    );
    for (var jr = 0; jr < failedForRetry.length; jr++) {
      var fd = failedForRetry[jr];
      var newRetryCount = fd.retry_count + 1;
      try {
        var newDispatchId = agentdesk.dispatch.create(
          fd.kanban_card_id,
          fd.to_agent_id,
          fd.dispatch_type || "implementation",
          fd.title
        );
        // 새 디스패치에 retry_count 기록
        agentdesk.dispatch.setRetryCount(newDispatchId, newRetryCount);
        agentdesk.log.info("[retry] Auto-retry dispatch for card " + fd.kanban_card_id +
          " — attempt " + newRetryCount + "/" + MAX_DISPATCH_RETRIES +
          " (old: " + fd.id + " → new: " + newDispatchId + ")");

        // Discord notification is handled by the dispatch outbox system (#209).
        // agentdesk.dispatch.create() enqueues an outbox entry via queue_dispatch_notify,
        // and the outbox worker delivers with two-phase guard (no duplicate risk).
      } catch (e) {
        agentdesk.log.error("[retry] Failed to create retry dispatch for card " +
          fd.kanban_card_id + ": " + e);
        // Don't block the card on transient retry failure — leave status as-is
        // so the next tick can retry. Only log the error.
      }
    }
  },

  // #219: Check if a tmux session has at least one live (non-dead) pane.
  // Mirrors Rust's has_live_pane() — uses #{pane_dead} format instead of
  // has-session (which returns true for zombie sessions with dead panes).
  _tmuxHasLivePane: function(tmuxName) {
    try {
      // "=" prefix prevents tmux prefix-matching (exact_target convention)
      var out = agentdesk.exec("tmux", ["list-panes", "-t", "=" + tmuxName, "-F", "#{pane_dead}"]);
      // Success: lines of "0" (alive) or "1" (dead). Any "0" = live pane exists.
      // Failure: "ERROR: ..." (session gone)
      return typeof out === "string" && out.indexOf("ERROR") === -1 && out.indexOf("0") !== -1;
    } catch(e) {
      return false;
    }
  },

  _section_I: function() {
    // ─── [I] 턴 데드락 감지 + 자동 복구 (15분 주기) ─────────
    // 판별: sessions.last_heartbeat 기반 (연속 스톨만 카운트)
    // 연장: 15분 단위로 최대 MAX_EXTENSIONS회 (연속 스톨만 카운트)
    // 확정: 연장 상한 초과 시 agentdesk.session.kill → 강제 중단 + 재디스패치
    var DEADLOCK_MINUTES = 15;
    var MAX_EXTENSIONS = 3;
    var iCfg = agentdesk.pipeline.getConfig();
    var iInitial = agentdesk.pipeline.kickoffState(iCfg);
    var iInProgress = agentdesk.pipeline.nextGatedTarget(iInitial, iCfg);
    var iForce = agentdesk.pipeline.forceOnlyTargets(iInProgress, iCfg);
    var iPending = iForce[0];

    // 먼저: heartbeat가 신선한 working 세션의 카운터를 리셋 (비연속 스톨 누적 방지)
    var freshSessions = agentdesk.db.query(
      "SELECT session_key FROM sessions WHERE status = 'working' " +
      "AND last_heartbeat >= datetime('now', '-" + DEADLOCK_MINUTES + " minutes')"
    );
    for (var fs = 0; fs < freshSessions.length; fs++) {
      var freshKey = "deadlock_check:" + freshSessions[fs].session_key;
      agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [freshKey]);
    }

    // Fix stale working sessions: if status=working but no inflight file exists,
    // the turn has ended but DB wasn't updated. Fix to idle.
    // #219: Increased grace period from 3min to 10min — agents running long tool
    // calls (cargo build, subagents) may not send heartbeats for several minutes.
    var staleWorkingSessions = agentdesk.db.query(
      "SELECT session_key FROM sessions WHERE status = 'working' " +
      "AND last_heartbeat < datetime('now', '-10 minutes')"
    );
    for (var sw = 0; sw < staleWorkingSessions.length; sw++) {
      var swKey = staleWorkingSessions[sw].session_key;
      var tmuxName = (swKey || "").split(":").pop();
      // #219: Check if tmux session has a live pane (not just session existence).
      // has-session returns true for zombie sessions with dead panes;
      // list-panes #{pane_dead} distinguishes live vs dead workers.
      var tmuxAlive = timeouts._tmuxHasLivePane(tmuxName);
      if (!tmuxAlive) {
        // #219: Fail any pending dispatch before transitioning to idle.
        // Without this, the dispatch stays "pending" as an orphan and gets
        // re-delivered or auto-completed, causing the failure loop.
        try {
          var swSessInfo = agentdesk.db.query(
            "SELECT active_dispatch_id FROM sessions WHERE session_key = ?", [swKey]
          );
          if (swSessInfo.length > 0 && swSessInfo[0].active_dispatch_id) {
            var swDispId = swSessInfo[0].active_dispatch_id;
            var swDispStatus = agentdesk.db.query(
              "SELECT status FROM task_dispatches WHERE id = ?", [swDispId]
            );
            if (swDispStatus.length > 0 && (swDispStatus[0].status === "pending" || swDispStatus[0].status === "dispatched")) {
              agentdesk.dispatch.markFailed(swDispId, "Stale working session recovery — no active tmux session after 10min");
              agentdesk.log.warn("[deadlock] Failed stale dispatch " + swDispId + " for session " + swKey);
            }
          }
        } catch(dispErr) {
          agentdesk.log.warn("[deadlock] Failed to mark dispatch for " + swKey + ": " + dispErr);
        }
        agentdesk.db.execute(
          "UPDATE sessions SET status = 'idle', active_dispatch_id = NULL WHERE session_key = ? AND status = 'working'",
          [swKey]
        );
        agentdesk.log.info("[deadlock] Fixed stale working session → idle: " + swKey);
      }
    }

    // 데드락 의심 세션: sessions.last_heartbeat 기반 판별
    var staleSessions = agentdesk.db.query(
      "SELECT session_key, agent_id, active_dispatch_id, last_heartbeat " +
      "FROM sessions WHERE status = 'working' " +
      "AND last_heartbeat < datetime('now', '-" + DEADLOCK_MINUTES + " minutes')"
    );
    for (var dl = 0; dl < staleSessions.length; dl++) {
      var sess = staleSessions[dl];
      var deadlockKey = "deadlock_check:" + sess.session_key;

      // #219: If tmux session has a live pane, the agent is actively working
      // despite stale heartbeat (long tool calls, subagents). Reset counter
      // and skip — heartbeat staleness alone is not sufficient for deadlock.
      var dlTmuxName = (sess.session_key || "").split(":").pop();
      if (timeouts._tmuxHasLivePane(dlTmuxName)) {
        agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [deadlockKey]);
        continue;
      }

      // Check extension count + last check timestamp
      var extRecord = agentdesk.db.query(
        "SELECT value FROM kv_meta WHERE key = ?", [deadlockKey]
      );
      var extensions = 0;
      var lastCheckAt = 0;
      if (extRecord.length > 0) {
        try {
          var parsed = JSON.parse(extRecord[0].value);
          extensions = parsed.count || 0;
          lastCheckAt = parsed.ts || 0;
        } catch(e) {
          // 기존 형식(숫자만) 마이그레이션
          extensions = parseInt(extRecord[0].value) || 0;
        }
      }

      // 마지막 체크 후 DEADLOCK_MINUTES 미경과 시 스킵 (1분마다 카운터 증가 방지)
      var nowMs = Date.now();
      if (lastCheckAt > 0 && (nowMs - lastCheckAt) < DEADLOCK_MINUTES * 60 * 1000) {
        continue;
      }

      if (extensions >= MAX_EXTENSIONS) {
        // ── 데드락 확정: 강제 중단 + 자동 복구 ──
        var totalMin = DEADLOCK_MINUTES * (MAX_EXTENSIONS + 1);
        agentdesk.log.warn("[deadlock] Session " + sess.session_key +
          " — max extensions (" + MAX_EXTENSIONS + ") reached. Force cancelling + re-dispatch.");

        // 1) authoritative force-kill API로 tmux 종료 + inflight cleanup + dispatch fail/retry 일원화
        var forceKillResp = null;
        try {
          var apiPort = agentdesk.config.get("server_port");
          if (!apiPort) {
            agentdesk.log.error("[deadlock] server_port missing — cannot call force-kill API");
            continue;
          }
          var forceKillUrl = "http://127.0.0.1:" + apiPort +
            "/api/sessions/" + encodeURIComponent(sess.session_key) + "/force-kill";
          forceKillResp = agentdesk.http.post(forceKillUrl, { retry: true });
        } catch (e) {
          agentdesk.log.error("[deadlock] force-kill API exception for " + sess.session_key + ": " + e);
          continue;
        }

        if (!forceKillResp || !forceKillResp.ok) {
          agentdesk.log.error("[deadlock] force-kill API failed for " + sess.session_key + ": " + JSON.stringify(forceKillResp));
          continue;
        }

        if (forceKillResp.tmux_killed) {
          agentdesk.log.info("[deadlock] Killed tmux session via API: " + sess.session_key);
        } else {
          agentdesk.log.warn("[deadlock] tmux already gone or kill no-op for " + sess.session_key);
        }

        var redispatched = !!forceKillResp.retry_dispatch_id;
        if (redispatched) {
          agentdesk.log.info("[deadlock] Retry dispatch created: " + forceKillResp.retry_dispatch_id);
        } else if (forceKillResp.queue_activation_requested) {
          agentdesk.log.info("[deadlock] No retry dispatch created — requested auto-queue activation for agent " + sess.agent_id);
        }

        // 4) Deadlock-manager 알림 (announce 봇)
        sendDeadlockAlert(
          "🔴 [Deadlock 복구] " + sess.agent_id + "\n" +
          "session_key: " + sess.session_key + "\n" +
          "tmux: " + ((sess.session_key || "").split(":").pop() || "unknown") + "\n" +
          "연장: " + extensions + "/" + MAX_EXTENSIONS + "\n" +
          totalMin + "분 무응답 → 강제 중단" +
          (redispatched ? " + 재디스패치 완료" : ""));

        // 5) Termination audit
        try {
          var probeInfo = "agent=" + sess.agent_id + " extensions=" + extensions + "/" + MAX_EXTENSIONS +
            " last_heartbeat=" + sess.last_heartbeat +
            " kill_ok=" + (!!forceKillResp.tmux_killed) +
            " inflight_cleared=" + (!!forceKillResp.inflight_cleared);
          agentdesk.db.execute(
            "INSERT INTO session_termination_events (session_key, dispatch_id, killer_component, reason_code, reason_text, probe_snapshot, tmux_alive) VALUES (?, ?, ?, ?, ?, ?, ?)",
            [sess.session_key, sess.active_dispatch_id || null, "deadlock_policy", "deadlock_timeout",
             totalMin + "min timeout — " + (redispatched ? "redispatched" : "cancelled"), probeInfo, 0]
          );
        } catch (e) { /* fire-and-forget */ }

        // 6) 이력 기록 (legacy)
        agentdesk.db.execute(
          "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
          ["deadlock_history:" + sess.session_key + ":" + Date.now(),
           JSON.stringify({
             session_key: sess.session_key,
             agent_id: sess.agent_id,
             dispatch_id: sess.active_dispatch_id,
             retry_dispatch_id: forceKillResp.retry_dispatch_id || null,
             extensions: extensions,
             action: redispatched ? "force_cancel_and_redispatch" : "force_cancel_only",
             ts: new Date().toISOString()
           })]
        );

        // 카운터 삭제 (다음 세션은 새 카운터)
        agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [deadlockKey]);

      } else {
        // ── 데드락 의심: 카운터 증가 (타임스탬프 포함, last_heartbeat 인위적 덮어쓰기 없음) ──
        agentdesk.db.execute(
          "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
          [deadlockKey, JSON.stringify({ count: extensions + 1, ts: nowMs })]
        );
        agentdesk.log.warn("[deadlock] Session " + sess.session_key +
          " — heartbeat stale " + DEADLOCK_MINUTES + "min. Extension " +
          (extensions + 1) + "/" + MAX_EXTENSIONS);
        sendDeadlockAlert(
          "⚠️ [Deadlock 의심] " + sess.agent_id + "\n" +
          "session_key: " + sess.session_key + "\n" +
          "tmux: " + ((sess.session_key || "").split(":").pop() || "unknown") + "\n" +
          "무응답: " + DEADLOCK_MINUTES + "분 (연장 " + (extensions + 1) + "/" + MAX_EXTENSIONS + ")");
      }
    }

    // Clean up deadlock counters for sessions no longer working
    var activeKeys = agentdesk.db.query(
      "SELECT key FROM kv_meta WHERE key LIKE 'deadlock_check:%'"
    );
    for (var ak = 0; ak < activeKeys.length; ak++) {
      var sessKey = activeKeys[ak].key.replace("deadlock_check:", "");
      var stillWorking = agentdesk.db.query(
        "SELECT COUNT(*) as cnt FROM sessions WHERE session_key = ? AND status = 'working'",
        [sessKey]
      );
      if (stillWorking.length > 0 && stillWorking[0].cnt === 0) {
        agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [activeKeys[ak].key]);
      }
    }

    // Clean up old deadlock history entries (7일 이상)
    var historyKeys = agentdesk.db.query(
      "SELECT key FROM kv_meta WHERE key LIKE 'deadlock_history:%'"
    );
    var sevenDaysAgo = Date.now() - 7 * 24 * 60 * 60 * 1000;
    for (var hk = 0; hk < historyKeys.length; hk++) {
      var parts = historyKeys[hk].key.split(":");
      var ts = parseInt(parts[parts.length - 1], 10);
      if (ts && ts < sevenDaysAgo) {
        agentdesk.db.execute("DELETE FROM kv_meta WHERE key = ?", [historyKeys[hk].key]);
      }
    }
  },

  _section_K: function() {
    // ─── [K] 고아 디스패치 복구 (5분) ────────────────────────
    // Card가 in_progress이고 latest dispatch가 pending인데
    // 해당 dispatch_id를 가진 working 세션이 없는 경우 = 고아 디스패치.
    // dcserver 재시작 등으로 세션-디스패치 연결이 끊긴 상태.
    // dispatch를 completed로 마크하고 card를 review로 전이하여 리뷰 파이프라인을 재개한다.
    var kCfg = agentdesk.pipeline.getConfig();
    var kInitial = agentdesk.pipeline.kickoffState(kCfg);
    var kInProgress = agentdesk.pipeline.nextGatedTarget(kInitial, kCfg);
    var kReview = agentdesk.pipeline.nextGatedTarget(kInProgress, kCfg);
    var orphanedDispatches = agentdesk.db.query(
      "SELECT td.id as dispatch_id, td.kanban_card_id, td.dispatch_type " +
      "FROM task_dispatches td " +
      "JOIN kanban_cards kc ON kc.id = td.kanban_card_id " +
      "WHERE kc.status = ? " +
      "AND td.status = 'pending' " +
      "AND kc.latest_dispatch_id = td.id " +
      "AND td.dispatch_type IN ('implementation', 'rework') " +
      "AND td.created_at < datetime('now', '-5 minutes') " +
      "AND NOT EXISTS (" +
      "  SELECT 1 FROM sessions s " +
      "  WHERE s.active_dispatch_id = td.id AND s.status = 'working'" +
      ")",
      [kInProgress]
    );
    for (var op = 0; op < orphanedDispatches.length; op++) {
      var od = orphanedDispatches[op];
      try {
        var decision = agentdesk.runtime.emitSignal("OrphanCandidate", {
          dispatch_id: od.dispatch_id,
          card_id: od.kanban_card_id,
          dispatch_type: od.dispatch_type,
          detected_from: "timeouts._section_K"
        });
        if (decision.executed) {
          agentdesk.log.warn("[orphan-recovery] Supervisor resumed orphaned dispatch " +
            od.dispatch_id + " → card " + od.kanban_card_id + " → " + kReview);
        } else {
          agentdesk.log.info("[orphan-recovery] Supervisor skipped " + od.dispatch_id +
            (decision.note ? " — " + decision.note : ""));
        }
      } catch (e) {
        agentdesk.log.error("[orphan-recovery] Supervisor emit failed for " + od.dispatch_id + ": " + e);
      }
    }
  },

  _section_L: function() {
    // ─── [L] Inflight 장시간 턴 감지 (#130) ──────────────────
    // heartbeat와 독립 — inflight 파일의 started_at 기반 단계별 알림.
    // Prevents alarm fatigue while still notifying at key thresholds.
    var ALERT_THRESHOLDS = [15, 30, 60, 120]; // minutes
    try {
      var inflights = agentdesk.inflight.list();
      for (var li = 0; li < inflights.length; li++) {
        var inf = inflights[li];
        if (!inf.started_at) continue;
        // Stale inflight check: skip cleanup here — let InflightCleanupGuard handle it.
        // Previous approach (checking working sessions) caused false positives because
        // DB session status can lag behind actual tmux state.
        var startedAt = new Date(inf.started_at);
        var elapsedMin = (Date.now() - startedAt.getTime()) / 60000;
        // Find the highest threshold that elapsed time exceeds
        var currentTier = -1;
        for (var t = ALERT_THRESHOLDS.length - 1; t >= 0; t--) {
          if (elapsedMin >= ALERT_THRESHOLDS[t]) { currentTier = t; break; }
        }
        if (currentTier < 0) continue; // under 15min, skip
        // Check if we already alerted at this tier
        var tierKey = "long_turn_tier:" + inf.provider + ":" + inf.channel_id;
        var lastTier = agentdesk.db.query("SELECT value FROM kv_meta WHERE key = ?", [tierKey]);
        var lastAlertedTier = lastTier.length > 0 ? parseInt(lastTier[0].value, 10) : -1;
        if (currentTier <= lastAlertedTier) continue; // already alerted at this tier or higher
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
          "경과: " + Math.round(elapsedMin) + "분 (" + ALERT_THRESHOLDS[currentTier] + "분 단계)\n" +
          "provider: " + (inf.provider || "?")
        );
        agentdesk.db.execute(
          "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
          [tierKey, "" + currentTier]
        );
        agentdesk.log.warn("[long-turn] " + (inf.channel_name || inf.channel_id) + " — " + Math.round(elapsedMin) + "min (tier " + currentTier + ")");
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
    } catch(de) {
      agentdesk.log.warn("[long-turn] inflight scan error: " + de);
    }
  },

  // ─── [M] Workspace branch 보호 (#181) ────────────────────
  // 메인 workspace repo가 wt/* 브랜치로 checkout되면 자동으로 main 복구.
  // 원인: Claude Code 에이전트가 메인 repo에서 worktree 브랜치를 checkout → policy
  // merge cleaner가 worktree 삭제 → 경로 깨짐 → 전체 세션 장애.
  _section_M: function() {
    // Get unique workspace paths from sessions table
    var workspaces = agentdesk.db.query(
      "SELECT DISTINCT json_extract(metadata, '$.workspace') as ws FROM sessions " +
      "WHERE json_extract(metadata, '$.workspace') IS NOT NULL"
    );
    // Also check known workspaces from agents table
    var agentWorkspaces = agentdesk.db.query(
      "SELECT DISTINCT workspace FROM agents WHERE workspace IS NOT NULL AND workspace != ''"
    );
    // Deduplicate
    var seen = {};
    var paths = [];
    for (var w = 0; w < workspaces.length; w++) {
      if (workspaces[w].ws && !seen[workspaces[w].ws]) {
        seen[workspaces[w].ws] = true;
        paths.push(workspaces[w].ws);
      }
    }
    for (var aw = 0; aw < agentWorkspaces.length; aw++) {
      if (agentWorkspaces[aw].workspace && !seen[agentWorkspaces[aw].workspace]) {
        seen[agentWorkspaces[aw].workspace] = true;
        paths.push(agentWorkspaces[aw].workspace);
      }
    }
    for (var p = 0; p < paths.length; p++) {
      var ws = paths[p];
      try {
        var branch = agentdesk.exec("git", JSON.stringify(["-C", ws, "branch", "--show-current"]));
        if (!branch) continue;
        branch = branch.replace(/\s+/g, "");
        if (branch.indexOf("wt/") === 0) {
          agentdesk.log.warn("[branch-guard] Workspace " + ws + " on worktree branch '" + branch + "' — recovering to main");
          // Stash any changes before switching
          agentdesk.exec("git", JSON.stringify(["-C", ws, "stash", "--include-untracked", "-m", "auto-stash before branch-guard recovery"]));
          var checkoutResult = agentdesk.exec("git", JSON.stringify(["-C", ws, "checkout", "main"]));
          agentdesk.exec("git", JSON.stringify(["-C", ws, "pull", "--ff-only"]));
          agentdesk.exec("git", JSON.stringify(["-C", ws, "worktree", "prune"]));
          agentdesk.log.warn("[branch-guard] Recovered " + ws + " to main (was: " + branch + ")");
          sendDeadlockAlert(
            "🔧 [branch-guard] Workspace 브랜치 자동 복구\n" +
            "경로: `" + ws + "`\n" +
            "이탈 브랜치: `" + branch + "` → `main`\n" +
            "원인: 에이전트가 worktree 브랜치를 메인 repo에서 checkout (#181)"
          );
        }
      } catch(e) {
        agentdesk.log.warn("[branch-guard] Error checking " + ws + ": " + e);
      }
    }
  },

  // ─── [N] Orphan review — review 상태인데 dispatch가 없는 카드 자동 복구 ──
  // 패턴: card.status=review, review_entered_at > 5분 전, pending/dispatched
  // review/review-decision/e2e-test dispatch 0건
  // 원인: force-transition 후 dispatch 누락, dispatch 생성 중 에러, race condition 등
  // 복구: in_progress → review 재진입으로 OnReviewEnter 훅이 dispatch를 생성하도록 유도
  _section_N: function() {
    var nCfg = agentdesk.pipeline.getConfig();
    var nInitial = agentdesk.pipeline.kickoffState(nCfg);
    var nInProgress = agentdesk.pipeline.nextGatedTarget(nInitial, nCfg);
    var nReview = agentdesk.pipeline.nextGatedTarget(nInProgress, nCfg);
    if (!nReview) return;

    var orphanReviews = agentdesk.db.query(
      "SELECT kc.id, kc.title, kc.github_issue_number, kc.assigned_agent_id " +
      "FROM kanban_cards kc " +
      "WHERE kc.status = ? " +
      "AND kc.review_entered_at IS NOT NULL " +
      "AND kc.review_entered_at < datetime('now', '-5 minutes') " +
      "AND NOT EXISTS (" +
      "  SELECT 1 FROM task_dispatches td " +
      "  WHERE td.kanban_card_id = kc.id " +
      "  AND td.dispatch_type IN ('review', 'review-decision', 'e2e-test') " +
      "  AND td.status IN ('pending', 'dispatched')" +
      ")",
      [nReview]
    );

    var protectedE2EReviews = agentdesk.db.query(
      "SELECT kc.id, kc.title, kc.github_issue_number, td.id AS dispatch_id, td.status AS dispatch_status " +
      "FROM kanban_cards kc " +
      "JOIN task_dispatches td ON td.kanban_card_id = kc.id " +
      "WHERE kc.status = ? " +
      "AND kc.review_entered_at IS NOT NULL " +
      "AND kc.review_entered_at < datetime('now', '-5 minutes') " +
      "AND td.dispatch_type = 'e2e-test' " +
      "AND td.status IN ('pending', 'dispatched')",
      [nReview]
    );

    for (var p = 0; p < protectedE2EReviews.length; p++) {
      var pc = protectedE2EReviews[p];
      agentdesk.log.info("[timeout] Orphan review guard: card " + pc.id +
        " (#" + (pc.github_issue_number || "?") + ") keeps review state because e2e-test dispatch " +
        pc.dispatch_id + " is still " + pc.dispatch_status);
    }

    // Orphan review = review state with no active dispatch after 5 min.
    // Instead of reimplementing OnReviewEnter safeguards, escalate to
    // pending_decision so PMD can decide the correct action.
    // This avoids partial policy reimplementation (R1/R2 review feedback).
    var nForce = agentdesk.pipeline.forceOnlyTargets(nInProgress, nCfg);
    var nPending = nForce[0];

    for (var n = 0; n < orphanReviews.length; n++) {
      var oc = orphanReviews[n];
      agentdesk.log.warn("[timeout] Orphan review detected: card " + oc.id +
        " (#" + (oc.github_issue_number || "?") + ") in review with no active dispatch → pending_decision");

      agentdesk.kanban.setStatus(oc.id, nPending);
      agentdesk.kanban.setReviewStatus(oc.id, null, {suggestion_pending_at: null});
      agentdesk.reviewState.sync(oc.id, "idle");

      // #231: Queue deduped PM notification — PM must decide
      _queuePMDecision(oc.id, (oc.title || oc.id), "orphan review — dispatch 없음 → pending_decision");
    }
  },

  // ─── [O] Idle session TTL cleanup — idle 60분 tmux-backed 세션 force-kill ──
  _section_O: function() {
    var apiPort = agentdesk.config.get("server_port");
    if (!apiPort) {
      agentdesk.log.error("[idle-kill] server_port missing — cannot call force-kill API");
      return;
    }

    var sessions = agentdesk.db.query(
      "SELECT session_key, agent_id, provider, COALESCE(last_heartbeat, created_at) AS last_seen_at " +
      "FROM sessions " +
      "WHERE status = 'idle' " +
      "AND provider IN ('claude', 'codex', 'qwen') " +
      "AND COALESCE(last_heartbeat, created_at) < datetime('now', '-60 minutes')"
    );

    var now = Date.now();

    for (var i = 0; i < sessions.length; i++) {
      var s = sessions[i];
      if (!s.session_key) continue;

      var lastSeenMs = s.last_seen_at ? new Date(s.last_seen_at).getTime() : NaN;
      var idleMin = isNaN(lastSeenMs) ? 60 : Math.max(60, Math.round((now - lastSeenMs) / 60000));

      var forceKillResp = null;
      try {
        var forceKillUrl = "http://127.0.0.1:" + apiPort +
          "/api/sessions/" + encodeURIComponent(s.session_key) + "/force-kill";
        forceKillResp = agentdesk.http.post(forceKillUrl, { retry: false });
      } catch (e) {
        agentdesk.log.error("[idle-kill] force-kill API exception for " + s.session_key + ": " + e);
        continue;
      }

      if (!forceKillResp || !forceKillResp.ok) {
        agentdesk.log.error("[idle-kill] force-kill API failed for " + s.session_key + ": " + JSON.stringify(forceKillResp));
        continue;
      }

      if (!forceKillResp.tmux_killed) {
        agentdesk.log.warn("[idle-kill] force-kill API succeeded but tmux was already gone for " + s.session_key);
        continue;
      }

      agentdesk.log.info("[idle-kill] Killed idle session after " + idleMin + "min: " + s.session_key);

      var primaryChannel = s.agent_id ? agentdesk.agents.resolvePrimaryChannel(s.agent_id) : null;
      var notifyTarget = primaryChannel ? ("channel:" + primaryChannel) : getPMDChannel();
      if (notifyTarget) {
        sendNotifyAlert(
          notifyTarget,
          "💤 [Idle 세션 자동 종료] " + (s.agent_id || "unknown-agent") + "\n" +
          "provider: `" + (s.provider || "unknown") + "`\n" +
          "session_key: `" + s.session_key + "`\n" +
          "idle: `" + idleMin + "분`\n" +
          "원인: idle 60분 경과 → tmux kill"
        );
      }
    }
  }
};

// ── Tiered tick handlers (#127) ──────────────────────────────────
// Sections are grouped by criticality and cadence.
// onTick (legacy, 5min) is kept as no-op for backward compat.

// 30s tier: [J] retry, [I-0] unsent notification recovery, [I] deadlock, [K] orphan
// Critical-path sections [I] and [K] run here to avoid non-critical section delays (#127)
timeouts.onTick30s = function(ev) {
  var start = Date.now();
  var t;
  t = Date.now(); try { timeouts._section_I0(); } catch(e) { agentdesk.log.warn("[tick30s] I-0 error: " + e); }
  agentdesk.log.debug("[tick30s][I-0] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_J(); } catch(e) { agentdesk.log.warn("[tick30s] J error: " + e); }
  agentdesk.log.debug("[tick30s][J] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_I(); } catch(e) { agentdesk.log.warn("[tick30s] I error: " + e); }
  agentdesk.log.debug("[tick30s][I] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_K(); } catch(e) { agentdesk.log.warn("[tick30s] K error: " + e); }
  agentdesk.log.debug("[tick30s][K] " + (Date.now() - t) + "ms");
  agentdesk.log.debug("[tick30s] total " + (Date.now() - start) + "ms");
};

// 1min tier: [A] [C] [D] [E] [L] (non-critical timeouts)
// [K] moved to 30s tier for critical-path isolation (#127)
timeouts.onTick1min = function(ev) {
  var start = Date.now();
  var t;
  t = Date.now(); try { timeouts._section_A(); } catch(e) { agentdesk.log.warn("[tick1min] A error: " + e); }
  agentdesk.log.debug("[tick1min][A] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_C(); } catch(e) { agentdesk.log.warn("[tick1min] C error: " + e); }
  agentdesk.log.debug("[tick1min][C] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_D(); } catch(e) { agentdesk.log.warn("[tick1min] D error: " + e); }
  agentdesk.log.debug("[tick1min][D] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_E(); } catch(e) { agentdesk.log.warn("[tick1min] E error: " + e); }
  agentdesk.log.debug("[tick1min][E] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_L(); } catch(e) { agentdesk.log.warn("[tick1min] L error: " + e); }
  agentdesk.log.debug("[tick1min][L] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_N(); } catch(e) { agentdesk.log.warn("[tick1min] N error: " + e); }
  agentdesk.log.debug("[tick1min][N] " + (Date.now() - t) + "ms");
  agentdesk.log.debug("[tick1min] total " + (Date.now() - start) + "ms");
};

// 5min tier: [R] [B] [F] [G] [H] [M] [O] + TTL cleanup (non-critical reconciliation)
// [I] moved to 30s tier for critical-path isolation (#127)
timeouts.onTick5min = function(ev) {
  var start = Date.now();
  var t;
  // #126: Purge expired kv_meta keys
  t = Date.now();
  try {
    agentdesk.db.execute("DELETE FROM kv_meta WHERE expires_at IS NOT NULL AND expires_at < datetime('now')");
  } catch(e) { agentdesk.log.warn("[tick5min] kv_ttl error: " + e); }
  agentdesk.log.debug("[tick5min][kv_ttl] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_R(); } catch(e) { agentdesk.log.warn("[tick5min] R error: " + e); }
  agentdesk.log.debug("[tick5min][R] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_B(); } catch(e) { agentdesk.log.warn("[tick5min] B error: " + e); }
  agentdesk.log.debug("[tick5min][B] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_F(); } catch(e) { agentdesk.log.warn("[tick5min] F error: " + e); }
  agentdesk.log.debug("[tick5min][F] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_G(); } catch(e) { agentdesk.log.warn("[tick5min] G error: " + e); }
  agentdesk.log.debug("[tick5min][G] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_H(); } catch(e) { agentdesk.log.warn("[tick5min] H error: " + e); }
  agentdesk.log.debug("[tick5min][H] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_M(); } catch(e) { agentdesk.log.warn("[tick5min] M error: " + e); }
  agentdesk.log.debug("[tick5min][M] " + (Date.now() - t) + "ms");
  t = Date.now(); try { timeouts._section_O(); } catch(e) { agentdesk.log.warn("[tick5min] O error: " + e); }
  agentdesk.log.debug("[tick5min][O] " + (Date.now() - t) + "ms");
  agentdesk.log.debug("[tick5min] total " + (Date.now() - start) + "ms");
};

// Legacy onTick: flush PM decision buffer after all tiered handlers (#231)
timeouts.onTick = function() {
  _flushPMDecisions();
};

agentdesk.registerPolicy(timeouts);
