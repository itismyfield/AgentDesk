/**
 * timeouts.js — ADK Policy: Timeout & Stale Detection
 * priority: 100
 *
 * Hook: onTick (1분 간격 — Rust 서버에서 주기적으로 fire)
 *
 * [A] Requested 타임아웃 (45분) → failed
 * [B] In-Progress 스테일 (2시간) → blocked
 * [C] 스테일 리뷰 (dispatch 완료인데 verdict 없음) → pending_decision
 * [D] DoD 대기 타임아웃 (15분) → pending_decision
 * [E] 자동-수용 결정 타임아웃 → auto-accept + rework
 * [F] 디스패치 큐 타임아웃 (100분) → 제거
 * [G] 스테일 디스패치 정리 (24시간) → failed
 * [H] Stale dispatched 큐 엔트리 진행
 */

var timeouts = {
  name: "timeouts",
  priority: 100,

  onTick: function() {
    // ─── [A] Requested 타임아웃 (45분) ─────────────────────
    var staleRequested = agentdesk.db.query(
      "SELECT id, assigned_agent_id, latest_dispatch_id FROM kanban_cards " +
      "WHERE status = 'requested' AND updated_at < datetime('now', '-45 minutes')"
    );
    for (var i = 0; i < staleRequested.length; i++) {
      // Dispatch를 failed로
      if (staleRequested[i].latest_dispatch_id) {
        agentdesk.db.execute(
          "UPDATE task_dispatches SET status = 'failed', result_summary = 'Timed out waiting for agent', updated_at = datetime('now') WHERE id = ? AND status IN ('pending','dispatched')",
          [staleRequested[i].latest_dispatch_id]
        );
      }
      // 카드는 pending_decision으로 (PMD가 판단)
      agentdesk.db.execute(
        "UPDATE kanban_cards SET status = 'pending_decision', blocked_reason = 'Timed out waiting for agent acceptance', updated_at = datetime('now') WHERE id = ?",
        [staleRequested[i].id]
      );
      agentdesk.log.warn("[timeout] Card " + staleRequested[i].id + " requested timeout → pending_decision");
    }

    // ─── [B] In-Progress 스테일 (2시간) ────────────────────
    var staleInProgress = agentdesk.db.query(
      "SELECT id FROM kanban_cards WHERE status = 'in_progress' AND updated_at < datetime('now', '-2 hours')"
    );
    for (var j = 0; j < staleInProgress.length; j++) {
      agentdesk.db.execute(
        "UPDATE kanban_cards SET status = 'blocked', blocked_reason = 'Stalled: no activity for 2+ hours', updated_at = datetime('now') WHERE id = ?",
        [staleInProgress[j].id]
      );
      agentdesk.log.warn("[timeout] Card " + staleInProgress[j].id + " in_progress stale → blocked");
    }

    // ─── [C] 스테일 리뷰 (dispatch 완료인데 verdict 없음) ──
    var staleReviews = agentdesk.db.query(
      "SELECT kc.id as card_id " +
      "FROM kanban_cards kc " +
      "JOIN task_dispatches td ON td.kanban_card_id = kc.id " +
      "WHERE kc.status = 'review' AND kc.review_status = 'reviewing' " +
      "AND td.dispatch_type = 'review' AND td.status IN ('completed', 'failed') " +
      "AND kc.updated_at < datetime('now', '-30 minutes')"
    );
    for (var k = 0; k < staleReviews.length; k++) {
      agentdesk.db.execute(
        "UPDATE kanban_cards SET status = 'pending_decision', review_status = NULL, updated_at = datetime('now') WHERE id = ?",
        [staleReviews[k].card_id]
      );
      agentdesk.log.warn("[timeout] Stale review → pending_decision: card " + staleReviews[k].card_id);
    }

    // ─── [D] DoD 대기 타임아웃 (15분) ──────────────────────
    var stuckDod = agentdesk.db.query(
      "SELECT id FROM kanban_cards " +
      "WHERE status = 'review' AND review_status = 'awaiting_dod' " +
      "AND updated_at < datetime('now', '-15 minutes')"
    );
    for (var d = 0; d < stuckDod.length; d++) {
      agentdesk.db.execute(
        "UPDATE kanban_cards SET status = 'pending_decision', review_status = NULL, updated_at = datetime('now') WHERE id = ?",
        [stuckDod[d].id]
      );
      agentdesk.log.warn("[timeout] DoD await timeout → pending_decision: card " + stuckDod[d].id);
    }

    // ─── [E] 자동-수용 결정 타임아웃 (suggestion_pending 15분) ──
    var staleSuggestions = agentdesk.db.query(
      "SELECT id FROM kanban_cards " +
      "WHERE review_status = 'suggestion_pending' " +
      "AND updated_at < datetime('now', '-15 minutes')"
    );
    for (var s = 0; s < staleSuggestions.length; s++) {
      // Auto-accept: suggestion_pending → rework_pending
      agentdesk.db.execute(
        "UPDATE kanban_cards SET review_status = 'rework_pending', updated_at = datetime('now') WHERE id = ?",
        [staleSuggestions[s].id]
      );
      agentdesk.log.warn("[timeout] Auto-accepted suggestions for card " + staleSuggestions[s].id);
    }

    // ─── [F] 디스패치 큐 타임아웃 (100분) ──────────────────
    agentdesk.db.execute(
      "DELETE FROM dispatch_queue WHERE queued_at < datetime('now', '-100 minutes')"
    );

    // ─── [G] 스테일 디스패치 정리 (24시간) ──────────────────
    var staleDispatches = agentdesk.db.query(
      "SELECT id, kanban_card_id FROM task_dispatches WHERE status IN ('pending','dispatched') AND created_at < datetime('now', '-24 hours')"
    );
    for (var sd = 0; sd < staleDispatches.length; sd++) {
      agentdesk.db.execute(
        "UPDATE task_dispatches SET status = 'failed', result_summary = 'Stale dispatch auto-failed after 24h', updated_at = datetime('now') WHERE id = ?",
        [staleDispatches[sd].id]
      );
      if (staleDispatches[sd].kanban_card_id) {
        agentdesk.db.execute(
          "UPDATE kanban_cards SET status = 'pending_decision', blocked_reason = 'Stale dispatch auto-failed after 24h', updated_at = datetime('now') WHERE id = ? AND status NOT IN ('done')",
          [staleDispatches[sd].kanban_card_id]
        );
      }
      agentdesk.log.warn("[timeout] Dispatch " + staleDispatches[sd].id + " stale 24h → failed");
    }

    // ─── [H] Stale dispatched 큐 엔트리 진행 ───────────────
    var staleQueueEntries = agentdesk.db.query(
      "SELECT dq.id FROM dispatch_queue dq " +
      "JOIN kanban_cards kc ON kc.id = dq.kanban_card_id " +
      "WHERE dq.status = 'dispatched' AND kc.status NOT IN ('requested', 'in_progress')"
    );
    for (var se = 0; se < staleQueueEntries.length; se++) {
      agentdesk.db.execute(
        "DELETE FROM dispatch_queue WHERE id = ?",
        [staleQueueEntries[se].id]
      );
    }
  }
};

agentdesk.registerPolicy(timeouts);
