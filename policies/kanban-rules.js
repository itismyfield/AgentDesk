/**
 * kanban-rules.js — ADK Policy: Core Kanban Lifecycle
 * priority: 10 (runs first)
 *
 * Hooks:
 *   onSessionStatusChange — dispatch session 상태 → card 상태 동기화
 *   onDispatchCompleted   — 완료 검증 (PM Decision Gate) + review 진입
 *   onCardTransition      — 상태별 부수효과 (dispatch 생성, PMD 알림 등)
 *   onCardTerminal        — completed_at 기록 + 자동큐 진행
 */

// ── Helpers ──────────────────────────────────────────────────

function sendDiscordNotification(target, content, bot) {
  try {
    var port = agentdesk.config.get("health_port") || 8798;
    var body = { target: target, content: content, source: "kanban-rules" };
    if (bot) body.bot = bot;
    agentdesk.http.post("http://127.0.0.1:" + port + "/api/send", body);
  } catch (e) {
    agentdesk.log.warn("[kanban] Discord send failed: " + e);
  }
}

function notifyPMD(cardId, reason) {
  var pmdChannel = agentdesk.config.get("kanban_manager_channel_id");
  if (!pmdChannel) {
    agentdesk.log.warn("[pm-gate] No kanban_manager_channel_id configured, skipping PMD notification");
    return;
  }
  var cards = agentdesk.db.query(
    "SELECT title FROM kanban_cards WHERE id = ?", [cardId]
  );
  var title = cards.length > 0 ? cards[0].title : cardId;
  sendDiscordNotification(
    "channel:" + pmdChannel,
    "[PM Decision] " + title + "\n사유: " + reason,
    "announce"
  );
}

// ── Policy ───────────────────────────────────────────────────

var rules = {
  name: "kanban-rules",
  priority: 10,

  // ── Session status → Card status ──────────────────────────
  onSessionStatusChange: function(payload) {
    // Require dispatch_id — sessions without an active dispatch cannot drive card transitions
    if (!payload.dispatch_id) return;

    var cards = agentdesk.db.query(
      "SELECT id, status FROM kanban_cards WHERE latest_dispatch_id = ?",
      [payload.dispatch_id]
    );
    if (cards.length === 0) return;
    var card = cards[0];

    // working → in_progress: only for implementation/rework dispatches
    // Review dispatches should NOT advance the card to in_progress
    if (payload.status === "working" && card.status === "requested") {
      var dispatch = agentdesk.db.query(
        "SELECT dispatch_type, status FROM task_dispatches WHERE id = ?",
        [payload.dispatch_id]
      );
      if (dispatch.length === 0) return;
      var dtype = dispatch[0].dispatch_type;
      // Only implementation and rework dispatches acknowledge work start
      if (dtype === "implementation" || dtype === "rework") {
        agentdesk.kanban.setStatus(card.id, "in_progress");
        agentdesk.log.info("[kanban] " + card.id + " requested → in_progress (ack via " + dtype + " dispatch " + payload.dispatch_id + ")");
      }
    }

    // idle on implementation/rework is handled in Rust hook_session by completing
    // the pending dispatch first, then letting onDispatchCompleted drive review entry.

    // idle + review dispatch → auto-complete with verdict
    // 카운터모델 리뷰 세션이 idle이 되면 디스패치를 자동 완료하고 verdict 전달
    if (payload.status === "idle" && card.status === "review") {
      var dispatch = agentdesk.db.query(
        "SELECT id, dispatch_type, status, result, kanban_card_id FROM task_dispatches WHERE id = ?",
        [payload.dispatch_id]
      );
      if (dispatch.length > 0 && dispatch[0].dispatch_type === "review" && dispatch[0].status === "pending") {
        // ── Verdict extraction (structured, dispatch-correlated) ──
        // Priority: 1) dispatch result JSON  2) GitHub comment with round marker  3) pending_decision
        var verdict = null;
        var resultJson = dispatch[0].result;

        // 1. Check dispatch result (set by /api/review-verdict callback)
        if (resultJson) {
          try {
            var parsed = JSON.parse(resultJson);
            if (parsed.verdict) verdict = parsed.verdict;
          } catch(e) { /* parse fail */ }
        }

        // 2. GitHub comment fallback — filter by current round/dispatch correlation
        if (!verdict) {
          var cardInfo = agentdesk.db.query(
            "SELECT github_issue_url, review_round FROM kanban_cards WHERE id = ?",
            [dispatch[0].kanban_card_id]
          );
          if (cardInfo.length > 0 && cardInfo[0].github_issue_url) {
            var urlMatch = (cardInfo[0].github_issue_url || "").match(/github\.com\/([^/]+\/[^/]+)\/issues\/(\d+)/);
            if (urlMatch) {
              try {
                var round = cardInfo[0].review_round || 1;
                var dispatchId = dispatch[0].id;
                // Filter comments that match current round OR dispatch_id
                // Round marker: "round 1", "R1", "라운드 1" etc.
                // Dispatch marker: dispatch_id substring
                var roundPattern = "round.?" + round + "|R" + round + "|라운드.?" + round + "|" + dispatchId.substring(0, 8);
                var ghOutput = agentdesk.exec("gh", [
                  "issue", "view", urlMatch[2], "--repo", urlMatch[1],
                  "--comments", "--json", "comments", "--jq",
                  "[.comments[].body] | map(select(test(\"" + roundPattern + "\"; \"i\"))) | last"
                ]);
                if (ghOutput && ghOutput.trim()) {
                  var lower = ghOutput.toLowerCase();
                  // Structured verdict markers
                  if (lower.indexOf("verdict: pass") >= 0 || lower.indexOf("verdict: **pass**") >= 0) {
                    verdict = "pass";
                  } else if (lower.indexOf("verdict: improve") >= 0 || lower.indexOf("verdict: **improve**") >= 0) {
                    verdict = "improve";
                  } else if (lower.indexOf("✅") >= 0 && lower.indexOf("accept") >= 0) {
                    verdict = "pass";
                  } else if (lower.indexOf("보완 필요") >= 0 || lower.indexOf("한 번 더") >= 0) {
                    verdict = "improve";
                  }
                }
              } catch(e) {
                agentdesk.log.warn("[kanban] GitHub comment parsing failed: " + e);
              }
            }
          }
        }

        // 3. No verdict found → pending_decision (never default to pass)
        if (!verdict) {
          agentdesk.kanban.setStatus(card.id, "pending_decision");
          agentdesk.db.execute(
            "UPDATE kanban_cards SET blocked_reason = 'Review completed but verdict unclear — manual decision needed' WHERE id = ?",
            [card.id]
          );
          agentdesk.log.warn("[kanban] review dispatch " + payload.dispatch_id + " — no clear verdict, → pending_decision");
          return;
        }

        // 디스패치 completed 처리
        agentdesk.db.execute(
          "UPDATE task_dispatches SET status = 'completed', result = ?, updated_at = datetime('now') WHERE id = ?",
          [JSON.stringify({ verdict: verdict, auto_completed: true, source: "github_comment" }), payload.dispatch_id]
        );
        agentdesk.log.info("[kanban] review dispatch " + payload.dispatch_id + " auto-completed with verdict: " + verdict);
      }
    }
  },

  // ── Dispatch Completed — PM Decision Gate ─────────────────
  onDispatchCompleted: function(payload) {
    var dispatches = agentdesk.db.query(
      "SELECT id, kanban_card_id, to_agent_id, dispatch_type, chain_depth, created_at FROM task_dispatches WHERE id = ?",
      [payload.dispatch_id]
    );
    if (dispatches.length === 0) return;
    var dispatch = dispatches[0];
    if (!dispatch.kanban_card_id) return;

    var cards = agentdesk.db.query(
      "SELECT id, status, priority, assigned_agent_id, deferred_dod_json FROM kanban_cards WHERE id = ?",
      [dispatch.kanban_card_id]
    );
    if (cards.length === 0) return;
    var card = cards[0];

    // Skip terminal cards
    if (card.status === "done") return;

    // Review/decision dispatches — handled by review-automation policy
    if (dispatch.dispatch_type === "review" || dispatch.dispatch_type === "review-decision") return;

    // Rework dispatches — skip gate, go directly to review
    if (dispatch.dispatch_type === "rework") {
      agentdesk.kanban.setStatus(card.id, "review");
      agentdesk.log.info("[kanban] " + card.id + " rework done → review");
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
    var dispatchContext = {};
    try { dispatchContext = JSON.parse(dispatch.context || "{}"); } catch(e) {}
    var pmGateEnabled = agentdesk.config.get("pm_decision_gate_enabled");
    if (dispatchContext.skip_gate) {
      agentdesk.log.info("[pm-gate] Skipped for card " + card.id + " (skip_gate flag)");
    } else if (pmGateEnabled !== false && pmGateEnabled !== "false") {
      var reasons = [];

      // Check 1: DoD completion
      if (card.deferred_dod_json) {
        try {
          var dod = JSON.parse(card.deferred_dod_json);
          if (Array.isArray(dod)) {
            var checked = 0;
            for (var i = 0; i < dod.length; i++) {
              if (dod[i].done || dod[i].checked) checked++;
            }
            if (checked < dod.length) {
              reasons.push("DoD 미완료: " + checked + "/" + dod.length);
            }
          }
        } catch (e) { /* parse fail = skip */ }
      }

      // Check 2: Minimum work duration (2 min)
      var MIN_WORK_SEC = 120;
      var sessions = agentdesk.db.query(
        "SELECT MIN(started_at) as first_work, MAX(last_seen_at) as last_seen " +
        "FROM dispatched_sessions WHERE active_dispatch_id = ? AND status = 'working'",
        [dispatch.id]
      );
      if (sessions.length > 0 && sessions[0].first_work && sessions[0].last_seen) {
        var durationSec = (new Date(sessions[0].last_seen) - new Date(sessions[0].first_work)) / 1000;
        if (durationSec < MIN_WORK_SEC) {
          reasons.push("작업 시간 부족: " + Math.round(durationSec) + "초 (최소 " + MIN_WORK_SEC + "초)");
        }
      }

      if (reasons.length > 0) {
        // Gate failed → pending_decision
        agentdesk.kanban.setStatus(card.id, "pending_decision");
        agentdesk.db.execute(
          "UPDATE kanban_cards SET review_status = NULL WHERE id = ?",
          [card.id]
        );
        agentdesk.log.warn("[pm-gate] Card " + card.id + " → pending_decision: " + reasons.join("; "));
        notifyPMD(card.id, reasons.join("; "));
        return;
      }
    }

    // ── Gate passed → always review (counter-model review) ──
    agentdesk.kanban.setStatus(card.id, "review");
    agentdesk.log.info("[kanban] " + card.id + " → review");
  },

  // ── Card Transition — side effects ────────────────────────
  onCardTransition: function(payload) {
    agentdesk.log.info("[kanban] card " + payload.card_id + ": " + payload.from + " → " + payload.to);

    // → requested: auto-create dispatch
    if (payload.to === "requested" && payload.from !== "requested") {
      var cards = agentdesk.db.query(
        "SELECT assigned_agent_id, title, latest_dispatch_id FROM kanban_cards WHERE id = ?",
        [payload.card_id]
      );
      if (cards.length > 0 && cards[0].assigned_agent_id) {
        var existingDispatch = cards[0].latest_dispatch_id
          ? agentdesk.db.query("SELECT status FROM task_dispatches WHERE id = ?", [cards[0].latest_dispatch_id])
          : [];
        var alreadyPending = existingDispatch.length > 0 && existingDispatch[0].status === "pending";

        if (!alreadyPending) {
          try {
            var dispatchId = agentdesk.dispatch.create(
              payload.card_id,
              cards[0].assigned_agent_id,
              "implementation",
              cards[0].title
            );
            agentdesk.log.info("[kanban] dispatch created: " + dispatchId);
            // Discord notification is handled by the Rust handler (async send_dispatch_to_discord)
            // to avoid ureq deadlock on tokio runtime.
          } catch (e) {
            agentdesk.log.warn("[kanban] dispatch creation failed: " + e);
          }
        }
      } else {
        agentdesk.log.warn("[kanban] card " + payload.card_id + " has no assignee — dispatch skipped");
      }
    }

    // → blocked: PMD 알림 (Agent in the Loop)
    if (payload.to === "blocked") {
      var reason = "상태 전이: " + payload.from + " → " + payload.to;

      // blocked_reason이 있으면 사용
      var blockInfo = agentdesk.db.query(
        "SELECT blocked_reason FROM kanban_cards WHERE id = ?",
        [payload.card_id]
      );
      if (blockInfo.length > 0 && blockInfo[0].blocked_reason) {
        reason = blockInfo[0].blocked_reason;
      }

      notifyPMD(payload.card_id, reason);
    }

    // → pending_decision: create pm-decision dispatch + notify PMD
    if (payload.to === "pending_decision") {
      var blockInfo = agentdesk.db.query(
        "SELECT blocked_reason, assigned_agent_id, title FROM kanban_cards WHERE id = ?",
        [payload.card_id]
      );
      var reason = "PM 결정 필요";
      var agentId = "";
      var title = payload.card_id;
      if (blockInfo.length > 0) {
        reason = blockInfo[0].blocked_reason || reason;
        agentId = blockInfo[0].assigned_agent_id || "";
        title = blockInfo[0].title || title;
      }

      // Create pm-decision dispatch to PMD
      var pmdChannel = agentdesk.config.get("kanban_manager_channel_id");
      if (pmdChannel) {
        // Find PMD agent by channel
        var pmdAgent = agentdesk.db.query(
          "SELECT id FROM agents WHERE discord_channel_id = ? OR discord_channel_alt = ? LIMIT 1",
          [pmdChannel, pmdChannel]
        );
        var pmdAgentId = pmdAgent.length > 0 ? pmdAgent[0].id : agentId;
        try {
          agentdesk.dispatch.create(
            payload.card_id,
            pmdAgentId,
            "pm-decision",
            "[PM Decision] " + title
          );
        } catch (e) {
          agentdesk.log.warn("[kanban] pm-decision dispatch failed: " + e);
        }
      }

      notifyPMD(payload.card_id, reason);
    }
  },

  // ── Terminal state ────────────────────────────────────────
  onCardTerminal: function(payload) {
    agentdesk.log.info("[kanban] card " + payload.card_id + " reached terminal: " + payload.status);

    if (payload.status === "done") {
      agentdesk.db.execute(
        "UPDATE kanban_cards SET completed_at = datetime('now') WHERE id = ? AND completed_at IS NULL",
        [payload.card_id]
      );
    }
  }
};

agentdesk.registerPolicy(rules);
