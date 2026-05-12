/**
 * kanban-rules.js — ADK Policy: Core Kanban Lifecycle
 * priority: 10 (runs first)
 *
 * Hooks:
 *   onSessionStatusChange — dispatch session 상태 → card 상태 동기화
 *   onDispatchCompleted   — 완료 검증 (PM Decision Gate) + review 진입
 *   onCardTransition      — 상태별 부수효과 (dispatch 생성, PMD 알림 등)
 *   onCardTerminal        — completed_at 기록 + 자동큐 진행
 *
 * #1078: Helper functions live in policies/lib/kanban-*.js submodules to keep
 * this file focused on the registered policy hooks. The shapes exposed via
 * `module.exports` (policy, _loadCardAlertContext, __test.runPreflight,
 * __test._loadCardAlertContext) are preserved.
 */

// ── Submodule helpers ────────────────────────────────────────
var _notifications = require("./lib/kanban-notifications");
var sendDiscordNotification = _notifications.sendDiscordNotification;
var emitQualityEvent = _notifications.emitQualityEvent;
var _loadCardAlertContext = _notifications._loadCardAlertContext;
var _formatCardAlertLabel = _notifications._formatCardAlertLabel;
var notifyCardOwner = _notifications.notifyCardOwner;

var _cardMetadata = require("./lib/kanban-card-metadata");
var _loadCardMetadata = _cardMetadata._loadCardMetadata;
var _mergeCardMetadata = _cardMetadata._mergeCardMetadata;
var _metadataParam = _cardMetadata._metadataParam;
var _writeCardMetadata = _cardMetadata._writeCardMetadata;
var _findAutoQueueEntriesByDispatch = _cardMetadata._findAutoQueueEntriesByDispatch;

var _inventory = require("./lib/kanban-inventory-refresh");
var INVENTORY_DOC_PATHS = _inventory.INVENTORY_DOC_PATHS;
var _extractRepoFromUrl = _inventory._extractRepoFromUrl;
var _firstPresent = _inventory._firstPresent;
var _execOrThrow = _inventory._execOrThrow;
var _splitNonEmptyLines = _inventory._splitNonEmptyLines;
var _normalizeDispatchTimestamp = _inventory._normalizeDispatchTimestamp;
var _resolveCompletedWorktreePath = _inventory._resolveCompletedWorktreePath;
var _resolveCompletedBranch = _inventory._resolveCompletedBranch;
var _dispatchTouchedSrcSinceCreated = _inventory._dispatchTouchedSrcSinceCreated;
var _inventoryDocsChanged = _inventory._inventoryDocsChanged;
var _autoRefreshInventoryDocs = _inventory._autoRefreshInventoryDocs;

var _preflight = require("./lib/kanban-preflight");
var _runPreflight = _preflight._runPreflight;

// ── Policy ───────────────────────────────────────────────────

var rules = {
  name: "kanban-rules",
  priority: 10,

  // ── Session status → Card status ──────────────────────────
  onSessionStatusChange: function(payload) {
    // Require dispatch_id — sessions without an active dispatch cannot drive card transitions
    if (!payload.dispatch_id) return;

    // Boot grace period: 서버 부팅 후 10분간 세션 상태 변경으로 인한 카드 전환 유예.
    // 재시작 직후 세션이 disconnected/idle로 보고되면서 진행 중인 카드가 오판되는 것을 방지.
    var isActiveSession = payload.status === "turn_active" || payload.status === "working";
    if (!isActiveSession) {
      var bootRows = agentdesk.db.query(
        "SELECT value FROM kv_meta WHERE key = 'server_boot_at'"
      );
      if (bootRows.length > 0) {
        var bootAt = new Date(bootRows[0].value + "Z");
        var bootElapsedMin = (Date.now() - bootAt.getTime()) / 60000;
        if (bootElapsedMin < 10) {
          return;
        }
      }
    }

    var cards = agentdesk.db.query(
      "SELECT id, status FROM kanban_cards WHERE latest_dispatch_id = ?",
      [payload.dispatch_id]
    );
    if (cards.length === 0) return;
    // #2051 Finding 5 (P2): a single dispatch_id should map to at most one
    // card, but reopen/race paths can violate this invariant. Log every
    // additional card so operators can spot drift without breaking the existing
    // first-card semantics.
    if (cards.length > 1) {
      agentdesk.log.warn(
        "[kanban] onSessionStatusChange dispatch " + payload.dispatch_id +
        " matched " + cards.length + " cards — only " + cards[0].id +
        " will be advanced; remaining card_ids=" +
        cards.slice(1).map(function (c) { return c.id; }).join(",")
      );
    }
    var card = cards[0];
    var cfg = agentdesk.pipeline.resolveForCard(card.id);
    var initialState = agentdesk.pipeline.kickoffState(cfg);
    var nextFromInitial = agentdesk.pipeline.nextGatedTarget(initialState, cfg);

    // working → nextFromInitial: only for implementation/rework dispatches
    // Review dispatches should NOT advance the card to in_progress
    if (isActiveSession && card.status === initialState) {
      var dispatch = agentdesk.db.query(
        "SELECT dispatch_type, status FROM task_dispatches WHERE id = ?",
        [payload.dispatch_id]
      );
      if (dispatch.length === 0) return;
      var dtype = dispatch[0].dispatch_type;
      var dstatus = dispatch[0].status;
      // #2051 Finding 5 (P2): also require the dispatch to still be live
      // (pending/dispatched). Without this guard a cancelled/failed dispatch
      // could still drive the card forward via a stale session status event,
      // and combined with Finding 1 (now fixed) used to bypass the
      // has_active_dispatch gate entirely.
      var dispatchLive = dstatus === "pending" || dstatus === "dispatched";
      // Only implementation and rework dispatches acknowledge work start
      if ((dtype === "implementation" || dtype === "rework") && dispatchLive) {
        agentdesk.kanban.setStatus(card.id, nextFromInitial);
        agentdesk.log.info("[kanban] " + card.id + " " + initialState + " → " + nextFromInitial + " (ack via " + dtype + " dispatch " + payload.dispatch_id + ")");
      } else if ((dtype === "implementation" || dtype === "rework") && !dispatchLive) {
        agentdesk.log.info(
          "[kanban] onSessionStatusChange skipped advance for " + card.id +
          " — dispatch " + payload.dispatch_id + " no longer live (status=" + dstatus + ")"
        );
      }
    }

    // idle on implementation/rework is handled in Rust hook_session by completing
    // the pending dispatch first, then letting onDispatchCompleted drive review entry.

    // idle + review dispatch → auto-complete is handled by Rust
    // (dispatched_sessions.rs idle auto-complete → complete_dispatch → OnDispatchCompleted).
    // Previously this JS policy also auto-completed review dispatches via direct DB UPDATE,
    // causing double processing (JS verdict extraction + Rust OnDispatchCompleted).
    // Now only Rust handles auto-complete; JS policy reacts via onDispatchCompleted hook.
    //
    // #2051 Finding 4 (P3): the legacy `if (false && payload.status === "idle"
    // ...)` block that previously performed synchronous `gh issue view
    // --comments` calls from this hook has been deleted. It was unreachable but
    // misleading — `var` hoisting kept the variable scope but ran no code, and
    // grepping the file made it look like verdict extraction lived in two
    // places. Auto-complete + verdict resolution for review dispatches is now
    // owned exclusively by Rust (`dispatched_sessions.rs` →
    // `complete_dispatch` → `OnDispatchCompleted`).
  },

  // ── Dispatch Completed — PM Decision Gate ─────────────────
  onDispatchCompleted: function(payload) {
    var dispatches = agentdesk.db.query(
      "SELECT id, kanban_card_id, to_agent_id, dispatch_type, chain_depth, created_at, result, context, status FROM task_dispatches WHERE id = ?",
      [payload.dispatch_id]
    );
    if (dispatches.length === 0) return;
    var dispatch = dispatches[0];
    var dispatchContext = {};
    try { dispatchContext = JSON.parse(dispatch.context || "{}"); } catch (e) { dispatchContext = {}; }
    if (dispatchContext.phase_gate) return;
    if (!dispatch.kanban_card_id) return;
    // #815 + #2051 Finding 27 (P3): only treat dispatches that are actually
    // `completed` as triggers for the review pipeline. Previously this guard
    // only skipped `cancelled`, which left `failed`/`superseded`/other
    // non-completed statuses able to drive the card into review on a race.
    // OnDispatchCompleted should only fan-out on the completed terminal state;
    // any other terminal status (cancelled by user, marked failed by retry
    // sweep, etc.) must be a no-op so the rest of the lifecycle can react.
    if (dispatch.status !== "completed") {
      agentdesk.log.info(
        "[kanban] onDispatchCompleted: skipping non-completed dispatch " +
          dispatch.id + " (status=" + dispatch.status + ")"
      );
      return;
    }

    var cards = agentdesk.db.query(
      "SELECT id, title, status, priority, assigned_agent_id, deferred_dod_json FROM kanban_cards WHERE id = ?",
      [dispatch.kanban_card_id]
    );
    if (cards.length === 0) return;
    var card = cards[0];
    var cfg = agentdesk.pipeline.resolveForCard(card.id);
    var inProgressState = agentdesk.pipeline.nextGatedTarget(agentdesk.pipeline.kickoffState(cfg), cfg);
    var reviewState = agentdesk.pipeline.nextGatedTarget(inProgressState, cfg);

    // Skip terminal cards
    if (agentdesk.pipeline.isTerminal(card.status, cfg)) return;

    // Review/create-pr lifecycle dispatches are handled by review-automation.
    if (dispatch.dispatch_type === "review"
        || dispatch.dispatch_type === "review-decision"
        || dispatch.dispatch_type === "create-pr") return;

    // #197: e2e-test dispatches — handled by deploy-pipeline policy
    if (dispatch.dispatch_type === "e2e-test") return;

    // #256: Consultation dispatch completed — update preflight metadata
    if (dispatch.dispatch_type === "consultation") {
      var consultResult = {};
      try { consultResult = JSON.parse(dispatch.result || "{}"); } catch(e) {}
      var meta = _loadCardMetadata(dispatch.kanban_card_id);
      meta.consultation_status = "completed";
      meta.consultation_result = consultResult;
      // If consultation clarified the issue, update preflight_status to "clear"
      // and immediately resume the linked auto-queue entry with a fresh
      // implementation dispatch. Otherwise escalate to manual intervention.
      if (consultResult.verdict === "clear" || consultResult.verdict === "proceed") {
        meta.preflight_status = "clear";
        meta.preflight_summary = "Consultation resolved: " + (consultResult.summary || "clarified");
        _writeCardMetadata(dispatch.kanban_card_id, meta);
        var aqEntries = _findAutoQueueEntriesByDispatch(dispatch.id, false);
        if (aqEntries.length > 0) {
          // #2051 Finding 15 (P2): a consultation may be linked to multiple
          // auto_queue_entries via auto_queue_entry_dispatch_history. Previously
          // only `aqEntries[0]` was redispatched + status-updated, leaving the
          // rest stuck in `dispatched` and prone to spurious dispatch creation
          // on the next stuck-dispatched sweep. Dispatch implementation for
          // the first entry (single agent owns the card) and mark every other
          // entry as `skipped` so the queue does not loop them.
          var primary = aqEntries[0];
          try {
            var nextDispatchId = agentdesk.dispatch.create(
              dispatch.kanban_card_id,
              primary.agent_id,
              "implementation",
              card.title || "Implementation",
              {
                auto_queue: true,
                entry_id: primary.id,
                parent_dispatch_id: dispatch.id
              }
            );
            if (nextDispatchId) {
              agentdesk.autoQueue.updateEntryStatus(
                primary.id,
                "dispatched",
                "consultation_resume",
                { dispatchId: nextDispatchId }
              );
              agentdesk.log.info("[preflight] Consultation resolved for " + dispatch.kanban_card_id + " — resumed implementation dispatch " + nextDispatchId);
            }
          } catch (e) {
            agentdesk.log.warn("[preflight] Consultation resolved for " + dispatch.kanban_card_id + " but implementation redispatch failed: " + e);
          }
          if (aqEntries.length > 1) {
            for (var aqi = 1; aqi < aqEntries.length; aqi++) {
              try {
                agentdesk.autoQueue.updateEntryStatus(
                  aqEntries[aqi].id,
                  "skipped",
                  "consultation_resume_duplicate",
                  { primaryEntryId: primary.id }
                );
              } catch (skipErr) {
                agentdesk.log.warn(
                  "[preflight] could not mark duplicate aq entry " +
                  aqEntries[aqi].id + " as skipped: " + skipErr
                );
              }
            }
            agentdesk.log.warn(
              "[preflight] Consultation dispatch " + dispatch.id +
              " was linked to " + aqEntries.length +
              " auto_queue entries — dispatched primary " + primary.id +
              " and skipped the remaining " + (aqEntries.length - 1)
            );
          }
        } else {
          agentdesk.log.info("[preflight] Consultation resolved for " + dispatch.kanban_card_id + " → clear");
        }
      } else {
        meta.preflight_status = "escalated";
        meta.preflight_summary = "Consultation did not resolve: " + (consultResult.summary || "still ambiguous");
        agentdesk.db.execute(
          "UPDATE kanban_cards SET metadata = ?, blocked_reason = ? WHERE id = ?",
          [_metadataParam(meta), "Consultation did not resolve ambiguity", dispatch.kanban_card_id]
        );
        escalateToManualIntervention(dispatch.kanban_card_id, "Consultation did not resolve ambiguity");
        agentdesk.log.warn("[preflight] Consultation unresolved for " + dispatch.kanban_card_id + " → manual intervention");
      }
      return;
    }

    var workResult = {};
    try { workResult = JSON.parse(dispatch.result || "{}"); } catch(e) {}
    _autoRefreshInventoryDocs(card, dispatch, dispatchContext, workResult);
    if ((dispatch.dispatch_type === "implementation" || dispatch.dispatch_type === "rework")
        && (workResult.work_outcome === "noop" || workResult.completed_without_changes === true)) {
      _mergeCardMetadata(dispatch.kanban_card_id, {
        work_resolution_status: "noop",
        work_resolution_result: workResult,
        preflight_status: null,
        preflight_summary: null,
        preflight_checked_at: null,
        consultation_status: null,
        consultation_result: null
      });
      agentdesk.db.execute("UPDATE kanban_cards SET blocked_reason = NULL WHERE id = ?", [dispatch.kanban_card_id]);
      agentdesk.log.info("[kanban] " + card.id + " " + dispatch.dispatch_type + " noop completion recorded — routing through review flow");
    }

    // Rework dispatches — skip gate, go directly to review
    if (dispatch.dispatch_type === "rework") {
      agentdesk.kanban.setStatus(card.id, reviewState);
      agentdesk.log.info("[kanban] " + card.id + " rework done → " + reviewState);
      return;
    }

    // ── XP reward ──
    var xpMap = { "low": 5, "medium": 10, "high": 18, "urgent": 30 };
    var xp = xpMap[card.priority] || 10;
    xp += Math.min(dispatch.chain_depth || 0, 3) * 2;

    if (dispatch.to_agent_id) {
      agentdesk.db.execute(
        "UPDATE agents SET xp = xp + ? WHERE id = ?",
        [xp, dispatch.to_agent_id]
      );
    }

    // ── PM Decision Gate ──
    // Skip gate if dispatch context has skip_gate flag (e.g., PMD manual review)
    var pmGateEnabled = agentdesk.config.get("pm_decision_gate_enabled");
    if (dispatchContext.skip_gate) {
      agentdesk.log.info("[pm-gate] Skipped for card " + card.id + " (skip_gate flag)");
    } else if (pmGateEnabled !== false && pmGateEnabled !== "false") {
      var reasons = [];

      // Check 1: DoD completion
      // Format: { items: ["task1", "task2"], verified: ["task1"] }
      // All items must be in verified to pass.
      if (card.deferred_dod_json) {
        try {
          var dod = JSON.parse(card.deferred_dod_json);
          var items = dod.items || [];
          var verified = dod.verified || [];
          if (items.length > 0) {
            var unverified = 0;
            for (var i = 0; i < items.length; i++) {
              if (verified.indexOf(items[i]) === -1) unverified++;
            }
            if (unverified > 0) {
              reasons.push("DoD 미완료: " + (items.length - unverified) + "/" + items.length);
            }
          }
        } catch (e) { /* parse fail = skip */ }
      }

      // Minimum work duration heuristic was intentionally removed.
      // Unified-thread / turn-bridge completions can legitimately finalize with
      // short measured wall-clock even when real work already happened, which
      // created false PM escalations (#257, #261, #262). PM alerts must be
      // reserved for objective failure signals, not timing heuristics.

      if (reasons.length > 0) {
        // Check if the only failure is DoD — give agent 15 min to complete it
        var dodOnly = reasons.length === 1 && reasons[0].indexOf("DoD 미완료") === 0;
        if (dodOnly) {
          // DoD 미완료만 → awaiting_dod (15분 유예, timeouts.js [D]가 만료 시 dilemma_pending)
          agentdesk.kanban.setStatus(card.id, reviewState);
          agentdesk.kanban.setReviewStatus(card.id, "awaiting_dod", {awaiting_dod_at: "now"});
          // #117: sync canonical review state
          agentdesk.reviewState.sync(card.id, "awaiting_dod");
          agentdesk.log.warn("[pm-gate] Card " + card.id + " → review(awaiting_dod): " + reasons[0]);
          return;
        }
        // Other gate failures → dilemma_pending
        var gateReason = reasons.join("; ");
        escalateToManualIntervention(card.id, gateReason, { review: true });
        agentdesk.log.warn("[pm-gate] Card " + card.id + " → dilemma_pending: " + gateReason);
        return;
      }
    }

    // ── Gate passed → always review (counter-model review) ──
    agentdesk.kanban.setStatus(card.id, reviewState);
    agentdesk.log.info("[kanban] " + card.id + " → " + reviewState);
  },

  // ── Card Transition — side effects ────────────────────────
  onCardTransition: function(payload) {
    agentdesk.log.info("[kanban] card " + payload.card_id + ": " + payload.from + " → " + payload.to);
    if (agentdesk.quality && typeof agentdesk.quality.emit === "function") {
      var qualityCardRows = agentdesk.db.query(
        "SELECT assigned_agent_id, latest_dispatch_id FROM kanban_cards WHERE id = ?",
        [payload.card_id]
      );
      var qualityCard = qualityCardRows.length > 0 ? qualityCardRows[0] : {};
      emitQualityEvent({
        event_type: "card_transitioned",
        source_event_id: payload.card_id + ":" + payload.to,
        correlation_id: qualityCard.latest_dispatch_id || payload.card_id,
        agent_id: qualityCard.assigned_agent_id || null,
        card_id: payload.card_id,
        dispatch_id: qualityCard.latest_dispatch_id || null,
        payload: {
          from: payload.from || null,
          to: payload.to || null
        }
      });
    }
    var cfg = agentdesk.pipeline.resolveForCard(payload.card_id);
    var initialState = agentdesk.pipeline.kickoffState(cfg);

    // → initialState (requested): run preflight validation (#256)
    // #255: requested is a dispatch-free preflight state. Dispatch is created separately
    // by auto-queue, which triggers DispatchAttached to advance requested → in_progress.
    if (payload.to === initialState && payload.from !== initialState) {
      var metaBeforePreflight = _loadCardMetadata(payload.card_id);
      if (
        metaBeforePreflight.skip_preflight_once === "api_reopen" ||
        metaBeforePreflight.skip_preflight_once === "pmd_reopen"
      ) {
        delete metaBeforePreflight.skip_preflight_once;
        metaBeforePreflight.preflight_status = "skipped";
        metaBeforePreflight.preflight_summary = "Skipped for API reopen";
        metaBeforePreflight.preflight_checked_at = new Date().toISOString();
        _writeCardMetadata(payload.card_id, metaBeforePreflight);
        agentdesk.log.info("[preflight] Skipped for API reopen: " + payload.card_id);
        return;
      }

      var preflight = _runPreflight(payload.card_id);
      // Store preflight result in metadata without clobbering unrelated keys.
      _mergeCardMetadata(payload.card_id, {
        preflight_status: preflight.status,
        preflight_summary: preflight.summary,
        preflight_checked_at: new Date().toISOString()
      });

      if (preflight.status === "invalid" || preflight.status === "already_applied") {
        // #2051 Finding 2 (P1): resolve terminal state dynamically from the
        // effective pipeline instead of hardcoding "done". Custom pipelines
        // may use different terminal labels (e.g. "completed", "closed"); a
        // literal "done" force-transition would bypass terminal cleanup hooks
        // (`ClearTerminalFields`, `SyncAutoQueue`) when the terminal name
        // differs.
        var preflightCfg = agentdesk.pipeline.resolveForCard(payload.card_id);
        var preflightTerminal = agentdesk.pipeline.terminalState(preflightCfg) || "done";
        // Move to terminal state without implementation dispatch
        agentdesk.kanban.setStatus(payload.card_id, preflightTerminal, true); // force
        // Clean up any auto-queue entries so the run doesn't stall
        var pendingEntries = agentdesk.db.query(
          "SELECT id FROM auto_queue_entries WHERE kanban_card_id = ? AND status = 'pending'",
          [payload.card_id]
        );
        for (var pi = 0; pi < pendingEntries.length; pi++) {
          agentdesk.autoQueue.updateEntryStatus(
            pendingEntries[pi].id,
            "skipped",
            "preflight_invalid"
          );
        }
        agentdesk.log.info("[preflight] Card " + payload.card_id + " → done (" + preflight.status + "): " + preflight.summary);
      } else if (preflight.status === "consult_required") {
        // Store consultation status — auto-queue tick will handle consultation dispatch creation
        agentdesk.log.info("[preflight] Card " + payload.card_id + " needs consultation: " + preflight.summary);
      }
      // "clear" and "assumption_ok" → do nothing, auto-queue will create implementation dispatch
    }
  },

  // ── Terminal state ────────────────────────────────────────
  // Auto-queue entry marking and next-item activation are handled by:
  //   1. Rust transition_status() — marks entries as done (authoritative)
  //   2. auto-queue.js onCardTerminal — dispatches next entry (single path, #110)
  // kanban-rules does NOT touch auto_queue_entries to avoid triple-update conflicts.
  onCardTerminal: function(payload) {
    agentdesk.log.info("[kanban] card " + payload.card_id + " reached terminal: " + payload.status);
    var cfg = agentdesk.pipeline.resolveForCard(payload.card_id);
    var terminalState = agentdesk.pipeline.terminalState(cfg);

    if (payload.status === terminalState) {
      agentdesk.db.execute(
        "UPDATE kanban_cards SET completed_at = datetime('now') WHERE id = ? AND completed_at IS NULL",
        [payload.card_id]
      );

      // #401: Auto-merge now handled by merge-automation.js (direct merge + PR fallback)

      var retrospectiveResult = agentdesk.runtime.recordCardRetrospective(
        payload.card_id,
        payload.status
      );
      if (retrospectiveResult && retrospectiveResult.error) {
        agentdesk.log.warn(
          "[kanban] retrospective record failed for " +
          payload.card_id +
          ": " +
          retrospectiveResult.error
        );
        // #2051 Finding 25 (P3): retrospective data feeds the QA/quality
        // dashboards. Silent log-only failures previously meant operators only
        // learnt about gaps from downstream dashboards going stale. Emit a
        // supervisor signal so the failure is observable end-to-end.
        try {
          if (agentdesk.runtime && typeof agentdesk.runtime.emitSignal === "function") {
            agentdesk.runtime.emitSignal("retrospective_record_failed", {
              card_id: payload.card_id,
              status: payload.status,
              error: String(retrospectiveResult.error)
            });
          }
        } catch (signalErr) {
          agentdesk.log.warn(
            "[kanban] retrospective_record_failed signal emit failed for " +
            payload.card_id + ": " + signalErr
          );
        }
      }
    }
  }
};

if (typeof agentdesk !== "undefined" && agentdesk && typeof agentdesk.registerPolicy === "function") {
  agentdesk.registerPolicy(rules);
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = {
    policy: rules,
    _loadCardAlertContext: _loadCardAlertContext,
    __test: {
      runPreflight: _runPreflight,
      _loadCardAlertContext: _loadCardAlertContext
    }
  };
}
