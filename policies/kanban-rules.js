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

// #401: Auto-merge worktree branch into main on card done.
// Finds the most recent completed dispatch for the card, extracts
// completed_branch from the result, and merges it into main.
// On conflict, creates a PR as fallback.
function _autoMergeWorktreeBranch(cardId) {
  var dispatches = agentdesk.db.query(
    "SELECT result FROM task_dispatches " +
    "WHERE kanban_card_id = ? AND status = 'completed' AND result IS NOT NULL " +
    "ORDER BY completed_at DESC LIMIT 1",
    [cardId]
  );
  if (dispatches.length === 0) return;

  var result;
  try { result = JSON.parse(dispatches[0].result); } catch(e) { return; }

  var branch = result.completed_branch;
  if (!branch) return;

  // Skip non-worktree branches (e.g. main, feat/*)
  if (branch === "main" || branch === "master") return;

  // Get card info for PR title
  var cards = agentdesk.db.query(
    "SELECT title, github_issue_number, repo_id FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  var card = cards.length > 0 ? cards[0] : {};
  var repoDir = result.completed_worktree_path;

  // Find the main workspace repo FIRST (worktree may already be cleaned up)
  var mainRepo = agentdesk.config.get("workspace_root");
  if (!mainRepo) {
    // Fallback: derive from worktree path
    var wtIdx = (repoDir || "").indexOf("/worktrees/");
    if (wtIdx > 0) {
      mainRepo = repoDir.substring(0, wtIdx) + "/workspaces/agentdesk";
    }
  }
  if (!mainRepo) {
    agentdesk.log.warn("[kanban] #401: cannot determine main repo path — skip merge");
    return;
  }

  // Check if branch has commits ahead of main (use mainRepo, worktree may be gone)
  var gitDir = repoDir || mainRepo;
  var logResult = agentdesk.exec("git", ["-C", gitDir, "log", "main.." + branch, "--oneline"]);
  if ((!logResult || logResult.trim() === "" || logResult.indexOf("ERROR") === 0) && gitDir !== mainRepo) {
    // Worktree gone — retry from main repo where branch ref still exists
    logResult = agentdesk.exec("git", ["-C", mainRepo, "log", "main.." + branch, "--oneline"]);
  }
  if (!logResult || logResult.trim() === "" || logResult.indexOf("ERROR") === 0) {
    agentdesk.log.info("[kanban] #401: no commits in " + branch + " — skip merge");
    return;
  }

  // Try merge
  agentdesk.log.warn("[kanban] #401: merging " + branch + " into main (mainRepo=" + mainRepo + ", repoDir=" + repoDir + ")");
  var mergeResult = agentdesk.exec("git", [
    "-C", mainRepo,
    "merge", branch, "--no-edit"
  ]);
  agentdesk.log.warn("[kanban] #401: merge result for " + branch + ": " + JSON.stringify(mergeResult));

  if (mergeResult && mergeResult.indexOf("CONFLICT") >= 0) {
    // Abort failed merge
    agentdesk.exec("git", ["-C", mainRepo, "merge", "--abort"]);
    agentdesk.log.warn("[kanban] #401: merge conflict on " + branch + " — creating PR");

    // Push branch and create PR
    agentdesk.exec("git", ["-C", mainRepo, "push", "origin", branch]);
    var issueNum = card.github_issue_number || "";
    var repo = card.repo_id || "";
    if (repo) {
      agentdesk.exec("gh", [
        "pr", "create",
        "--repo", repo,
        "--head", branch,
        "--title", "#" + issueNum + " " + (card.title || branch),
        "--body", "Auto-generated PR from worktree merge conflict.\n\nResolve conflicts and merge manually."
      ]);
    }
  } else {
    agentdesk.log.info("[kanban] #401: merged " + branch + " into main successfully");
    // Push main
    agentdesk.exec("git", ["-C", mainRepo, "push", "origin", "main"]);
    if (repoDir && repoDir !== mainRepo && repoDir.indexOf("/worktrees/") >= 0) {
      var mainRepoFromGit = agentdesk.exec("git", [
        "-C", repoDir, "rev-parse", "--path-format=absolute", "--git-common-dir"
      ]);
      if (mainRepoFromGit && mainRepoFromGit.indexOf("ERROR") !== 0) {
        mainRepo = mainRepoFromGit.replace(/\/.git\/?$/, "");
      }
      agentdesk.exec("git", ["-C", mainRepo, "worktree", "remove", repoDir, "--force"]);
      agentdesk.exec("git", ["-C", mainRepo, "branch", "-D", branch]);
      agentdesk.log.info("[kanban] #401: cleaned up worktree " + repoDir + " and branch " + branch);
    }
  }
}

function sendDiscordNotification(target, content, bot) {
  agentdesk.message.queue(target, content, bot || "announce", "system");
}

function notifyPMD(cardId, reason) {
  escalate(cardId, reason);
}

// ── Preflight helpers (#256) ─────────────────────────────────

function _extractRepoFromUrl(url) {
  if (!url) return null;
  var match = url.match(/github\.com\/([^\/]+\/[^\/]+)/);
  return match ? match[1] : null;
}

function _loadCardMetadata(cardId) {
  var rows = agentdesk.db.query(
    "SELECT metadata FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (rows.length === 0 || !rows[0].metadata) return {};
  try {
    var parsed = JSON.parse(rows[0].metadata);
    return parsed && typeof parsed === "object" ? parsed : {};
  } catch (e) {
    return {};
  }
}

function _mergeCardMetadata(cardId, patch) {
  var meta = _loadCardMetadata(cardId);
  for (var key in patch) {
    if (Object.prototype.hasOwnProperty.call(patch, key)) {
      meta[key] = patch[key];
    }
  }
  agentdesk.db.execute(
    "UPDATE kanban_cards SET metadata = ? WHERE id = ?",
    [JSON.stringify(meta), cardId]
  );
  return meta;
}

function _findAutoQueueEntriesByDispatch(dispatchId, liveOnly) {
  var statusClause = liveOnly
    ? "e.status IN ('pending', 'dispatched')"
    : "e.status = 'dispatched'";
  return agentdesk.db.query(
    "SELECT DISTINCT e.id, e.agent_id FROM auto_queue_entries e " +
    "LEFT JOIN auto_queue_entry_dispatch_history h " +
    "  ON h.entry_id = e.id AND h.dispatch_id = ? " +
    "WHERE " + statusClause + " " +
    "  AND (e.dispatch_id = ? OR h.dispatch_id IS NOT NULL)",
    [dispatchId, dispatchId]
  );
}

function _runPreflight(cardId) {
  var card = agentdesk.db.query(
    "SELECT kc.id, kc.title, kc.github_issue_number, kc.github_issue_url, kc.status, kc.description, " +
    "kc.assigned_agent_id, kc.metadata, kc.blocked_reason " +
    "FROM kanban_cards kc WHERE kc.id = ?",
    [cardId]
  );
  if (card.length === 0) return { status: "invalid", summary: "Card not found" };
  var c = card[0];

  // Check 1: GitHub issue closed? (uses gh CLI since no bridge exists)
  if (c.github_issue_number && c.github_issue_url) {
    var repo = _extractRepoFromUrl(c.github_issue_url);
    if (repo) {
      try {
        var ghOutput = agentdesk.exec("gh", [
          "issue", "view", String(c.github_issue_number),
          "--repo", repo, "--json", "state", "--jq", ".state"
        ]);
        if (ghOutput && ghOutput.trim() === "CLOSED") {
          return { status: "already_applied", summary: "GitHub issue #" + c.github_issue_number + " is closed" };
        }
      } catch (e) {
        // GitHub CLI not available or failed, skip check
      }
    }
  }

  // Check 2: Already has terminal dispatch?
  var terminalDispatch = agentdesk.db.query(
    "SELECT id, status FROM task_dispatches WHERE kanban_card_id = ? AND dispatch_type = 'implementation' AND status = 'completed'",
    [cardId]
  );
  if (terminalDispatch.length > 0) {
    return { status: "already_applied", summary: "Implementation dispatch already completed" };
  }

  // Check 3: Description/body too short or empty?
  var body = c.description || "";
  if (body.trim().length < 30) {
    return { status: "consult_required", summary: "Issue body is too short or empty — needs clarification" };
  }

  // Check 4: No DoD section?
  if (body.indexOf("DoD") === -1 && body.indexOf("Definition of Done") === -1 && body.indexOf("완료 기준") === -1) {
    return { status: "assumption_ok", summary: "No explicit DoD found, assuming spec is sufficient" };
  }

  // All checks passed
  return { status: "clear", summary: "Preflight checks passed" };
}

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
    if (payload.status !== "working") {
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
    var card = cards[0];
    var cfg = agentdesk.pipeline.resolveForCard(card.id);
    var initialState = agentdesk.pipeline.kickoffState(cfg);
    var nextFromInitial = agentdesk.pipeline.nextGatedTarget(initialState, cfg);

    // working → nextFromInitial: only for implementation/rework dispatches
    // Review dispatches should NOT advance the card to in_progress
    if (payload.status === "working" && card.status === initialState) {
      var dispatch = agentdesk.db.query(
        "SELECT dispatch_type, status FROM task_dispatches WHERE id = ?",
        [payload.dispatch_id]
      );
      if (dispatch.length === 0) return;
      var dtype = dispatch[0].dispatch_type;
      // Only implementation and rework dispatches acknowledge work start
      if (dtype === "implementation" || dtype === "rework") {
        agentdesk.kanban.setStatus(card.id, nextFromInitial);
        agentdesk.log.info("[kanban] " + card.id + " " + initialState + " → " + nextFromInitial + " (ack via " + dtype + " dispatch " + payload.dispatch_id + ")");
      }
    }

    // idle on implementation/rework is handled in Rust hook_session by completing
    // the pending dispatch first, then letting onDispatchCompleted drive review entry.

    // idle + review dispatch → auto-complete is handled by Rust
    // (dispatched_sessions.rs idle auto-complete → complete_dispatch → OnDispatchCompleted).
    // Previously this JS policy also auto-completed review dispatches via direct DB UPDATE,
    // causing double processing (JS verdict extraction + Rust OnDispatchCompleted).
    // Now only Rust handles auto-complete; JS policy reacts via onDispatchCompleted hook.
    var reviewState = agentdesk.pipeline.nextGatedTarget(nextFromInitial, cfg);
    var forceTargets = agentdesk.pipeline.forceOnlyTargets(nextFromInitial, cfg);
    var pendingState = forceTargets[0];

    if (false && payload.status === "idle" && card.status === reviewState) {
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
                agentdesk.log.info("[kanban-debug] gh comment output for dispatch " + payload.dispatch_id + ": " + (ghOutput || "(empty)").substring(0, 300));
                if (ghOutput && ghOutput.trim()) {
                  var lower = ghOutput.toLowerCase();
                  // Structured verdict markers
                  if (lower.indexOf("verdict: pass") >= 0 || lower.indexOf("verdict: **pass**") >= 0) {
                    verdict = "pass";
                    agentdesk.log.info("[kanban-debug] MATCHED verdict:pass from comment");
                  } else if (lower.indexOf("verdict: improve") >= 0 || lower.indexOf("verdict: **improve**") >= 0) {
                    verdict = "improve";
                    agentdesk.log.info("[kanban-debug] MATCHED verdict:improve from comment");
                  } else if (lower.indexOf("✅") >= 0 && lower.indexOf("accept") >= 0) {
                    verdict = "pass";
                    agentdesk.log.info("[kanban-debug] MATCHED ✅+accept from comment");
                  } else if (lower.indexOf("보완 필요") >= 0 || lower.indexOf("한 번 더") >= 0) {
                    verdict = "improve";
                    agentdesk.log.info("[kanban-debug] MATCHED 보완필요 from comment");
                  } else {
                    agentdesk.log.info("[kanban-debug] NO verdict match in comment");
                  }
                } else {
                  agentdesk.log.info("[kanban-debug] gh comment output empty — no match");
                }
              } catch(e) {
                agentdesk.log.warn("[kanban] GitHub comment parsing failed: " + e);
              }
            }
          }
        }

        // 3. No verdict found → pendingState (never default to pass)
        if (!verdict) {
          agentdesk.kanban.setStatus(card.id, pendingState);
          agentdesk.db.execute(
            "UPDATE kanban_cards SET blocked_reason = 'Review completed but verdict unclear — manual decision needed' WHERE id = ?",
            [card.id]
          );
          agentdesk.log.warn("[kanban] review dispatch " + payload.dispatch_id + " — no clear verdict, → " + pendingState);
          return;
        }

        // 디스패치 completed 처리
        var mcResult = agentdesk.dispatch.markCompleted(payload.dispatch_id, JSON.stringify({ verdict: verdict, auto_completed: true, source: "github_comment" }));
        if (mcResult.rows_affected === 0) {
          agentdesk.log.info("[kanban] dispatch " + payload.dispatch_id + " already terminal, skipping auto-complete");
          return;
        }
        agentdesk.log.info("[kanban] review dispatch " + payload.dispatch_id + " auto-completed with verdict: " + verdict);
      }
    }
  },

  // ── Dispatch Completed — PM Decision Gate ─────────────────
  onDispatchCompleted: function(payload) {
    var dispatches = agentdesk.db.query(
      "SELECT id, kanban_card_id, to_agent_id, dispatch_type, chain_depth, created_at, result, context FROM task_dispatches WHERE id = ?",
      [payload.dispatch_id]
    );
    if (dispatches.length === 0) return;
    var dispatch = dispatches[0];
    var dispatchContext = {};
    try { dispatchContext = JSON.parse(dispatch.context || "{}"); } catch (e) { dispatchContext = {}; }
    if (dispatchContext.phase_gate) return;
    if (!dispatch.kanban_card_id) return;

    var cards = agentdesk.db.query(
      "SELECT id, title, status, priority, assigned_agent_id, deferred_dod_json FROM kanban_cards WHERE id = ?",
      [dispatch.kanban_card_id]
    );
    if (cards.length === 0) return;
    var card = cards[0];
    var cfg = agentdesk.pipeline.resolveForCard(card.id);
    var inProgressState = agentdesk.pipeline.nextGatedTarget(agentdesk.pipeline.kickoffState(cfg), cfg);
    var reviewState = agentdesk.pipeline.nextGatedTarget(inProgressState, cfg);
    var forceTargets = agentdesk.pipeline.forceOnlyTargets(inProgressState, cfg);
    var pendingState = forceTargets[0];

    // Skip terminal cards
    if (agentdesk.pipeline.isTerminal(card.status, cfg)) return;

    // Review/decision dispatches — handled by review-automation policy
    if (dispatch.dispatch_type === "review" || dispatch.dispatch_type === "review-decision") return;

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
      // implementation dispatch. Otherwise escalate to pending_decision.
      if (consultResult.verdict === "clear" || consultResult.verdict === "proceed") {
        meta.preflight_status = "clear";
        meta.preflight_summary = "Consultation resolved: " + (consultResult.summary || "clarified");
        agentdesk.db.execute(
          "UPDATE kanban_cards SET metadata = ? WHERE id = ?",
          [JSON.stringify(meta), dispatch.kanban_card_id]
        );
        var aqEntries = _findAutoQueueEntriesByDispatch(dispatch.id, false);
        if (aqEntries.length > 0) {
          try {
            var nextDispatchId = agentdesk.dispatch.create(
              dispatch.kanban_card_id,
              aqEntries[0].agent_id,
              "implementation",
              card.title || "Implementation",
              {
                auto_queue: true,
                entry_id: aqEntries[0].id,
                parent_dispatch_id: dispatch.id
              }
            );
            if (nextDispatchId) {
              agentdesk.autoQueue.updateEntryStatus(
                aqEntries[0].id,
                "dispatched",
                "consultation_resume",
                { dispatchId: nextDispatchId }
              );
              agentdesk.log.info("[preflight] Consultation resolved for " + dispatch.kanban_card_id + " — resumed implementation dispatch " + nextDispatchId);
            }
          } catch (e) {
            agentdesk.log.warn("[preflight] Consultation resolved for " + dispatch.kanban_card_id + " but implementation redispatch failed: " + e);
          }
        } else {
          agentdesk.log.info("[preflight] Consultation resolved for " + dispatch.kanban_card_id + " → clear");
        }
      } else {
        meta.preflight_status = "escalated";
        meta.preflight_summary = "Consultation did not resolve: " + (consultResult.summary || "still ambiguous");
        agentdesk.db.execute(
          "UPDATE kanban_cards SET metadata = ?, blocked_reason = ? WHERE id = ?",
          [JSON.stringify(meta), "Consultation did not resolve ambiguity", dispatch.kanban_card_id]
        );
        agentdesk.kanban.setStatus(dispatch.kanban_card_id, pendingState);
        agentdesk.log.warn("[preflight] Consultation unresolved for " + dispatch.kanban_card_id + " → " + pendingState);
      }
      return;
    }

    var workResult = {};
    try { workResult = JSON.parse(dispatch.result || "{}"); } catch(e) {}
    if ((dispatch.dispatch_type === "implementation" || dispatch.dispatch_type === "rework")
        && workResult.work_outcome === "noop") {
      var noopMeta = _loadCardMetadata(dispatch.kanban_card_id);
      noopMeta.work_resolution_status = "noop";
      noopMeta.work_resolution_result = workResult;
      var noopCardStatusTarget = workResult.card_status_target || "ready";
      agentdesk.db.execute(
        "UPDATE kanban_cards SET metadata = ?, blocked_reason = NULL WHERE id = ?",
        [JSON.stringify(noopMeta), dispatch.kanban_card_id]
      );
      var noopEntries = _findAutoQueueEntriesByDispatch(dispatch.id, true);
      for (var ne = 0; ne < noopEntries.length; ne++) {
        agentdesk.autoQueue.updateEntryStatus(
          noopEntries[ne].id,
          "done",
          "dispatch_noop"
        );
      }
      agentdesk.kanban.setReviewStatus(card.id, null, {suggestion_pending_at: null, awaiting_dod_at: null});
      agentdesk.reviewState.sync(card.id, "idle");
      agentdesk.kanban.setStatus(card.id, noopCardStatusTarget, true);
      agentdesk.log.info("[kanban] " + card.id + " " + dispatch.dispatch_type + " noop → " + noopCardStatusTarget + " (auto-queue done)");
      return;
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
          // DoD 미완료만 → awaiting_dod (15분 유예, timeouts.js [D]가 만료 시 pendingState)
          agentdesk.kanban.setStatus(card.id, reviewState);
          agentdesk.kanban.setReviewStatus(card.id, "awaiting_dod", {awaiting_dod_at: "now"});
          // #117: sync canonical review state
          agentdesk.reviewState.sync(card.id, "awaiting_dod");
          agentdesk.log.warn("[pm-gate] Card " + card.id + " → review(awaiting_dod): " + reasons[0]);
          return;
        }
        // Other gate failures → pendingState
        agentdesk.kanban.setStatus(card.id, pendingState);
        agentdesk.kanban.setReviewStatus(card.id, null, {suggestion_pending_at: null});
        // #117: sync canonical review state
        agentdesk.reviewState.sync(card.id, "idle");
        agentdesk.log.warn("[pm-gate] Card " + card.id + " → " + pendingState + ": " + reasons.join("; "));
        notifyPMD(card.id, reasons.join("; "));
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
    var cfg = agentdesk.pipeline.resolveForCard(payload.card_id);
    var initialState = agentdesk.pipeline.kickoffState(cfg);
    var inProgressForForce = agentdesk.pipeline.nextGatedTarget(initialState, cfg);
    var blockedTargets = agentdesk.pipeline.forceOnlyTargets(inProgressForForce, cfg);
    var pendingState = blockedTargets[0];

    // → initialState (requested): run preflight validation (#256)
    // #255: requested is a dispatch-free preflight state. Dispatch is created separately
    // by auto-queue, which triggers DispatchAttached to advance requested → in_progress.
    if (payload.to === initialState && payload.from !== initialState) {
      var metaBeforePreflight = _loadCardMetadata(payload.card_id);
      if (metaBeforePreflight.skip_preflight_once === "pmd_reopen") {
        delete metaBeforePreflight.skip_preflight_once;
        metaBeforePreflight.preflight_status = "skipped";
        metaBeforePreflight.preflight_summary = "Skipped for PMD reopen";
        metaBeforePreflight.preflight_checked_at = new Date().toISOString();
        agentdesk.db.execute(
          "UPDATE kanban_cards SET metadata = ? WHERE id = ?",
          [JSON.stringify(metaBeforePreflight), payload.card_id]
        );
        agentdesk.log.info("[preflight] Skipped for PMD reopen: " + payload.card_id);
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
        // Move to done without implementation dispatch
        agentdesk.kanban.setStatus(payload.card_id, "done", true); // force
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

    // → blocked (force-only target): PMD 알림 (Agent in the Loop)
    // "blocked" is a force-only target from in_progress — check all force targets
    var inProgressState = agentdesk.pipeline.nextGatedTarget(initialState, cfg);
    var allForceTargets = agentdesk.pipeline.forceOnlyTargets(inProgressState, cfg);
    // blocked is typically the second force target (index 1)
    var blockedState = allForceTargets.length > 1 ? allForceTargets[1] : allForceTargets[0];
    if (payload.to === blockedState) {
      agentdesk.log.info("[kanban] card " + payload.card_id + " entered blocked state");
    }

    // → pendingState: log only (pm-decision dispatch removed — not effective in practice)
    if (payload.to === pendingState) {
      agentdesk.log.info("[kanban] card " + payload.card_id + " entered pendingState via force-transition");
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

      // #401: Auto-merge worktree branch on done transition
      _autoMergeWorktreeBranch(payload.card_id);

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
      }
    }
  }
};

agentdesk.registerPolicy(rules);
