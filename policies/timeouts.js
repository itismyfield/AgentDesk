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
 * [M] Workspace branch 보호 (5분) — 메인 repo가 wt/* 브랜치로 이탈하면 자동 복구 (#181)
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

var timeouts = {
  name: "timeouts",
  priority: 100,

  // Legacy onTick: no-op, replaced by tiered tick handlers (#127)
  onTick: function() {},

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
        // Check 2: Minimum work duration (2 min)
        var MIN_WORK_SEC = 120;
        var sessions = agentdesk.db.query(
          "SELECT td.created_at as first_work, MAX(s.last_heartbeat) as last_seen " +
          "FROM task_dispatches td " +
          "JOIN sessions s ON s.active_dispatch_id = td.id AND s.status = 'working' " +
          "WHERE td.id = ?",
          [di.id]
        );
        if (sessions.length > 0 && sessions[0].first_work && sessions[0].last_seen) {
          var durationSec = (new Date(sessions[0].last_seen) - new Date(sessions[0].first_work)) / 1000;
          if (durationSec < MIN_WORK_SEC) {
            reasons.push("작업 시간 부족: " + Math.round(durationSec) + "초 (최소 " + MIN_WORK_SEC + "초)");
          }
        }
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
          // PMD notification via async outbox (#120)
          var pmdCh = agentdesk.config.get("kanban_manager_channel_id");
          if (pmdCh) {
            var cardTitle2 = agentdesk.db.query("SELECT title FROM kanban_cards WHERE id = ?", [card.id]);
            var t2 = cardTitle2.length > 0 ? cardTitle2[0].title : card.id;
            var pmdMsg = "[PM Decision] " + t2 + "\n사유: " + reasons.join("; ");
            agentdesk.message.queue("channel:" + pmdCh, pmdMsg, "announce", "system");
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
      "COALESCE(td.retry_count, 0) as retry_count " +
      "FROM kanban_cards kc " +
      "LEFT JOIN task_dispatches td ON td.id = kc.latest_dispatch_id " +
      "WHERE kc.status = ? AND kc.requested_at IS NOT NULL AND kc.requested_at < datetime('now', '" + requestedInterval + "')",
      [aInitial]
    );
    for (var i = 0; i < staleRequested.length; i++) {
      var rc = staleRequested[i];
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
        // PMD에게 결정 요청
        var cardInfo = agentdesk.db.query(
          "SELECT title, github_issue_url, assigned_agent_id FROM kanban_cards WHERE id = ?",
          [rc.id]
        );
        var cardTitle = (cardInfo.length > 0) ? cardInfo[0].title : rc.id;
        var cardUrl = (cardInfo.length > 0 && cardInfo[0].github_issue_url) ? "\n" + cardInfo[0].github_issue_url : "";
        var assignee = (cardInfo.length > 0 && cardInfo[0].assigned_agent_id) ? cardInfo[0].assigned_agent_id : "미배정";
        var kmChannel = getPMDChannel();
        if (kmChannel) {
          agentdesk.message.queue(
            kmChannel,
            "[PM Decision] " + cardTitle + "\n사유: " + MAX_DISPATCH_RETRIES + " retries exhausted",
            "announce",
            "system"
          );
        }
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
      "SELECT id FROM kanban_cards WHERE status = ? AND started_at IS NOT NULL AND started_at < datetime('now', '" + inProgressInterval + "')",
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
      // PMD에게 결정 요청 (announce bot)
      var stalledInfo = agentdesk.db.query(
        "SELECT title, github_issue_url, assigned_agent_id FROM kanban_cards WHERE id = ?",
        [staleInProgress[j].id]
      );
      var stalledTitle = (stalledInfo.length > 0) ? stalledInfo[0].title : staleInProgress[j].id;
      var stalledUrl = (stalledInfo.length > 0 && stalledInfo[0].github_issue_url) ? "\n" + stalledInfo[0].github_issue_url : "";
      var stalledAssignee = (stalledInfo.length > 0 && stalledInfo[0].assigned_agent_id) ? stalledInfo[0].assigned_agent_id : "미배정";
      var kmChannel2 = getPMDChannel();
      if (kmChannel2) {
        agentdesk.message.queue(
          kmChannel2,
          "[Stalled] " + stalledTitle + " (담당: " + stalledAssignee + ")" + stalledUrl + "\n" + staleMin + "분+ 활동 없음 → blocked",
          "announce",
          "system"
        );
      }
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
      "SELECT td.id, td.dispatch_type, td.to_agent_id, kc.title, kc.github_issue_url, kc.github_issue_number " +
      "FROM task_dispatches td " +
      "JOIN kanban_cards kc ON td.kanban_card_id = kc.id " +
      "WHERE td.status = 'pending' " +
      "AND td.created_at < datetime('now', '-2 minutes') " +
      "AND NOT EXISTS (SELECT 1 FROM kv_meta WHERE key = 'dispatch_notified:' || td.id)"
    );
    for (var un = 0; un < unnotifiedDispatches.length; un++) {
      var ud = unnotifiedDispatches[un];

      // Determine channel
      var agentChannel = agentdesk.db.query(
        "SELECT discord_channel_id, discord_channel_alt FROM agents WHERE id = ?",
        [ud.to_agent_id]
      );
      if (agentChannel.length === 0) continue;

      // Only "review" goes to the counter-model alt channel.
      // "review-decision" is sent to the primary channel to reuse the implementation thread.
      var useAlt = (ud.dispatch_type === "review");
      var channelId = useAlt ? agentChannel[0].discord_channel_alt : agentChannel[0].discord_channel_id;
      if (!channelId) continue;

      var issueLink = ud.github_issue_url
        ? "\n[" + ud.title + " #" + ud.github_issue_number + "](<" + ud.github_issue_url + ">)"
        : "";
      var prefix = useAlt
        ? "DISPATCH:" + ud.id + " - " + ud.title + "\n⚠️ 검토 전용 — 작업 착수 금지\n코드 리뷰만 수행하고 GitHub 이슈에 코멘트로 피드백해주세요."
        : "DISPATCH:" + ud.id + " - " + ud.title;

      var notifyContent = prefix + issueLink;
      agentdesk.message.queue("channel:" + channelId, notifyContent, "announce", "system");
      agentdesk.db.execute(
        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('dispatch_notified:' || ?1, datetime('now'))",
        [ud.id]
      );
      agentdesk.log.info("[notify-recovery] Dispatch " + ud.id + " queued for delivery");
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

        // Discord 알림 직접 전송 ([I-0] 2분 대기 없이 즉시 알림)
        var retryAgent = agentdesk.db.query(
          "SELECT discord_channel_id, discord_channel_alt FROM agents WHERE id = ?",
          [fd.to_agent_id]
        );
        if (retryAgent.length > 0) {
          var useAlt = (fd.dispatch_type === "review");
          var retryChannelId = useAlt ? retryAgent[0].discord_channel_alt : retryAgent[0].discord_channel_id;
          if (retryChannelId) {
            var issueLink = fd.github_issue_url
              ? "\n[" + fd.title + " #" + fd.github_issue_number + "](<" + fd.github_issue_url + ">)"
              : "";
            var retryPrefix = useAlt
              ? "DISPATCH:" + newDispatchId + " - " + fd.title + "\n⚠️ 검토 전용 — 작업 착수 금지\n코드 리뷰만 수행하고 GitHub 이슈에 코멘트로 피드백해주세요."
              : "DISPATCH:" + newDispatchId + " - " + fd.title;
            var retryContent = retryPrefix + issueLink;
            agentdesk.message.queue("channel:" + retryChannelId, retryContent, "announce", "system");
            agentdesk.db.execute(
              "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('dispatch_notified:' || ?1, datetime('now'))",
              [newDispatchId]
            );
            agentdesk.log.info("[retry] Dispatch " + newDispatchId + " notification queued");
          }
        }
      } catch (e) {
        agentdesk.log.error("[retry] Failed to create retry dispatch for card " +
          fd.kanban_card_id + ": " + e);
        // Don't block the card on transient retry failure — leave status as-is
        // so the next tick can retry. Only log the error.
      }
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
    var staleWorkingSessions = agentdesk.db.query(
      "SELECT session_key FROM sessions WHERE status = 'working' " +
      "AND last_heartbeat < datetime('now', '-3 minutes')"
    );
    for (var sw = 0; sw < staleWorkingSessions.length; sw++) {
      var swKey = staleWorkingSessions[sw].session_key;
      var tmuxName = (swKey || "").split(":").pop();
      // Check if tmux session is still alive and has a running process
      var tmuxAlive = false;
      try {
        var checkOut = agentdesk.exec("tmux", JSON.stringify(["list-panes", "-t", tmuxName, "-F", "#{pane_current_command}"]));
        tmuxAlive = checkOut && checkOut.indexOf("agentdesk") !== -1;
      } catch(e) { tmuxAlive = false; }
      if (!tmuxAlive) {
        agentdesk.db.execute(
          "UPDATE sessions SET status = 'idle' WHERE session_key = ? AND status = 'working'",
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

        // 1) agentdesk.session.kill로 tmux 세션 강제 종료
        var killResult = JSON.parse(agentdesk.session.kill(sess.session_key));
        if (killResult.ok) {
          agentdesk.log.info("[deadlock] Killed tmux session: " + sess.session_key);
        } else {
          // kill 실패 — tmux 세션이 이미 죽어있는지 확인
          var tmuxName = sess.session_key.split(":").pop() || sess.session_key;
          var tmuxExists = false;
          try {
            var checkResult = agentdesk.exec("tmux", JSON.stringify(["has-session", "-t", tmuxName]));
            tmuxExists = (checkResult && checkResult.indexOf("error") === -1);
          } catch(e) {
            tmuxExists = false;
          }
          if (tmuxExists) {
            // tmux 세션이 살아있으면 worker가 아직 동작 중 — 건너뜀
            agentdesk.log.warn("[deadlock] tmux kill failed but session alive, skipping re-dispatch: " + killResult.error);
            continue;
          }
          // tmux 세션이 없으면 고아 상태 — disconnected 전환 + 재디스패치 진행
          agentdesk.log.warn("[deadlock] tmux session gone (orphan), proceeding with cleanup: " + tmuxName);
        }

        // 2) 세션 상태 disconnected (last_heartbeat는 원본 유지 — 인위적 덮어쓰기 방지)
        agentdesk.db.execute(
          "UPDATE sessions SET status = 'disconnected' WHERE session_key = ?",
          [sess.session_key]
        );

        // 3) 현재 디스패치 실패 + 재디스패치
        var redispatched = false;
        if (sess.active_dispatch_id) {
          // 먼저 현재 상태 확인 — 이미 completed/failed면 재디스패치 불필요
          var dispInfo = agentdesk.db.query(
            "SELECT kanban_card_id, to_agent_id, dispatch_type, title, status " +
            "FROM task_dispatches WHERE id = ?",
            [sess.active_dispatch_id]
          );

          if (dispInfo.length > 0 && (dispInfo[0].status === "pending" || dispInfo[0].status === "dispatched")) {
            var di = dispInfo[0];
            var dlResult = agentdesk.dispatch.markFailed(sess.active_dispatch_id, "Deadlock auto-recovery: " + totalMin + "min timeout");
            if (dlResult.rows_affected === 0) {
              agentdesk.log.info("[deadlock] Dispatch " + sess.active_dispatch_id + " already terminal, skipping");
              continue;
            }

            try {
              agentdesk.dispatch.create(
                di.kanban_card_id,
                di.to_agent_id,
                di.dispatch_type || "implementation",
                "[Retry] " + (di.title || "deadlock recovery")
              );
              redispatched = true;
              agentdesk.log.info("[deadlock] Re-dispatched card " +
                di.kanban_card_id + " → " + di.to_agent_id);
            } catch (e) {
              // 재디스패치 실패 시 PMD 판단으로 이관
              agentdesk.kanban.setStatus(di.kanban_card_id, iPending);
              agentdesk.db.execute(
                "UPDATE kanban_cards SET blocked_reason = ? WHERE id = ?",
                ["Deadlock recovery re-dispatch failed: " + e, di.kanban_card_id]
              );
              agentdesk.log.error("[deadlock] Re-dispatch failed for " +
                di.kanban_card_id + ": " + e + " → pending_decision");
            }
          } else if (dispInfo.length > 0) {
            agentdesk.log.info("[deadlock] Dispatch " + sess.active_dispatch_id +
              " already " + dispInfo[0].status + " — skip re-dispatch");
          }
        }

        // 4) Deadlock-manager 알림 (announce 봇)
        sendDeadlockAlert(
          "🔴 [Deadlock 복구] " + sess.agent_id + "\n" +
          "session_key: " + sess.session_key + "\n" +
          "tmux: " + ((sess.session_key || "").split(":").pop() || "unknown") + "\n" +
          "연장: " + extensions + "/" + MAX_EXTENSIONS + "\n" +
          totalMin + "분 무응답 → 강제 중단" +
          (redispatched ? " + 재디스패치 완료" : ""));

        // 5) 이력 기록
        agentdesk.db.execute(
          "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
          ["deadlock_history:" + sess.session_key + ":" + Date.now(),
           JSON.stringify({
             session_key: sess.session_key,
             agent_id: sess.agent_id,
             dispatch_id: sess.active_dispatch_id,
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
      // 1) Dispatch를 completed로 마크
      agentdesk.dispatch.markCompleted(od.dispatch_id, '{"auto_completed":true,"completion_source":"orphan_recovery"}');
      // 2) Card를 review로 전이 → OnReviewEnter 훅이 review dispatch를 생성
      agentdesk.kanban.setStatus(od.kanban_card_id, kReview);
      agentdesk.log.warn("[orphan-recovery] Completed orphaned dispatch " + od.dispatch_id +
        " (type=" + od.dispatch_type + ") → card " + od.kanban_card_id + " → " + kReview);
      // 3) PMD 알림
      var orphanInfo = agentdesk.db.query(
        "SELECT title, assigned_agent_id FROM kanban_cards WHERE id = ?",
        [od.kanban_card_id]
      );
      var orphanTitle = (orphanInfo.length > 0) ? orphanInfo[0].title : od.kanban_card_id;
      var orphanAgent = (orphanInfo.length > 0) ? orphanInfo[0].assigned_agent_id : "?";
      sendNotifyAlert(getPMDChannel(),
        "🔄 [고아 디스패치 복구] " + orphanAgent + " — " + orphanTitle +
        "\n사유: pending 디스패치 5분 경과 + 활성 세션 없음 → " + kReview + " 전이");
    }
  },

  _section_L: function() {
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
        sendDeadlockAlert(
          "⚠️ [장시간 턴] " + (inf.channel_name || inf.channel_id) + "\n" +
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
    // If no workspaces found from DB, check the server's own workspace
    if (paths.length === 0) {
      var serverWs = agentdesk.config.get("workspace");
      if (serverWs) paths.push(serverWs);
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
  // 패턴: card.status=review, review_entered_at > 5분 전, pending/dispatched review dispatch 0건
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
      "  AND td.dispatch_type IN ('review', 'review-decision') " +
      "  AND td.status IN ('pending', 'dispatched')" +
      ")",
      [nReview]
    );

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

      var kmChannel = getPMDChannel();
      if (kmChannel) {
        agentdesk.message.queue(
          kmChannel,
          "⚠️ [orphan-review] #" + (oc.github_issue_number || "?") + " " +
          (oc.title || oc.id) + "\nreview 상태인데 dispatch 없음 → pending_decision 전환 (PMD 결정 필요)",
          "notify",
          "system"
        );
      }
    }
  },

  // ─── [I] 컨텍스트 윈도우 자동 관리 ─────────────────────
  // onTick에서 세션 토큰 사용량을 모니터링하고 compact/clear 자동 호출
  onContextCheck: function() {
    var CONTEXT_WINDOW = 1000000; // 1M tokens
    var compactPercent = parseInt(agentdesk.config.get("context_compact_percent") || "60", 10);
    var clearPercent = parseInt(agentdesk.config.get("context_clear_percent") || "40", 10);
    var clearIdleMin = parseInt(agentdesk.config.get("context_clear_idle_minutes") || "60", 10);

    var sessions = agentdesk.db.query(
      "SELECT session_key, agent_id, tokens, status, last_heartbeat, provider FROM sessions WHERE status IN ('idle', 'working')"
    );

    var now = Date.now();

    for (var i = 0; i < sessions.length; i++) {
      var s = sessions[i];
      if (!s.session_key) continue;

      // Skip non-Claude sessions
      var provider = s.provider || "claude";
      if (provider !== "claude") continue;

      // Skip working sessions — turn-end handler in tmux watcher handles compact
      if (s.status === "working") continue;

      // Check cooldown (5 min) to avoid spamming commands
      var cooldownKey = "context_action_" + s.session_key;
      var lastAction = agentdesk.db.query(
        "SELECT value FROM kv_meta WHERE key = ?", [cooldownKey]
      );
      if (lastAction.length > 0) {
        var lastMs = parseInt(lastAction[0].value, 10);
        if (now - lastMs < 300000) continue; // 5 min cooldown
      }

      // Use DB tokens directly — updated from result events by tmux watcher/turn_bridge
      var pct = (s.tokens / CONTEXT_WINDOW) * 100;

      // Compact: >= compactPercent
      if (pct >= compactPercent) {
        var result = JSON.parse(agentdesk.session.sendCommand(s.session_key, "/compact"));
        if (result.ok) {
          agentdesk.log.info("[context] Auto-compact: " + s.session_key + " (" + Math.round(pct) + "%)");
          agentdesk.db.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
            [cooldownKey, "" + now]
          );
          // Discord notification
          var agent = agentdesk.db.query("SELECT discord_channel_id FROM agents WHERE id = ?", [s.agent_id]);
          if (agent.length > 0 && agent[0].discord_channel_id) {
            sendNotifyAlert(
              "channel:" + agent[0].discord_channel_id,
              "⚡ 컨텍스트 자동 compact 실행 (" + Math.round(pct) + "% → " + s.session_key + ")"
            );
          }
        }
        continue; // Don't also clear in same tick
      }

      // Clear: >= clearPercent AND idle for clearIdleMin
      if (pct >= clearPercent && s.last_heartbeat) {
        var lastHb = new Date(s.last_heartbeat).getTime();
        var idleMs = now - lastHb;
        var idleMin = idleMs / 60000;

        if (idleMin >= clearIdleMin) {
          var result2 = JSON.parse(agentdesk.session.sendCommand(s.session_key, "/clear"));
          if (result2.ok) {
            agentdesk.log.info("[context] Auto-clear: " + s.session_key + " (" + Math.round(pct) + "%, idle " + Math.round(idleMin) + "min)");
            agentdesk.db.execute(
              "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?, ?)",
              [cooldownKey, "" + now]
            );
            var agent2 = agentdesk.db.query("SELECT discord_channel_id FROM agents WHERE id = ?", [s.agent_id]);
            if (agent2.length > 0 && agent2[0].discord_channel_id) {
              sendNotifyAlert(
                "channel:" + agent2[0].discord_channel_id,
                "🧹 컨텍스트 자동 clear 실행 (" + Math.round(pct) + "%, idle " + Math.round(idleMin) + "분 → " + s.session_key + ")"
              );
            }
          }
        }
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

// 5min tier: [R] [B] [F] [G] [H] [ctx] + TTL cleanup (non-critical reconciliation)
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
  if (timeouts.onContextCheck) {
    t = Date.now(); try { timeouts.onContextCheck(); } catch(e) { agentdesk.log.warn("[tick5min] ctx error: " + e); }
    agentdesk.log.debug("[tick5min][ctx] " + (Date.now() - t) + "ms");
  }
  agentdesk.log.debug("[tick5min] total " + (Date.now() - start) + "ms");
};

// Legacy onTick: no-op (tiered hooks handle everything)
timeouts.onTick = function() {};

agentdesk.registerPolicy(timeouts);
