// ── merge-automation.js ──────────────────────────────────────────────────
// Automates PR merging and worktree cleanup after review passes.
//
// Flow:
//   1. Card reaches terminal (done) → try direct merge from tracked worktree branch
//   2. If direct merge conflicts → create/track PR → CI/merge automation takes over
//   3. Existing tracked PRs in merge state → enable auto-merge
//   4. OnTick5min → detect conflicting PRs → dispatch rebase
//   5. OnTick5min → cleanup merged worktree branches
//
// PR discovery: looks up the card's worktree branch from sessions table,
// then finds the PR by branch name via `gh pr list`.
//
// Config (kv_meta):
//   merge_automation_enabled  — "true" to enable (default: disabled)
//   merge_strategy            — "squash" | "rebase" | "merge" (default: "squash")
//   merge_strategy_mode       — "direct-first" | "pr-always" (default: "direct-first")
//   merge_allowed_authors     — comma-separated GitHub usernames for auto-merge
//                               (e.g. "itismyfield,kunkunGames,bot[bot]")
//
// Manual merge trigger:
//   Set kv: merge_request:{pr_number} = "{owner/repo}"
//   OnTick5min picks it up and merges (no author check — explicit request)

(function() {

var prTracking = agentdesk.prTracking;

var CODEX_REVIEWERS = {
  "chatgpt-codex-connector": true,
  "chatgpt-codex-connector[bot]": true
};
var CODEX_REVIEW_TTL_SECONDS = 14 * 24 * 60 * 60;
var CODEX_NOTIFICATION_TTL_SECONDS = 6 * 60 * 60;

var mergeAutomation = {
  name: "merge-automation",
  priority: 200,  // Run after all other policies

  // ── Card reached terminal → trigger auto-merge ──────────────────────
  onCardTerminal: function(payload) {
    if (!isEnabled()) return;

    var cardId = payload.card_id;

    // #701: Defer cards in the explicit create-pr retry loop. In
    // direct-first mode tryDirectMergeOrTrackPr attempts a direct merge
    // to main BEFORE falling back to PR creation — so letting a
    // pr:create_failed card through here could ship code to main with
    // no PR and no CI. The processTrackedMergeQueue retry loop (fired
    // by onTick5min) handles these rows exclusively.
    var cardRow = agentdesk.db.query(
      "SELECT blocked_reason FROM kanban_cards WHERE id = ?",
      [cardId]
    );
    var tracking = loadTrackedPrForCard(cardId);

    // #743 (C6 degraded path): a pr:create_failed card with NO pr_tracking
    // row has nowhere for processTrackedMergeQueue to pick up the retry —
    // the row got wiped (schema reset, manual cleanup) or was never seeded
    // (pre-handoff crash). Escalate to manual intervention rather than
    // silently stranding the card. The prefix defer below covers the
    // normal case and already-escalated marker.
    if (cardRow.length > 0 && cardRow[0].blocked_reason
        && cardRow[0].blocked_reason.indexOf("pr:create_failed") === 0) {
      if (!tracking && cardRow[0].blocked_reason.indexOf("pr:create_failed_escalated:") !== 0) {
        // escalateToManualIntervention sets blocked_reason to its reason arg,
        // so pass the escalated marker directly — no separate UPDATE needed.
        try {
          escalateToManualIntervention(cardId, "pr:create_failed_escalated:no_tracking");
        } catch (e) {
          agentdesk.log.warn("[merge] escalate failed for card " + cardId + ": " + e);
          agentdesk.db.execute(
            "UPDATE kanban_cards SET blocked_reason = 'pr:create_failed_escalated:no_tracking', " +
            "updated_at = datetime('now') WHERE id = ?",
            [cardId]
          );
        }
        return;
      }
      agentdesk.log.info(
        "[merge] Card " + cardId + " terminal with pr:create_failed marker — deferring to processTrackedMergeQueue retry (skip direct-merge)"
      );
      return;
    }

    // #701/#743: Defer cards still owned by the create-pr retry loop. Before
    // deferring, detect stale pr_tracking (generation mismatch or head_sha
    // divergence) and reseed so the retry loop runs against the current
    // candidate rather than a superseded one.
    if (tracking && tracking.state === "create-pr") {
      var activeGen = loadActiveCreatePrGeneration(cardId);
      if (tracking.dispatch_generation && activeGen
          && tracking.dispatch_generation !== activeGen) {
        agentdesk.log.info(
          "[merge] Card " + cardId + " pr_tracking generation=" + tracking.dispatch_generation +
          " != active dispatch generation=" + activeGen + " — reseeding"
        );
        try {
          agentdesk.reviewAutomation.reseedPrTracking(cardId);
        } catch (e) {
          agentdesk.log.warn("[merge] reseedPrTracking failed for card " + cardId + ": " + e);
        }
        return;
      }
      var latestHead = loadLatestWorkHeadSha(cardId);
      if (tracking.head_sha && latestHead && tracking.head_sha !== latestHead) {
        agentdesk.log.info(
          "[merge] Card " + cardId + " pr_tracking head_sha=" + tracking.head_sha +
          " != latest work head_sha=" + latestHead + " — reseeding"
        );
        try {
          agentdesk.reviewAutomation.reseedPrTracking(cardId);
        } catch (e) {
          agentdesk.log.warn("[merge] reseedPrTracking failed for card " + cardId + ": " + e);
        }
        return;
      }
      agentdesk.log.info(
        "[merge] Card " + cardId + " terminal with pr_tracking state='create-pr' — deferring to processTrackedMergeQueue retry (skip direct-merge)"
      );
      return;
    }

    if (!tracking || !tracking.pr_number || !tracking.repo_id) {
      tryDirectMergeOrTrackPr(cardId, tracking);
      return;
    }

    if (!tracking || tracking.state !== "merge" || !tracking.pr_number || !tracking.repo_id) {
      return;
    }
    agentdesk.log.info("[merge] Card " + cardId + " terminal — checking tracked PR #" + tracking.pr_number);

    // Author check — only auto-merge PRs from allowed authors
    var author = getPrAuthor(tracking.pr_number, tracking.repo_id);
    if (!isAllowedAuthor(author)) {
      agentdesk.log.info("[merge] PR #" + tracking.pr_number + " author '" + author + "' not in allowed list, skipping auto-merge");
      return;
    }

    enableAutoMerge(tracking.pr_number, tracking.repo_id, cardId);
  },

  // ── Periodic: manual merge requests + conflicts + cleanup ────────────
  onTick5min: function() {
    if (!isEnabled()) return;

    processCodexReviewSignals();
    processManualMergeRequests();
    processTrackedMergeQueue();
    cleanupMergedWorktrees();
    detectConflictingPrs();
  }
};

// ── Helpers ───────────────────────────────────────────────────────────

function isEnabled() {
  return agentdesk.config.get("merge_automation_enabled") === "true";
}

function mergeModeStateKey(cardId) {
  return "merge_strategy_mode:card:" + cardId;
}

function normalizeMergeStrategyMode(value) {
  var normalized = String(value || "").trim().toLowerCase();
  return normalized === "pr-always" ? "pr-always" : "direct-first";
}

function getConfiguredMergeStrategyMode() {
  return normalizeMergeStrategyMode(agentdesk.config.get("merge_strategy_mode") || "direct-first");
}

function loadTrackedMergeStrategyMode(cardId) {
  if (!cardId) return null;
  var rows = agentdesk.db.query(
    "SELECT value FROM kv_meta WHERE key = ? LIMIT 1",
    [mergeModeStateKey(cardId)]
  );
  if (rows.length === 0 || !rows[0].value) return null;
  return normalizeMergeStrategyMode(rows[0].value);
}

function persistTrackedMergeStrategyMode(cardId, mode) {
  if (!cardId) return;
  agentdesk.db.execute(
    "INSERT OR REPLACE INTO kv_meta (key, value, expires_at) VALUES (?, ?, NULL)",
    [mergeModeStateKey(cardId), normalizeMergeStrategyMode(mode)]
  );
}

function clearTrackedMergeStrategyMode(cardId) {
  if (!cardId) return;
  agentdesk.db.execute(
    "DELETE FROM kv_meta WHERE key = ?",
    [mergeModeStateKey(cardId)]
  );
}

function resolveTrackedMergeStrategyMode(cardId) {
  return loadTrackedMergeStrategyMode(cardId) || getConfiguredMergeStrategyMode();
}

function sanitizeKvKeyPart(value) {
  return String(value || "").replace(/[^A-Za-z0-9._-]/g, "_");
}

function isCodexReviewer(login) {
  if (!login) return false;
  return !!CODEX_REVIEWERS[String(login).toLowerCase()];
}

function containsBlockingSeverity(text) {
  return /\bP[12]\b/i.test(text || "");
}

function compactWhitespace(text) {
  return String(text || "").replace(/\s+/g, " ").trim();
}

function summarizeInlineText(text) {
  var compact = compactWhitespace(text);
  if (compact.length <= 180) return compact;
  return compact.substring(0, 177) + "...";
}

function extractIssueNumberFromText(text) {
  var match = String(text || "").match(/#(\d+)/);
  return match ? parseInt(match[1], 10) : null;
}

function loadCardContext(cardId) {
  var cards = agentdesk.db.query(
    "SELECT id, status, assigned_agent_id, title, github_issue_number, active_thread_id, repo_id " +
    ", github_issue_url " +
    "FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  return cards.length > 0 ? cards[0] : null;
}

function loadTrackedPrForCard(cardId) {
  return prTracking.load(cardId);
}

// #743: Return the dispatch_generation stamp on the currently-active
// create-pr dispatch (pending or dispatched), or null when none exists.
// Used by onCardTerminal to detect stale pr_tracking rows.
function loadActiveCreatePrGeneration(cardId) {
  var rows = agentdesk.db.query(
    "SELECT json_extract(context, '$.dispatch_generation') AS gen " +
    "FROM task_dispatches " +
    "WHERE kanban_card_id = ? AND dispatch_type = 'create-pr' " +
    "AND status IN ('pending', 'dispatched') " +
    "ORDER BY rowid DESC LIMIT 1",
    [cardId]
  );
  if (rows.length === 0) return null;
  var gen = rows[0].gen;
  return gen ? String(gen) : null;
}

// #743: Return the head_sha of the latest completed implementation/rework
// dispatch, or null. Used to detect head_sha divergence between
// pr_tracking and the candidate commit.
function loadLatestWorkHeadSha(cardId) {
  var target = loadLatestCompletedWorkTarget(cardId);
  return target && target.head_sha ? String(target.head_sha) : null;
}

function loadTrackedPrForRepoPr(repoId, prNumber) {
  return prTracking.findByRepoPr(repoId, prNumber);
}

function upsertPrTracking(cardId, repoId, worktreePath, branch, prNumber, headSha, state, lastError) {
  return prTracking.upsert(cardId, repoId, worktreePath, branch, prNumber, headSha, state, lastError);
}

function listTrackedPrRows(whereClause, params) {
  return prTracking.list(whereClause, params);
}

function findOpenPrByTrackedBranch(repoId, branch) {
  return prTracking.findOpenPrByBranch(repoId, branch);
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

function appendMergeCandidateReason(details, code, message) {
  if (!details) return;
  if (!details.reasons) details.reasons = [];
  details.reasons.push({
    code: code,
    message: message
  });
}

function buildWorkTargetFromDispatchRow(row) {
  var result = parseJsonObject(row.result);
  var context = parseJsonObject(row.context);
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

  return {
    worktree_path: worktreePath,
    branch: branch,
    head_sha: headSha
  };
}

function inspectLatestCompletedWorkTarget(cardId) {
  var inspected = [];
  var cancelledFallbackIndex = -1;
  var excludedRows = agentdesk.db.query(
    "SELECT id, status, result, context FROM task_dispatches " +
    "WHERE kanban_card_id = ? " +
    "AND dispatch_type IN ('implementation', 'rework') " +
    "AND status IN ('pending', 'dispatched', 'cancelled') " +
    "ORDER BY datetime(COALESCE(completed_at, updated_at, created_at)) DESC, rowid DESC LIMIT 8",
    [cardId]
  );
  for (var i = 0; i < excludedRows.length; i++) {
    var excludedRow = excludedRows[i];
    var excludedTarget = buildWorkTargetFromDispatchRow(excludedRow);
    var fallbackEligible = excludedRow.status === "cancelled" && !!excludedTarget.worktree_path;
    inspected.push({
      dispatch_id: excludedRow.id,
      status: excludedRow.status,
      selected: false,
      reason: fallbackEligible
        ? "cancelled dispatch retained as terminal fallback candidate"
        : "status '" + excludedRow.status + "' is not merge-eligible",
      target: excludedTarget
    });
    if (fallbackEligible && cancelledFallbackIndex === -1) {
      cancelledFallbackIndex = inspected.length - 1;
    }
  }

  var rows = agentdesk.db.query(
    "SELECT id, status, result, context FROM task_dispatches " +
    "WHERE kanban_card_id = ? " +
    "AND dispatch_type IN ('implementation', 'rework') " +
    "AND status = 'completed' " +
    "ORDER BY datetime(COALESCE(completed_at, updated_at, created_at)) DESC, rowid DESC LIMIT 8",
    [cardId]
  );

  for (var i = 0; i < rows.length; i++) {
    var row = rows[i];
    var target = buildWorkTargetFromDispatchRow(row);
    var reason = !target.worktree_path && !target.branch && !target.head_sha
      ? "completed dispatch has no worktree_path/branch/head_sha metadata"
      : null;
    inspected.push({
      dispatch_id: row.id,
      status: row.status,
      selected: !reason,
      reason: reason || "selected latest completed work dispatch",
      target: target
    });
    if (reason) {
      continue;
    }
    return { target: target, inspected: inspected };
  }

  if (cancelledFallbackIndex !== -1) {
    inspected[cancelledFallbackIndex].selected = true;
    inspected[cancelledFallbackIndex].reason =
      "selected latest cancelled work dispatch as terminal fallback candidate";
    return {
      target: inspected[cancelledFallbackIndex].target,
      inspected: inspected
    };
  }

  return { target: null, inspected: inspected };
}

function loadLatestCompletedWorkTarget(cardId) {
  return inspectLatestCompletedWorkTarget(cardId).target;
}

function execGitOrThrow(args) {
  var output = agentdesk.exec("git", args);
  if (typeof output === "string" && output.indexOf("ERROR") === 0) {
    throw new Error(output.replace(/^ERROR:\s*/, ""));
  }
  return typeof output === "string" ? output : "";
}

function execGitMaybe(args) {
  var output = agentdesk.exec("git", args);
  if (typeof output === "string" && output.indexOf("ERROR") === 0) {
    return null;
  }
  return typeof output === "string" ? output : "";
}

function parseWorktreeList(text) {
  var entries = [];
  var current = { path: "", branch: null };
  var lines = String(text || "").split(/\r?\n/);
  for (var i = 0; i < lines.length; i++) {
    var line = lines[i];
    if (line.indexOf("worktree ") === 0) {
      if (current.path) entries.push(current);
      current = { path: line.substring("worktree ".length), branch: null };
    } else if (line.indexOf("branch ") === 0) {
      var branch = line.substring("branch ".length);
      current.branch = branch.indexOf("refs/heads/") === 0
        ? branch.substring("refs/heads/".length)
        : branch;
    } else if (!line.trim() && current.path) {
      entries.push(current);
      current = { path: "", branch: null };
    }
  }
  if (current.path) entries.push(current);
  return entries;
}

function maybeRestoreMergeStash(mainWorktreePath, stashCreated) {
  if (!stashCreated) return null;
  var output = agentdesk.exec("git", ["-C", mainWorktreePath, "stash", "pop"]);
  if (typeof output === "string" && output.indexOf("ERROR") === 0) {
    var err = output.replace(/^ERROR:\s*/, "").trim();
    return err
      ? "stash created but restore reported conflicts: " + err
      : "stash created but restore needs manual check";
  }
  return "stash restored";
}

function maybeResetDirectMergeHead(mainWorktreePath, originalHead) {
  if (!originalHead) return null;
  var output = agentdesk.exec("git", ["-C", mainWorktreePath, "reset", "--hard", originalHead]);
  if (typeof output === "string" && output.indexOf("ERROR") === 0) {
    var err = output.replace(/^ERROR:\s*/, "").trim();
    return err
      ? "reset to original main HEAD failed: " + err
      : "reset to original main HEAD failed";
  }
  return "main worktree reset to pre-merge HEAD";
}

function isCherryPickConflict(errorText) {
  return /CONFLICT|could not apply|after resolving the conflicts|merge conflict/i.test(String(errorText || ""));
}

function isPushRejected(errorText) {
  return /rejected|fetch first|non-fast-forward|failed to push some refs/i.test(String(errorText || ""));
}

function retryDirectMergePush(mainWorktreePath, mainBranch) {
  var maxRetries = 3;
  var attempts = 0;
  var lastError = null;

  while (attempts <= maxRetries) {
    var pushOutput = agentdesk.exec("git", ["-C", mainWorktreePath, "push", "origin", mainBranch]);
    if (!(typeof pushOutput === "string" && pushOutput.indexOf("ERROR") === 0)) {
      return {
        ok: true,
        attempts: attempts + 1
      };
    }

    lastError = pushOutput.replace(/^ERROR:\s*/, "");
    if (!isPushRejected(lastError) || attempts === maxRetries) {
      return {
        ok: false,
        conflict: false,
        error: lastError,
        attempts: attempts + 1
      };
    }

    attempts += 1;

    var fetchOutput = agentdesk.exec("git", ["-C", mainWorktreePath, "fetch", "origin", mainBranch]);
    if (typeof fetchOutput === "string" && fetchOutput.indexOf("ERROR") === 0) {
      return {
        ok: false,
        conflict: false,
        error: fetchOutput.replace(/^ERROR:\s*/, ""),
        attempts: attempts
      };
    }

    var rebaseOutput = agentdesk.exec("git", ["-C", mainWorktreePath, "rebase", "origin/" + mainBranch]);
    if (typeof rebaseOutput === "string" && rebaseOutput.indexOf("ERROR") === 0) {
      return {
        ok: false,
        conflict: isCherryPickConflict(rebaseOutput),
        error: rebaseOutput.replace(/^ERROR:\s*/, ""),
        attempts: attempts,
        rebase_failed: true
      };
    }
  }

  return {
    ok: false,
    conflict: false,
    error: lastError || "direct merge push failed",
    attempts: attempts
  };
}

function parsePrNumberFromOutput(output) {
  var match = String(output || "").match(/\/pull\/(\d+)/);
  return match ? parseInt(match[1], 10) : null;
}

function resolveTerminalMergeCandidate(cardId, tracking, details) {
  var card = loadCardContext(cardId);
  if (details) {
    details.card = card;
    details.tracking = tracking || null;
  }
  if (!card) {
    appendMergeCandidateReason(details, "card_not_found", "card not found");
    agentdesk.log.info("[merge] Card " + cardId + " terminal merge skipped: card not found");
    return null;
  }

  var latestWorkInfo = inspectLatestCompletedWorkTarget(cardId);
  var latestWork = latestWorkInfo.target;
  if (details) {
    details.latest_work_dispatches = latestWorkInfo.inspected;
    details.latest_work = latestWork;
  }
  var repoId = firstPresent(
    tracking && tracking.repo_id,
    card.repo_id,
    extractRepoFromIssueUrl(card.github_issue_url)
  );
  var worktreePath = firstPresent(
    latestWork && latestWork.worktree_path,
    tracking && tracking.worktree_path
  );
  var branch = firstPresent(
    latestWork && latestWork.branch,
    tracking && tracking.branch
  );
  var headSha = firstPresent(
    latestWork && latestWork.head_sha,
    tracking && tracking.head_sha
  );

  if (tracking && latestWork) {
    if (tracking.worktree_path && latestWork.worktree_path
        && tracking.worktree_path !== latestWork.worktree_path) {
      appendMergeCandidateReason(
        details,
        "tracking_worktree_stale",
        "tracking worktree_path " + tracking.worktree_path +
          " replaced with latest completed work worktree_path " + latestWork.worktree_path
      );
    }
    if (tracking.branch && latestWork.branch && tracking.branch !== latestWork.branch) {
      appendMergeCandidateReason(
        details,
        "tracking_branch_stale",
        "tracking branch " + tracking.branch +
          " replaced with latest completed work branch " + latestWork.branch
      );
    }
    if (tracking.head_sha && latestWork.head_sha && tracking.head_sha !== latestWork.head_sha) {
      appendMergeCandidateReason(
        details,
        "tracking_head_sha_stale",
        "tracking head_sha " + tracking.head_sha +
          " replaced with latest completed work head_sha " + latestWork.head_sha
      );
    }
  }

  if (!repoId) {
    appendMergeCandidateReason(details, "missing_repo_id", "repo_id missing");
    agentdesk.log.info("[merge] Card " + cardId + " terminal merge skipped: repo_id missing");
    return null;
  }

  var missing = [];
  if (!worktreePath) missing.push("worktree_path");
  if (!branch) missing.push("branch");
  if (missing.length > 0) {
    appendMergeCandidateReason(
      details,
      "missing_candidate_fields",
      "missing " + missing.join(", ")
    );
    agentdesk.log.info(
      "[merge] Card " + cardId + " terminal merge skipped: missing " + missing.join(", ")
    );
    return null;
  }

  var session = findLatestSessionForWorktree(worktreePath);
  if (!session) {
    appendMergeCandidateReason(
      details,
      "untrusted_worktree_path",
      "untrusted worktree_path (no session match): " + worktreePath
    );
    agentdesk.log.warn(
      "[merge] Card " + cardId + " terminal merge skipped: untrusted worktree_path (no session match): " +
      worktreePath
    );
    return null;
  }

  if (
    card.assigned_agent_id &&
    session.agent_id &&
    String(session.agent_id) !== String(card.assigned_agent_id)
  ) {
    appendMergeCandidateReason(
      details,
      "worktree_owner_mismatch",
      "worktree owner mismatch (" + session.agent_id + " != " + card.assigned_agent_id + ")"
    );
    agentdesk.log.warn(
      "[merge] Card " + cardId + " terminal merge skipped: worktree owner mismatch (" +
      session.agent_id + " != " + card.assigned_agent_id + ")"
    );
    return null;
  }

  // Resolve branch from the trusted worktree session path; ignore dispatch-provided
  // branch values if they disagree.
  var canonicalBranchResult = agentdesk.exec("git", ["-C", worktreePath, "branch", "--show-current"]);
  if (
    !canonicalBranchResult ||
    canonicalBranchResult.indexOf("ERROR") === 0 ||
    !canonicalBranchResult.trim()
  ) {
    appendMergeCandidateReason(
      details,
      "canonical_branch_resolution_failed",
      "failed to resolve branch from trusted worktree"
    );
    agentdesk.log.warn(
      "[merge] Card " + cardId + " terminal merge skipped: failed to resolve branch from trusted worktree"
    );
    return null;
  }
  var canonicalBranch = canonicalBranchResult.trim();
  if (branch && branch !== canonicalBranch) {
    appendMergeCandidateReason(
      details,
      "canonical_branch_override",
      "branch " + branch + " replaced with canonical branch " + canonicalBranch
    );
    agentdesk.log.warn(
      "[merge] Card " + cardId + " terminal merge: dispatch branch mismatch (" +
      branch + " -> " + canonicalBranch + "); using canonical branch"
    );
  }
  branch = canonicalBranch;

  var candidate = {
    card: card,
    repo_id: repoId,
    worktree_path: worktreePath,
    branch: branch,
    head_sha: headSha
  };
  if (details) {
    details.candidate = candidate;
    appendMergeCandidateReason(
      details,
      "candidate_ready",
      "selected candidate " + branch + " at " + worktreePath
    );
  }
  return candidate;
}

function resolveCanonicalRepoRoot(worktreePath) {
  var commonDir = execGitOrThrow([
    "-C",
    worktreePath,
    "rev-parse",
    "--path-format=absolute",
    "--git-common-dir"
  ]).trim();
  return commonDir.replace(/\/\.git\/?$/, "");
}

function resolveMainWorktree(repoDir) {
  var worktreeOutput = execGitOrThrow(["-C", repoDir, "worktree", "list", "--porcelain"]);
  var worktrees = parseWorktreeList(worktreeOutput);
  if (!worktrees.length) {
    throw new Error("could not locate main worktree");
  }
  for (var i = 0; i < worktrees.length; i++) {
    if (worktrees[i].branch === "main" || worktrees[i].branch === "master") {
      return worktrees[i];
    }
  }
  return worktrees[0];
}

function resolveMainBranchForCandidate(candidate) {
  var repoDir = resolveCanonicalRepoRoot(candidate.worktree_path);
  var mainWorktree = resolveMainWorktree(repoDir);
  return mainWorktree.branch || "main";
}

function attemptDirectMerge(candidate) {
  var repoDir = resolveCanonicalRepoRoot(candidate.worktree_path);
  var mainWorktree = resolveMainWorktree(repoDir);
  var mainBranch = mainWorktree.branch || "main";
  var branchRange = mainBranch + ".." + candidate.branch;
  var originalHead = execGitOrThrow(["-C", mainWorktree.path, "rev-parse", "HEAD"]).trim();
  var commitsOutput = execGitOrThrow([
    "-C",
    mainWorktree.path,
    "rev-list",
    "--reverse",
    branchRange
  ]);
  var commits = commitsOutput
    .split(/\r?\n/)
    .map(function(line) { return line.trim(); })
    .filter(function(line) { return !!line; });
  if (commits.length === 0) {
    return {
      ok: true,
      already_merged: true,
      branch: candidate.branch,
      main_branch: mainBranch,
      commits: [],
      stash: null
    };
  }

  var hasLocalChanges = execGitOrThrow(["-C", mainWorktree.path, "status", "--porcelain"]).trim() !== "";
  var stashCreated = false;
  if (hasLocalChanges) {
    execGitOrThrow([
      "-C",
      mainWorktree.path,
      "stash",
      "push",
      "-u",
      "-m",
      "agentdesk merge-automation " + candidate.branch
    ]);
    stashCreated = true;
  }

  var cherryPickArgs = ["-C", mainWorktree.path, "cherry-pick"].concat(commits);
  var cherryPickOutput = agentdesk.exec("git", cherryPickArgs);
  if (typeof cherryPickOutput === "string" && cherryPickOutput.indexOf("ERROR") === 0) {
    execGitMaybe(["-C", mainWorktree.path, "cherry-pick", "--abort"]);
    return {
      ok: false,
      conflict: isCherryPickConflict(cherryPickOutput),
      branch: candidate.branch,
      main_branch: mainBranch,
      error: cherryPickOutput.replace(/^ERROR:\s*/, ""),
      stash: maybeRestoreMergeStash(mainWorktree.path, stashCreated)
    };
  }

  try {
    execGitOrThrow(["-C", mainWorktree.path, "push", "origin", mainBranch]);
  } catch (e) {
    var cleanupNotes = [];
    var resetStatus = maybeResetDirectMergeHead(mainWorktree.path, originalHead);
    if (resetStatus) cleanupNotes.push(resetStatus);
    var stashStatus = maybeRestoreMergeStash(mainWorktree.path, stashCreated);
    if (stashStatus) cleanupNotes.push(stashStatus);
    return {
      ok: false,
      conflict: !!pushResult.conflict,
      branch: candidate.branch,
      main_branch: mainBranch,
      error: String(e),
      stash: cleanupNotes.length > 0 ? cleanupNotes.join("; ") : null
    };
  }

  return {
    ok: true,
    already_merged: false,
    branch: candidate.branch,
    main_branch: mainBranch,
    commits: commits,
    stash: maybeRestoreMergeStash(mainWorktree.path, stashCreated)
  };
}

function buildTrackedPrTitle(card) {
  var issueNum = card.github_issue_number || "?";
  return "#" + issueNum + " " + card.title;
}

function buildTrackedPrBody(card, options) {
  var mode = options && options.mode ? options.mode : "direct-first";
  var mergeResult = options && options.merge_result ? options.merge_result : null;
  var mainBranch = options && options.main_branch ? options.main_branch : "main";
  var lines = [];

  if (mode === "pr-always") {
    lines.push("Automated PR created because `merge_strategy_mode` is set to `pr-always`.");
  } else if (mergeResult && mergeResult.conflict) {
    lines.push(
      "Automated fallback PR after direct merge into `" + mainBranch + "` hit a cherry-pick conflict."
    );
  } else {
    lines.push("Automated fallback PR after direct merge into `" + mainBranch + "` could not be completed safely.");
  }
  lines.push("");
  lines.push("Card: `" + card.id + "`");
  if (card.github_issue_url) {
    lines.push("Issue: " + card.github_issue_url);
  }
  if (mode === "pr-always") {
    lines.push("");
    lines.push("Merge path: wait for CI + Codex review approval before auto-merge.");
  } else if (mergeResult && mergeResult.error) {
    lines.push("");
    lines.push(mergeResult.conflict ? "Conflict summary:" : "Direct-merge failure summary:");
    lines.push(summarizeInlineText(mergeResult.error));
  }
  return lines.join("\n");
}

function resolveTrackedPrBaseBranch(candidate, fallbackBranch) {
  try {
    return resolveMainBranchForCandidate(candidate);
  } catch (e) {
    return fallbackBranch || "main";
  }
}

function markTrackedPrWaitingForCi(cardId, candidate, pr, headSha) {
  upsertPrTracking(
    cardId,
    candidate.repo_id,
    candidate.worktree_path,
    pr.branch || candidate.branch,
    pr.number,
    headSha,
    "wait-ci",
    null
  );
  agentdesk.db.execute(
    "UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = ?",
    [cardId]
  );
}

function tryCreateTrackedPr(cardId, tracking, candidate, options) {
  try {
    var trackedPr = createOrLocateTrackedPr(candidate, options || {});
    if (!trackedPr || !trackedPr.number) {
      throw new Error("no open PR found for branch " + candidate.branch);
    }

    var trackedHeadSha = getCurrentPrHeadSha(trackedPr.number, candidate.repo_id) || trackedPr.sha || candidate.head_sha;
    markTrackedPrWaitingForCi(cardId, candidate, trackedPr, trackedHeadSha);
    return {
      ok: true,
      pr: trackedPr,
      head_sha: trackedHeadSha
    };
  } catch (e) {
    upsertPrTracking(
      cardId,
      candidate.repo_id,
      candidate.worktree_path,
      candidate.branch,
      tracking ? tracking.pr_number : null,
      candidate.head_sha,
      "create-pr",
      String(e)
    );
    agentdesk.db.execute(
      "UPDATE kanban_cards SET blocked_reason = 'pr:create_failed' WHERE id = ?",
      [cardId]
    );
    return {
      ok: false,
      error: String(e)
    };
  }
}

function createOrLocateTrackedPr(candidate, options) {
  var existing = findOpenPrByTrackedBranch(candidate.repo_id, candidate.branch);
  if (existing) return existing;

  execGitOrThrow(["-C", candidate.worktree_path, "push", "-u", "origin", candidate.branch]);

  var createOutput = agentdesk.exec("gh", [
    "pr", "create",
    "--repo", candidate.repo_id,
    "--base", (options && options.main_branch) || "main",
    "--head", candidate.branch,
    "--title", buildTrackedPrTitle(candidate.card),
    "--body", buildTrackedPrBody(candidate.card, options || {})
  ]);
  if (typeof createOutput === "string" && createOutput.indexOf("ERROR") === 0) {
    if (/already exists/i.test(createOutput)) {
      return findOpenPrByTrackedBranch(candidate.repo_id, candidate.branch);
    }
    throw new Error(createOutput.replace(/^ERROR:\s*/, ""));
  }

  var prNumber = parsePrNumberFromOutput(createOutput);
  if (prNumber) {
    return {
      number: prNumber,
      branch: candidate.branch,
      sha: candidate.head_sha,
      repo: candidate.repo_id
    };
  }

  return findOpenPrByTrackedBranch(candidate.repo_id, candidate.branch);
}

function tryDirectMergeOrTrackPr(cardId, tracking) {
  var candidate = resolveTerminalMergeCandidate(cardId, tracking);
  if (!candidate) {
    agentdesk.log.info(
      "[merge] Card " + cardId + " terminal merge candidate unresolved; skipping direct merge/PR fallback"
    );
    return;
  }

  var mergeMode = resolveTrackedMergeStrategyMode(cardId);
  if (mergeMode !== "pr-always") {
    agentdesk.log.warn(
      "[merge] Card " + cardId + " requested direct-first merge, but direct merge is disabled; falling back to PR + CI"
    );
    mergeMode = "pr-always";
  }
  persistTrackedMergeStrategyMode(cardId, mergeMode);

  if (mergeMode === "pr-always") {
    var trackedPrResult = tryCreateTrackedPr(cardId, tracking, candidate, {
      mode: mergeMode,
      main_branch: resolveTrackedPrBaseBranch(candidate)
    });
    if (trackedPrResult.ok) {
      agentdesk.log.info("[merge] Card " + cardId + " is in pr-always mode — PR #" + trackedPrResult.pr.number + " is now tracked for CI");
    } else {
      agentdesk.log.warn("[merge] PR creation failed for pr-always card " + cardId + ": " + trackedPrResult.error);
    }
    return;
  }

  var mergeResult = null;
  try {
    mergeResult = attemptDirectMerge(candidate);
  } catch (e) {
    agentdesk.log.warn("[merge] Direct merge setup failed for card " + cardId + ": " + e);
    var setupFallback = tryCreateTrackedPr(cardId, tracking, candidate, {
      mode: mergeMode,
      main_branch: resolveTrackedPrBaseBranch(candidate),
      merge_result: { error: String(e), conflict: false }
    });
    if (setupFallback.ok) {
      agentdesk.log.info(
        "[merge] Card " + cardId + " fell back to PR #" + setupFallback.pr.number + " after direct-merge setup failure"
      );
    } else {
      agentdesk.log.warn("[merge] Direct merge setup fallback PR creation failed for card " + cardId + ": " + setupFallback.error);
    }
    return;
  }

  if (mergeResult.ok) {
    upsertPrTracking(
      cardId,
      candidate.repo_id,
      candidate.worktree_path,
      candidate.branch,
      null,
      candidate.head_sha,
      "closed",
      null
    );
    agentdesk.db.execute(
      "UPDATE kanban_cards SET blocked_reason = NULL WHERE id = ?",
      [cardId]
    );
    clearTrackedMergeStrategyMode(cardId);
    agentdesk.log.info("[merge] Card " + cardId + " direct-merged " + candidate.branch + " into " + mergeResult.main_branch);
    return;
  }

  agentdesk.log.warn("[merge] Direct merge failed for card " + cardId + ": " + mergeResult.error);
  var fallbackPr = tryCreateTrackedPr(cardId, tracking, candidate, {
    mode: mergeMode,
    main_branch: mergeResult.main_branch || resolveTrackedPrBaseBranch(candidate),
    merge_result: mergeResult
  });
  if (fallbackPr.ok) {
    agentdesk.log.info(
      "[merge] Card " + cardId + " fell back to PR #" + fallbackPr.pr.number + " after direct-merge failure"
    );
  } else {
    agentdesk.log.warn("[merge] Direct merge fallback PR creation failed for card " + cardId + ": " + fallbackPr.error);
  }
}

function getCurrentPrHeadSha(prNumber, repo) {
  var json = agentdesk.exec("gh", [
    "pr", "view", String(prNumber),
    "--json", "headRefOid",
    "--jq", ".headRefOid",
    "--repo", repo
  ]);
  if (json && json.indexOf("ERROR") !== 0) return json.trim();
  return null;
}

function getLatestCiRunForTrackedPr(repo, branch, headSha) {
  if (!repo || !branch) return null;
  var runsJson = agentdesk.exec("gh", [
    "run", "list",
    "--branch", branch,
    "--repo", repo,
    "--json", "databaseId,status,conclusion,headSha,event",
    "--limit", "5"
  ]);
  if (!runsJson || runsJson.indexOf("ERROR") === 0) return null;
  try {
    var runs = JSON.parse(runsJson);
    if (!runs || runs.length === 0) return null;
    if (headSha) {
      for (var i = 0; i < runs.length; i++) {
        if (runs[i].headSha === headSha) return runs[i];
      }
    }
    return runs[0];
  } catch (e) {
    return null;
  }
}

function verifyTrackedPrMergeReadiness(tracking, currentSha) {
  if (!tracking) return { ok: false, reason: "missing pr_tracking" };
  if (!tracking.branch) return { ok: false, reason: "missing tracked branch" };
  if (!tracking.repo_id) return { ok: false, reason: "missing tracked repo" };
  if (tracking.head_sha && currentSha && tracking.head_sha !== currentSha) {
    return {
      ok: false,
      reason: "tracked head SHA mismatch (" + tracking.head_sha + " != " + currentSha + ")"
    };
  }
  var run = getLatestCiRunForTrackedPr(tracking.repo_id, tracking.branch, currentSha || tracking.head_sha);
  if (!run) return { ok: false, reason: "no CI run found for tracked branch" };
  if (run.status !== "completed") {
    return { ok: false, reason: "CI still " + run.status + " for run " + run.databaseId };
  }
  if (run.conclusion !== "success") {
    return { ok: false, reason: "CI not green (" + run.conclusion + ") for run " + run.databaseId };
  }
  return { ok: true, run: run };
}

function findLatestSessionForWorktree(worktreePath) {
  if (!worktreePath) return null;
  var rows = agentdesk.db.query(
    "SELECT agent_id, thread_channel_id, status, session_key, cwd " +
    "FROM sessions WHERE cwd = ? OR cwd LIKE ? ORDER BY last_heartbeat DESC LIMIT 1",
    [worktreePath, worktreePath + "/%"]
  );
  return rows.length > 0 ? rows[0] : null;
}

function listSessionsForWorktree(worktreePath) {
  if (!worktreePath) return [];
  return agentdesk.db.query(
    "SELECT session_key, cwd FROM sessions WHERE cwd = ? OR cwd LIKE ?",
    [worktreePath, worktreePath + "/%"]
  );
}

function getReviewTargets(cardId) {
  var cfg = agentdesk.pipeline.resolveForCard(cardId);
  var terminalState = agentdesk.pipeline.terminalState(cfg);
  var initialState = agentdesk.pipeline.kickoffState(cfg);
  var inProgressState = agentdesk.pipeline.nextGatedTarget(initialState, cfg);
  var reviewState = agentdesk.pipeline.nextGatedTarget(inProgressState, cfg);
  var reviewReworkTarget = agentdesk.pipeline.nextGatedTargetWithGate(reviewState, "review_rework", cfg) || inProgressState;
  return {
    cfg: cfg,
    terminalState: terminalState,
    initialState: initialState,
    inProgressState: inProgressState,
    reviewState: reviewState,
    reviewReworkTarget: reviewReworkTarget
  };
}

function listOpenPrs(repo) {
  var prsJson = agentdesk.exec("gh", [
    "pr", "list",
    "--state", "open",
    "--json", "number,headRefName,title,mergeable",
    "--repo", repo
  ]);
  if (!prsJson || prsJson.indexOf("ERROR") === 0) return [];
  try {
    return JSON.parse(prsJson);
  } catch (e) {
    agentdesk.log.warn("[merge] Failed to parse open PR list for " + repo + ": " + e);
    return [];
  }
}

function fetchCodexReviews(repo, prNumber) {
  var json = agentdesk.exec("gh", [
    "api",
    "repos/" + repo + "/pulls/" + prNumber + "/reviews"
  ]);
  if (!json || json.indexOf("ERROR") === 0) return [];

  try {
    var reviews = JSON.parse(json);
    var filtered = [];
    for (var i = 0; i < reviews.length; i++) {
      var review = reviews[i] || {};
      var login = review.user && review.user.login ? review.user.login : "";
      if (!isCodexReviewer(login)) continue;
      filtered.push({
        id: String(review.id || ""),
        state: review.state || "",
        body: review.body || "",
        submitted_at: review.submitted_at || "",
        login: login
      });
    }
    filtered.sort(function(a, b) {
      if (a.submitted_at < b.submitted_at) return -1;
      if (a.submitted_at > b.submitted_at) return 1;
      if (a.id < b.id) return -1;
      if (a.id > b.id) return 1;
      return 0;
    });
    return filtered;
  } catch (e) {
    agentdesk.log.warn("[merge] Failed to parse Codex reviews for PR #" + prNumber + ": " + e);
    return [];
  }
}

function fetchCodexReviewThreads(repo, prNumber) {
  var parts = String(repo || "").split("/");
  if (parts.length !== 2) return [];

  var query =
    "query($owner:String!, $name:String!, $number:Int!) {" +
    " repository(owner:$owner, name:$name) {" +
    "  pullRequest(number:$number) {" +
    "   reviewThreads(first:100) {" +
    "    nodes {" +
    "     id isResolved isOutdated " +
    "     comments(first:100) {" +
    "      nodes {" +
    "       id body path line url " +
    "       author { login } " +
    "       pullRequestReview { id state author { login } }" +
    "      }" +
    "     }" +
    "    }" +
    "   }" +
    "  }" +
    " }" +
    "}";

  var json = agentdesk.exec("gh", [
    "api", "graphql",
    "-f", "query=" + query,
    "-f", "owner=" + parts[0],
    "-f", "name=" + parts[1],
    "-F", "number=" + String(prNumber)
  ]);
  if (!json || json.indexOf("ERROR") === 0) return [];

  try {
    var parsed = JSON.parse(json);
    var repository = ((parsed || {}).data || {}).repository || {};
    var pullRequest = repository.pullRequest || {};
    var reviewThreads = pullRequest.reviewThreads || {};
    return reviewThreads.nodes || [];
  } catch (e) {
    agentdesk.log.warn("[merge] Failed to parse Codex review threads for PR #" + prNumber + ": " + e);
    return [];
  }
}

function buildCodexReviewSnapshot(repo, prNumber) {
  var reviews = fetchCodexReviews(repo, prNumber);
  if (!reviews.length) return null;

  var latest = reviews[reviews.length - 1];
  var threads = fetchCodexReviewThreads(repo, prNumber);
  var blockingComments = [];
  var blockingReviewIds = {};
  var blockingFiles = [];
  var seenFiles = {};

  for (var i = 0; i < threads.length; i++) {
    var thread = threads[i] || {};
    if (thread.isResolved || thread.isOutdated) continue;

    var comments = thread.comments && thread.comments.nodes ? thread.comments.nodes : [];
    for (var j = 0; j < comments.length; j++) {
      var comment = comments[j] || {};
      var review = comment.pullRequestReview || {};
      var reviewId = review.id ? String(review.id) : String(latest.id);
      var login = (review.author && review.author.login) || (comment.author && comment.author.login) || "";
      if (!isCodexReviewer(login)) continue;
      if (!containsBlockingSeverity(comment.body)) continue;

      var path = comment.path || "(unknown file)";
      if (!seenFiles[path]) {
        seenFiles[path] = true;
        blockingFiles.push(path);
      }
      blockingReviewIds[reviewId] = true;
      blockingComments.push({
        reviewId: reviewId,
        path: path,
        line: comment.line != null ? String(comment.line) : "?",
        body: summarizeInlineText(comment.body),
        url: comment.url || ""
      });
    }
  }

  var triggerReviewId = String(latest.id);
  if (blockingComments.length > 0) {
    for (var r = reviews.length - 1; r >= 0; r--) {
      var candidateId = String(reviews[r].id);
      if (blockingReviewIds[candidateId]) {
        triggerReviewId = candidateId;
        break;
      }
    }
  }

  return {
    latestReviewId: String(latest.id),
    latestState: latest.state || "",
    latestBody: summarizeInlineText(latest.body || ""),
    latestSubmittedAt: latest.submitted_at || "",
    blockingComments: blockingComments,
    blockingFiles: blockingFiles,
    triggerReviewId: triggerReviewId,
    hasBlocking: blockingComments.length > 0
  };
}

function isCodexReviewApproved(snapshot) {
  return !!snapshot && String(snapshot.latestState || "").toUpperCase() === "APPROVED";
}

function codexReviewDedupKey(repo, prNumber, reviewId) {
  return "codex_review_processed:" +
    sanitizeKvKeyPart(repo) + ":" +
    sanitizeKvKeyPart(prNumber) + ":" +
    sanitizeKvKeyPart(reviewId);
}

function codexNotificationDedupKey(repo, prNumber, reviewId, kind) {
  return "codex_review_notified:" +
    sanitizeKvKeyPart(kind) + ":" +
    sanitizeKvKeyPart(repo) + ":" +
    sanitizeKvKeyPart(prNumber) + ":" +
    sanitizeKvKeyPart(reviewId);
}

function mergeGuardDedupKey(repo, prNumber, reviewId) {
  return "codex_merge_guard:" +
    sanitizeKvKeyPart(repo) + ":" +
    sanitizeKvKeyPart(prNumber) + ":" +
    sanitizeKvKeyPart(reviewId);
}

function findCardForPr(repo, pr) {
  var tracking = loadTrackedPrForRepoPr(repo, pr.number);
  return tracking ? loadCardContext(tracking.card_id) : null;
}

function resolveCodexNotificationTarget(card) {
  if (!card) return null;

  try {
    var unified = agentdesk.db.query(
      "SELECT r.unified_thread_channel_id FROM auto_queue_entries e " +
      "JOIN auto_queue_runs r ON r.id = e.run_id " +
      "WHERE e.kanban_card_id = ? AND r.unified_thread_channel_id IS NOT NULL " +
      "ORDER BY r.created_at DESC LIMIT 1",
      [card.id]
    );
    if (unified.length > 0 && unified[0].unified_thread_channel_id) {
      return unified[0].unified_thread_channel_id;
    }
  } catch (e) {}

  if (card.active_thread_id) return card.active_thread_id;

  if (card.assigned_agent_id) {
    var sessions = agentdesk.db.query(
      "SELECT thread_channel_id FROM sessions WHERE agent_id = ? AND thread_channel_id IS NOT NULL " +
      "ORDER BY last_heartbeat DESC LIMIT 1",
      [card.assigned_agent_id]
    );
    if (sessions.length > 0 && sessions[0].thread_channel_id) {
      return sessions[0].thread_channel_id;
    }

    var primary = agentdesk.agents.resolvePrimaryChannel(card.assigned_agent_id);
    if (primary) return primary;
  }

  return null;
}

function buildCodexReviewMessage(pr, snapshot, followUpIssue, mergeGuarded) {
  var lines = [];
  if (snapshot.hasBlocking) {
    lines.push("⚠️ PR #" + pr.number + " Codex 리뷰: unresolved P1/P2 " + snapshot.blockingComments.length + "건");
    if (snapshot.blockingFiles.length > 0) {
      lines.push("파일: " + snapshot.blockingFiles.join(", "));
    }
    for (var i = 0; i < snapshot.blockingComments.length && i < 3; i++) {
      var c = snapshot.blockingComments[i];
      lines.push("- " + c.path + ":" + c.line + " " + c.body);
    }
    if (snapshot.blockingComments.length > 3) {
      lines.push("- 외 " + (snapshot.blockingComments.length - 3) + "건");
    }
    if (followUpIssue) {
      var ref = followUpIssue.number ? "#" + followUpIssue.number : "생성 완료";
      lines.push("follow-up 이슈를 생성했습니다: " + ref);
      if (followUpIssue.url) {
        lines.push(followUpIssue.url);
      }
    } else if (mergeGuarded) {
      lines.push("merge를 차단했습니다.");
    }
  } else {
    lines.push("✅ PR #" + pr.number + " Codex 리뷰 통과");
    lines.push("blocking inline comment 없음");
  }
  return lines.join("\n");
}

function notifyCodexReview(card, pr, snapshot, kind, followUpIssue, mergeGuarded) {
  var target = resolveCodexNotificationTarget(card);
  if (!target) return;

  var dedupKey = codexNotificationDedupKey(pr.repo || "", pr.number, snapshot.triggerReviewId || snapshot.latestReviewId, kind);
  if (agentdesk.kv.get(dedupKey)) return;

  agentdesk.message.queue(
    target,
    buildCodexReviewMessage(pr, snapshot, followUpIssue, mergeGuarded),
    "announce",
    "merge-automation"
  );
  agentdesk.kv.set(dedupKey, "true", CODEX_NOTIFICATION_TTL_SECONDS);
}

function ensureGitHubLabel(repo, name, color, description) {
  if (!repo || !name) return false;
  var output = agentdesk.exec("gh", [
    "label", "create", name,
    "--repo", repo,
    "--force",
    "--color", color || "B60205",
    "--description", description || name
  ]);
  if (output && output.indexOf("ERROR") === 0) {
    agentdesk.log.warn("[merge] Failed to ensure label '" + name + "' in " + repo + ": " + output);
    return false;
  }
  return true;
}

function normalizeGitHubUrlOutput(text) {
  var lines = String(text || "").split(/\r?\n/);
  for (var i = 0; i < lines.length; i++) {
    var trimmed = lines[i].trim();
    if (/^https?:\/\//i.test(trimmed)) return trimmed;
  }
  var compact = compactWhitespace(text);
  return /^https?:\/\//i.test(compact) ? compact : "";
}

function extractIssueNumberFromUrl(url) {
  var match = String(url || "").match(/\/issues\/(\d+)(?:[/?#]|$)/);
  return match ? parseInt(match[1], 10) : null;
}

function buildCodexFollowUpTitle(card, pr) {
  var issueNum = card.github_issue_number || "?";
  return compactWhitespace("[Codex Follow-up] PR #" + pr.number + " #" + issueNum + " " + card.title);
}

function buildCodexFollowUpBody(card, pr, snapshot) {
  var lines = [
    "Codex PR review reported unresolved P1/P2 inline comments.",
    "",
    "원본 카드: `" + card.id + "`",
    "원본 PR: https://github.com/" + pr.repo + "/pull/" + pr.number,
    "원본 이슈: " + (card.github_issue_url || "(none)"),
    "담당 에이전트: `" + card.assigned_agent_id + "`",
    "리뷰 ID: `" + (snapshot.triggerReviewId || snapshot.latestReviewId || "") + "`",
    "",
    "현재 작업을 끊는 rework dispatch 대신 follow-up backlog issue로 전환합니다.",
    "",
    "Comments:"
  ];

  if (snapshot.blockingFiles.length > 0) {
    lines.push("Files: " + snapshot.blockingFiles.join(", "));
  }

  for (var i = 0; i < snapshot.blockingComments.length; i++) {
    var comment = snapshot.blockingComments[i];
    lines.push("- " + comment.path + ":" + comment.line + " — " + comment.body);
    if (comment.url) {
      lines.push("  comment: " + comment.url);
    }
  }

  if (snapshot.latestBody) {
    lines.push("");
    lines.push("Latest Codex review summary:");
    lines.push(snapshot.latestBody);
  }

  lines.push("");
  lines.push("Handle this as a follow-up backlog issue. Do not interrupt the agent's current session.");
  return lines.join("\n");
}

function parseIssueNumberFromUrl(url) {
  return extractIssueNumberFromUrl(url);
}

function codexFollowupPriority(snapshot) {
  for (var i = 0; i < snapshot.blockingComments.length; i++) {
    if (/\bP1\b/i.test(snapshot.blockingComments[i].body || "")) {
      return "urgent";
    }
  }
  return "high";
}

function createCodexFollowupIssue(card, pr, snapshot) {
  var repo = pr.repo || card.repo_id;
  if (!repo) return null;

  var title = buildCodexFollowUpTitle(card, pr);
  var body = buildCodexFollowUpBody(card, pr, snapshot);
  var agentLabel = card.assigned_agent_id ? "agent:" + card.assigned_agent_id : null;
  if (agentLabel) {
    ensureGitHubLabel(repo, agentLabel, "1D76DB", "Auto-assign follow-up work to " + card.assigned_agent_id);
  }
  var args = [
    "issue", "create",
    "--repo", repo,
    "--title", title,
    "--body", body
  ];
  if (agentLabel) {
    args.push("--label", agentLabel);
  }
  var output = agentdesk.exec("gh", args);
  if (agentLabel && typeof output === "string" && output.indexOf("ERROR") === 0) {
    agentdesk.log.warn("[merge] Codex follow-up issue create with label failed for PR #" + pr.number + ": " + output);
    output = agentdesk.exec("gh", [
      "issue", "create",
      "--repo", repo,
      "--title", title,
      "--body", body
    ]);
  }
  if (typeof output === "string" && output.indexOf("ERROR") === 0) {
    throw new Error(output.replace(/^ERROR:\s*/, ""));
  }
  if (typeof output !== "string") {
    throw new Error("gh issue create returned non-string output");
  }

  var issueUrl = normalizeGitHubUrlOutput(output);
  if (!issueUrl) {
    throw new Error("gh issue create returned empty output");
  }
  var issueNumber = extractIssueNumberFromUrl(issueUrl);
  if (!issueNumber) {
    throw new Error("gh issue create returned invalid issue URL: " + issueUrl);
  }

  return {
    url: issueUrl,
    issueNumber: issueNumber,
    title: title,
    body: body,
    repo: repo,
    priority: codexFollowupPriority(snapshot),
    labels: agentLabel || ""
  };
}

function createCodexFollowupBacklogCard(card, pr, snapshot, issueInfo) {
  if (!issueInfo || !issueInfo.url || !issueInfo.repo) return null;

  var issueNumber = Number(issueInfo.issueNumber || 0) || parseIssueNumberFromUrl(issueInfo.url);
  if (!issueNumber) {
    agentdesk.log.warn("[merge] Codex follow-up issue URL missing issue number for PR #" + pr.number + ": " + issueInfo.url);
    return null;
  }

  var localCardId = compactWhitespace(
    "codex-followup-" +
    sanitizeKvKeyPart(issueInfo.repo) + "-" +
    sanitizeKvKeyPart(pr.number) + "-" +
    sanitizeKvKeyPart(snapshot.triggerReviewId || snapshot.latestReviewId)
  );

  agentdesk.db.execute(
    "INSERT OR IGNORE INTO kanban_cards " +
    "(id, repo_id, title, status, priority, github_issue_url, github_issue_number, description, metadata, created_at, updated_at) " +
    "VALUES (?, ?, ?, 'backlog', ?, ?, ?, ?, ?, datetime('now'), datetime('now'))",
    [
      localCardId,
      issueInfo.repo,
      issueInfo.title,
      issueInfo.priority,
      issueInfo.url,
      issueNumber,
      issueInfo.body,
      JSON.stringify({ labels: issueInfo.labels })
    ]
  );

  return localCardId;
}

function processCodexBlockingReview(card, pr, snapshot) {
  if (!card || !snapshot.hasBlocking) return;

  var dedupKey = codexReviewDedupKey(pr.repo, pr.number, snapshot.triggerReviewId);
  if (agentdesk.kv.get(dedupKey)) return;
  var latestCard = loadCardContext(card.id);
  if (!latestCard) return;

  try {
    var issueInfo = createCodexFollowupIssue(latestCard, pr, snapshot);
    agentdesk.kv.set(dedupKey, issueInfo.url, CODEX_REVIEW_TTL_SECONDS);
    createCodexFollowupBacklogCard(latestCard, pr, snapshot, issueInfo);
    var followUpIssue = {
      url: issueInfo.url,
      number: issueInfo.issueNumber
    };
    agentdesk.log.info(
      "[merge] Created Codex follow-up issue for PR #" + pr.number +
      (followUpIssue.url ? ": " + followUpIssue.url : "")
    );
    notifyCodexReview(latestCard, pr, snapshot, "blocking", followUpIssue, false);
  } catch (e) {
    agentdesk.log.warn("[merge] Failed to create Codex follow-up issue for PR #" + pr.number + ": " + e);
  }
}

function processCodexPassReview(card, pr, snapshot) {
  if (!card || snapshot.hasBlocking || !isCodexReviewApproved(snapshot)) return;
  notifyCodexReview(card, pr, snapshot, "pass", false, false);
}

function processCodexReviewSignals() {
  var tracked = listTrackedPrRows(
    "pr_number IS NOT NULL AND state IN ('wait-ci', 'merge')",
    []
  );
  for (var i = 0; i < tracked.length; i++) {
    var row = tracked[i];
    if (!row.repo_id || !row.pr_number) continue;

    var snapshot = buildCodexReviewSnapshot(row.repo_id, row.pr_number);
    if (!snapshot) continue;

    var card = loadCardContext(row.card_id);
    if (!card) continue;

    var pr = {
      number: row.pr_number,
      repo: row.repo_id,
      branch: row.branch || "",
      headRefName: row.branch || ""
    };

    if (snapshot.hasBlocking) {
      processCodexBlockingReview(card, pr, snapshot);
    } else {
      processCodexPassReview(card, pr, snapshot);
    }
  }
}

/**
 * Enable auto-merge on a PR (shared by auto and manual paths).
 * Returns true on success, false on failure.
 */
function enableAutoMerge(prNumber, repo, trackingKey) {
  var tracking = String(trackingKey || "").indexOf("manual:") === 0
    ? loadTrackedPrForRepoPr(repo, prNumber)
    : loadTrackedPrForCard(trackingKey);
  if (!tracking) {
    tracking = loadTrackedPrForRepoPr(repo, prNumber);
  }
  var trackingId = tracking ? tracking.card_id : trackingKey;
  var currentSha = getCurrentPrHeadSha(prNumber, repo);

  if (tracking) {
    var readiness = verifyTrackedPrMergeReadiness(tracking, currentSha || tracking.head_sha);
    if (!readiness.ok) {
      agentdesk.log.warn("[merge] Merge pre-check failed for PR #" + prNumber + ": " + readiness.reason);
      upsertPrTracking(
        tracking.card_id,
        tracking.repo_id,
        tracking.worktree_path,
        tracking.branch,
        tracking.pr_number,
        currentSha || tracking.head_sha,
        "escalated",
        readiness.reason
      );
      agentdesk.kv.set("merge_failed:" + trackingId, JSON.stringify({
        pr_number: prNumber,
        error: readiness.reason,
        timestamp: new Date().toISOString()
      }), 86400);
      notifyMergeFailure(tracking.card_id, prNumber, repo, readiness.reason);
      return false;
    }
  }

  var snapshot = buildCodexReviewSnapshot(repo, prNumber);
  if (snapshot && snapshot.hasBlocking) {
    agentdesk.log.warn("[merge] Blocking auto-merge for PR #" + prNumber + " due to unresolved Codex P1/P2 comments");
    agentdesk.kv.set("merge_blocked:" + trackingId, JSON.stringify({
      pr_number: prNumber,
      review_id: snapshot.triggerReviewId,
      blocked_comments: snapshot.blockingComments.length,
      timestamp: new Date().toISOString()
    }), 86400);
    if (tracking) {
      upsertPrTracking(
        tracking.card_id,
        tracking.repo_id,
        tracking.worktree_path,
        tracking.branch,
        tracking.pr_number,
        currentSha || tracking.head_sha,
        tracking.state || "merge",
        "unresolved Codex blocking comment"
      );
    }

    var guardKey = mergeGuardDedupKey(repo, prNumber, snapshot.triggerReviewId);
    if (!agentdesk.kv.get(guardKey)) {
      var card = tracking ? loadCardContext(tracking.card_id) : null;
      if (card) {
        notifyCodexReview(card, { number: prNumber, repo: repo }, snapshot, "merge-guard", false, true);
      }
      agentdesk.kv.set(guardKey, "true", CODEX_NOTIFICATION_TTL_SECONDS);
    }
    return false;
  }

  var trackedMode = tracking ? resolveTrackedMergeStrategyMode(tracking.card_id) : "direct-first";
  if (trackedMode === "pr-always" && !isCodexReviewApproved(snapshot)) {
    var approvalReason = snapshot
      ? "waiting for Codex Cloud review approval (" + (snapshot.latestState || "pending") + ")"
      : "waiting for Codex Cloud review approval";
    agentdesk.log.info("[merge] PR #" + prNumber + " is waiting for Codex approval before auto-merge");
    if (tracking) {
      upsertPrTracking(
        tracking.card_id,
        tracking.repo_id,
        tracking.worktree_path,
        tracking.branch,
        tracking.pr_number,
        currentSha || tracking.head_sha,
        tracking.state || "merge",
        approvalReason
      );
    }
    return false;
  }

  var strategy = agentdesk.config.get("merge_strategy") || "squash";
  var result = agentdesk.exec("gh", [
    "pr", "merge", String(prNumber),
    "--auto", "--" + strategy,
    "--repo", repo
  ]);

  if (result && result.indexOf("ERROR") === 0) {
    agentdesk.log.warn("[merge] Auto-merge failed for PR #" + prNumber + ": " + result);
    agentdesk.kv.set("merge_failed:" + trackingId, JSON.stringify({
      pr_number: prNumber,
      error: result,
      timestamp: new Date().toISOString()
    }), 86400);
    if (tracking) {
      upsertPrTracking(
        tracking.card_id,
        tracking.repo_id,
        tracking.worktree_path,
        tracking.branch,
        tracking.pr_number,
        currentSha || tracking.head_sha,
        "escalated",
        result
      );
    }
    notifyMergeFailure(tracking ? tracking.card_id : null, prNumber, repo, result);
    return false;
  }

  agentdesk.log.info("[merge] Auto-merge enabled for PR #" + prNumber + " (" + strategy + ")");
  agentdesk.kv.set("merge_pending:" + trackingId, String(prNumber), 86400);
  if (tracking) {
    upsertPrTracking(
      tracking.card_id,
      tracking.repo_id,
      tracking.worktree_path,
      tracking.branch,
      tracking.pr_number,
      currentSha || tracking.head_sha,
      "post-merge-cleanup",
      null
    );
  }
  return true;
}

function notifyMergeFailure(cardId, prNumber, repo, reason) {
  if (!cardId) return;

  var dedupKey = "merge_failure_notified:" + cardId + ":" + prNumber;
  if (agentdesk.kv.get(dedupKey)) return;

  var card = loadCardContext(cardId);
  if (!card) return;

  var target = card.active_thread_id;
  if (!target && card.assigned_agent_id) {
    target = agentdesk.agents.resolvePrimaryChannel(card.assigned_agent_id);
  }
  if (!target) {
    var pmdChannel = agentdesk.config.get("kanban_manager_channel_id");
    if (pmdChannel) {
      target = "channel:" + pmdChannel;
    }
  }
  if (!target) return;

  var titleRef = card.github_issue_number
    ? ("#" + card.github_issue_number + " " + (card.title || card.id))
    : (card.title || card.id);
  agentdesk.message.queue(
    target,
    "⚠️ " + titleRef + "\n" +
      "PR #" + prNumber + " auto-merge failed in `" + repo + "`.\n" +
      "Reason: " + summarizeInlineText(reason) + "\n" +
      "수동 확인이 필요합니다.",
    "announce",
    "merge-automation"
  );
  agentdesk.kv.set(dedupKey, "true", 7200);
}

/**
 * Get PR author login via gh CLI.
 */
function getPrAuthor(prNumber, repo) {
  var json = agentdesk.exec("gh", [
    "pr", "view", String(prNumber),
    "--json", "author",
    "--jq", ".author.login",
    "--repo", repo
  ]);
  if (json && json.indexOf("ERROR") !== 0) {
    return json.trim();
  }
  return "";
}

/**
 * Check if author is in the allowed list for auto-merge.
 * Reads merge_allowed_authors from kv_meta (comma-separated).
 * If not configured, rejects all auto-merges (safe default).
 */
function isAllowedAuthor(author) {
  if (!author) return false;
  var allowed = agentdesk.config.get("merge_allowed_authors");
  if (allowed) {
    var list = allowed.split(",").map(function(s) { return s.trim().toLowerCase(); });
    return list.indexOf(author.toLowerCase()) >= 0;
  }
  // No allowlist configured — reject to be safe
  agentdesk.log.info("[merge] merge_allowed_authors not configured, rejecting auto-merge");
  return false;
}

function sessionKeyToTmuxName(sessionKey) {
  if (!sessionKey) return "";
  var parts = String(sessionKey).split(":");
  if (parts.length <= 1) return parts[0];
  return parts.slice(1).join(":");
}

function tmuxSessionHasLivePane(tmuxName) {
  if (!tmuxName) return false;
  try {
    var out = agentdesk.exec("tmux", ["list-panes", "-t", "=" + tmuxName, "-F", "#{pane_dead}"]);
    return typeof out === "string" && out.indexOf("ERROR") === -1 && out.indexOf("0") !== -1;
  } catch (e) {
    return false;
  }
}

/**
 * Process manual merge requests from kv_meta.
 * Set merge_request:{pr_number} = "{owner/repo}" to trigger.
 * No author check — explicit manual request implies approval.
 */
function processManualMergeRequests() {
  var requests = agentdesk.db.query(
    "SELECT key, value FROM kv_meta WHERE key LIKE 'merge_request:%' AND (expires_at IS NULL OR expires_at > datetime('now'))",
    []
  );
  for (var i = 0; i < requests.length; i++) {
    var prNumber = requests[i].key.replace("merge_request:", "");
    var repo = requests[i].value;
    agentdesk.log.info("[merge] Processing manual merge request for PR #" + prNumber + " in " + repo);
    var ok = enableAutoMerge(parseInt(prNumber, 10), repo, "manual:" + prNumber);
    if (ok) {
      agentdesk.kv.delete(requests[i].key);
    } else {
      agentdesk.log.warn("[merge] Manual merge request for PR #" + prNumber + " failed, will retry next tick");
    }
  }
}

function processTrackedMergeQueue() {
  var tracked = listTrackedPrRows("state IN ('create-pr', 'merge')", []);
  for (var i = 0; i < tracked.length; i++) {
    var row = tracked[i];
    var card = loadCardContext(row.card_id);
    if (!card) continue;
    var cfg = agentdesk.pipeline.resolveForCard(card.id);
    if (!agentdesk.pipeline.isTerminal(card.status, cfg)) continue;

    if (row.state === "create-pr") {
      var candidate = resolveTerminalMergeCandidate(row.card_id, row);
      if (!candidate) continue;
      var trackedMode = resolveTrackedMergeStrategyMode(row.card_id);
      persistTrackedMergeStrategyMode(row.card_id, trackedMode);
      var retriedPr = tryCreateTrackedPr(row.card_id, row, candidate, {
        mode: trackedMode,
        main_branch: resolveTrackedPrBaseBranch(candidate),
        merge_result: row.last_error ? { error: row.last_error, conflict: false } : null
      });
      if (retriedPr.ok) {
        agentdesk.log.info(
          "[merge] Card " + row.card_id + " retried create-pr — PR #" + retriedPr.pr.number + " is now tracked for CI"
        );
      } else {
        agentdesk.log.warn("[merge] Create-pr retry failed for card " + row.card_id + ": " + retriedPr.error);
      }
      continue;
    }

    if (!row.pr_number || !row.repo_id) continue;
    var author = getPrAuthor(row.pr_number, row.repo_id);
    if (!isAllowedAuthor(author)) continue;
    enableAutoMerge(row.pr_number, row.repo_id, row.card_id);
  }
}

/**
 * Find the PR associated with a card by looking up the agent's worktree branch.
 *
 * Strategy:
 *   1. Get card's assigned_agent_id
 *   2. Find agent's sessions → get cwd (worktree path)
 *   3. Get branch name from the worktree path
 *   4. Find PR by branch name via gh CLI
 */
function findPrForCard(cardId) {
  var tracking = loadTrackedPrForCard(cardId);
  if (!tracking) return null;
  if ((!tracking.pr_number || !tracking.head_sha) && tracking.repo_id && tracking.branch) {
    var discovered = findOpenPrByTrackedBranch(tracking.repo_id, tracking.branch);
    if (discovered) {
      upsertPrTracking(
        tracking.card_id,
        tracking.repo_id,
        tracking.worktree_path,
        discovered.branch || tracking.branch,
        discovered.number,
        discovered.sha,
        tracking.state || "merge",
        null
      );
      tracking = loadTrackedPrForCard(cardId) || tracking;
    }
  }
  if (!tracking.pr_number || !tracking.repo_id) return null;
  return {
    number: tracking.pr_number,
    branch: tracking.branch,
    repo: tracking.repo_id,
    sha: tracking.head_sha
  };
}

function getBranchFromWorktree(cwd) {
  if (!cwd) return null;
  var result = agentdesk.exec("git", ["-C", cwd, "branch", "--show-current"]);
  if (result && result.indexOf("ERROR") !== 0 && result.trim()) {
    return result.trim();
  }
  return null;
}

/**
 * Cleanup worktrees whose branches have been merged.
 * Checks recently merged PRs and removes their worktree + branch.
 */
function cleanupMergedWorktrees() {
  var tracked = listTrackedPrRows(
    "state = 'post-merge-cleanup' AND pr_number IS NOT NULL AND repo_id IS NOT NULL",
    []
  );
  for (var i = 0; i < tracked.length; i++) {
    var row = tracked[i];
    try {
      var prJson = agentdesk.exec("gh", [
        "pr", "view", String(row.pr_number),
        "--json", "mergedAt,headRefName",
        "--repo", row.repo_id
      ]);
      if (!prJson || prJson.indexOf("ERROR") === 0) continue;

      var pr = JSON.parse(prJson);
      if (!pr || !pr.mergedAt) continue;

      var branch = row.branch || pr.headRefName;
      var cwd = row.worktree_path;
      if (!cwd || !branch) {
        upsertPrTracking(
          row.card_id,
          row.repo_id,
          row.worktree_path,
          branch,
          row.pr_number,
          row.head_sha,
          "post-merge-cleanup",
          "cleanup requires canonical worktree_path and branch"
        );
        continue;
      }

      var sessions = listSessionsForWorktree(cwd);
      var inUseBy = null;
      for (var s = 0; s < sessions.length; s++) {
        var tmuxName = sessionKeyToTmuxName(sessions[s].session_key);
        if (tmuxSessionHasLivePane(tmuxName)) {
          inUseBy = sessions[s];
          break;
        }
      }
      if (inUseBy) {
        agentdesk.log.info(
          "[merge] Skipping cleanup for merged worktree still in use: " +
          branch + " at " + inUseBy.cwd + " (" + inUseBy.session_key + ")"
        );
        continue;
      }

      agentdesk.log.info("[merge] Cleaning up merged worktree: " + branch + " at " + cwd);
      var mainRepo = agentdesk.exec("git", [
        "-C", cwd, "rev-parse", "--path-format=absolute", "--git-common-dir"
      ]);
      if (!mainRepo || mainRepo.indexOf("ERROR") === 0) {
        upsertPrTracking(
          row.card_id,
          row.repo_id,
          row.worktree_path,
          branch,
          row.pr_number,
          row.head_sha,
          "post-merge-cleanup",
          "could not determine main repo for worktree"
        );
        agentdesk.log.warn("[merge] Could not determine main repo for worktree: " + cwd);
        continue;
      }

      mainRepo = mainRepo.replace(/\/.git\/?$/, "");
      agentdesk.exec("git", ["-C", mainRepo, "worktree", "remove", cwd, "--force"]);
      agentdesk.exec("git", ["-C", mainRepo, "branch", "-d", branch]);
      agentdesk.log.info("[merge] Worktree removed: " + cwd);

      upsertPrTracking(
        row.card_id,
        row.repo_id,
        cwd,
        branch,
        row.pr_number,
        row.head_sha,
        "closed",
        null
      );
      clearTrackedMergeStrategyMode(row.card_id);
      agentdesk.kv.delete("merge_pending:" + row.card_id);
      agentdesk.kv.delete("merge_failed:" + row.card_id);
      agentdesk.kv.delete("merge_blocked:" + row.card_id);
    } catch (e) {
      agentdesk.log.warn("[merge] Cleanup error for card " + row.card_id + ": " + e);
      upsertPrTracking(
        row.card_id,
        row.repo_id,
        row.worktree_path,
        row.branch,
        row.pr_number,
        row.head_sha,
        "post-merge-cleanup",
        String(e)
      );
    }
  }
}

/**
 * Detect open PRs with merge conflicts and dispatch rebase tasks.
 *
 * For each conflicting PR:
 *   1. Find the thread session that owns the worktree branch
 *   2. If thread is alive → send rebase instruction message directly
 *   3. If thread is dead → create a new dispatch (spawns new thread)
 *   4. Fallback: notify agent's main channel if no session found
 */
function detectConflictingPrs() {
  var tracked = listTrackedPrRows(
    "pr_number IS NOT NULL AND repo_id IS NOT NULL AND state IN ('wait-ci', 'merge', 'post-merge-cleanup')",
    []
  );
  for (var i = 0; i < tracked.length; i++) {
    var row = tracked[i];
    var prJson = agentdesk.exec("gh", [
      "pr", "view", String(row.pr_number),
      "--json", "number,headRefName,mergeable,title",
      "--repo", row.repo_id
    ]);
    if (!prJson || prJson.indexOf("ERROR") === 0) continue;

    try {
      var pr = JSON.parse(prJson);
      if (!pr || pr.mergeable !== "CONFLICTING") continue;

      var prNum = pr.number;
      var title = pr.title;
      agentdesk.log.warn("[merge] PR #" + prNum + " has conflicts: " + title);

      var session = findLatestSessionForWorktree(row.worktree_path);
      var card = loadCardContext(row.card_id);
      if (session && session.thread_channel_id) {
        var isAlive = session.status === "working" || session.status === "idle";
        if (isAlive) {
          var msgKey = "conflict_messaged:" + prNum;
          if (agentdesk.kv.get(msgKey)) continue;

          agentdesk.message.queue(
            session.thread_channel_id,
            "⚠️ PR #" + prNum + " has merge conflicts with main.\n" +
            "Please rebase: `git fetch origin main && git rebase origin/main`\n" +
            "Then force push: `git push --force-with-lease`",
            "announce",
            "merge-automation"
          );
          agentdesk.kv.set(msgKey, "true", 1800);
          agentdesk.log.info("[merge] Sent rebase message to thread " + session.thread_channel_id);
          continue;
        }

        var dispKey = "conflict_dispatched:" + prNum;
        if (agentdesk.kv.get(dispKey)) continue;
        if (card && session.agent_id) {
          try {
            agentdesk.dispatch.create(
              card.id,
              session.agent_id,
              "implementation",
              "[Rebase] PR #" + prNum + " — resolve merge conflicts with main"
            );
            agentdesk.kv.set(dispKey, "true", 7200);
            agentdesk.log.info("[merge] Created rebase dispatch for agent " + session.agent_id);
            continue;
          } catch (e) {
            agentdesk.log.info("[merge] Dispatch create failed (likely pending exists): " + e);
          }
        }
        if (session.agent_id) {
          notifyAgentMainChannel(session.agent_id, prNum, title);
        }
        continue;
      }

      if (card && card.active_thread_id) {
        var activeKey = "conflict_messaged:" + prNum;
        if (agentdesk.kv.get(activeKey)) continue;
        agentdesk.message.queue(
          card.active_thread_id,
          "⚠️ PR #" + prNum + " has merge conflicts with main.\n" +
          "Please rebase: `git fetch origin main && git rebase origin/main`\n" +
          "Then force push: `git push --force-with-lease`",
          "announce",
          "merge-automation"
        );
        agentdesk.kv.set(activeKey, "true", 1800);
        continue;
      }

      if (card && card.assigned_agent_id) {
        notifyAgentMainChannel(card.assigned_agent_id, prNum, title);
      } else {
        agentdesk.log.info("[merge] No tracked session found for conflicting PR #" + prNum);
      }
    } catch (e) {
      agentdesk.log.warn("[merge] Conflict detection error: " + e);
    }
  }
}

/**
 * Find an active (non-terminal) card assigned to the given agent.
 * Prefers cards in review/in_progress states.
 */
function findCardForAgent(agentId) {
  var cards = agentdesk.db.query(
    "SELECT id, status FROM kanban_cards " +
    "WHERE assigned_agent_id = ? AND status NOT IN ('done', 'archived') " +
    "ORDER BY updated_at DESC LIMIT 1",
    [agentId]
  );
  return cards.length > 0 ? cards[0] : null;
}

/**
 * Fallback: notify agent's main Discord channel about conflicts.
 */
function notifyAgentMainChannel(agentId, prNum, title) {
  var kvKey = "conflict_notified:" + prNum;
  if (agentdesk.kv.get(kvKey)) return;

  // #304: resolve primary channel via centralized resolver
  var mainCh = agentdesk.agents.resolvePrimaryChannel(agentId);
  if (mainCh) {
    agentdesk.message.queue(
      mainCh,
      "⚠️ PR #" + prNum + " (" + title + ") has merge conflicts with main. Please rebase.",
      "announce",
      "merge-automation"
    );
  }
  agentdesk.kv.set(kvKey, "true", 7200); // 2h TTL
}

agentdesk.mergeAutomation = agentdesk.mergeAutomation || {};
agentdesk.mergeAutomation.diagnoseTerminalMergeCandidate = function(cardId) {
  var tracking = loadTrackedPrForCard(cardId);
  var details = {
    card_id: cardId,
    tracking: tracking || null,
    latest_work_dispatches: [],
    latest_work: null,
    reasons: [],
    candidate: null
  };
  resolveTerminalMergeCandidate(cardId, tracking, details);
  return details;
};

agentdesk.registerPolicy(mergeAutomation);

})();
