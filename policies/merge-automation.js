// ── merge-automation.js ──────────────────────────────────────────────────
// Automates PR merging and worktree cleanup after review passes.
//
// Flow:
//   1. Card reaches terminal (done) → find associated PR → enable auto-merge
//   2. OnTick5min → detect conflicting PRs → dispatch rebase
//   3. OnTick5min → cleanup merged worktree branches
//
// PR discovery: looks up the card's worktree branch from sessions table,
// then finds the PR by branch name via `gh pr list`.
//
// Config (kv_meta):
//   merge_automation_enabled  — "true" to enable (default: disabled)
//   merge_strategy            — "squash" | "rebase" | "merge" (default: "squash")
//   merge_allowed_authors     — comma-separated GitHub usernames for auto-merge
//                               (e.g. "itismyfield,kunkunGames,bot[bot]")
//
// Manual merge trigger:
//   Set kv: merge_request:{pr_number} = "{owner/repo}"
//   OnTick5min picks it up and merges (no author check — explicit request)

var CODEX_REVIEWERS = {
  "chatgpt-codex-connector": true,
  "chatgpt-codex-connector[bot]": true
};
var CODEX_REVIEW_TTL_SECONDS = 14 * 24 * 60 * 60;
var CODEX_NOTIFICATION_TTL_SECONDS = 6 * 60 * 60;
var CODEX_MAX_CONTEXT_COMMENTS = 5;

var mergeAutomation = {
  name: "merge-automation",
  priority: 200,  // Run after all other policies

  // ── Card reached terminal → trigger auto-merge ──────────────────────
  onCardTerminal: function(payload) {
    if (!isEnabled()) return;

    var cardId = payload.card_id;
    var tracking = loadTrackedPrForCard(cardId);
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
    "FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  return cards.length > 0 ? cards[0] : null;
}

function parseJsonObject(raw) {
  if (!raw) return {};
  try {
    return JSON.parse(raw) || {};
  } catch (e) {
    return {};
  }
}

function loadTrackedPrForCard(cardId) {
  importLegacyPrCacheRows();
  var rows = agentdesk.db.query(
    "SELECT card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, last_error " +
    "FROM pr_tracking WHERE card_id = ?",
    [cardId]
  );
  return rows.length > 0 ? rows[0] : null;
}

function loadTrackedPrForRepoPr(repoId, prNumber) {
  importLegacyPrCacheRows();
  var rows = agentdesk.db.query(
    "SELECT card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, last_error " +
    "FROM pr_tracking WHERE repo_id = ? AND pr_number = ? LIMIT 1",
    [repoId, prNumber]
  );
  return rows.length > 0 ? rows[0] : null;
}

function upsertPrTracking(cardId, repoId, worktreePath, branch, prNumber, headSha, state, lastError) {
  agentdesk.db.execute(
    "INSERT INTO pr_tracking (card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, last_error, created_at, updated_at) " +
    "VALUES (?, ?, ?, ?, ?, ?, ?, ?, datetime('now'), datetime('now')) " +
    "ON CONFLICT(card_id) DO UPDATE SET " +
    "repo_id = COALESCE(excluded.repo_id, pr_tracking.repo_id), " +
    "worktree_path = COALESCE(excluded.worktree_path, pr_tracking.worktree_path), " +
    "branch = COALESCE(excluded.branch, pr_tracking.branch), " +
    "pr_number = COALESCE(excluded.pr_number, pr_tracking.pr_number), " +
    "head_sha = COALESCE(excluded.head_sha, pr_tracking.head_sha), " +
    "state = COALESCE(excluded.state, pr_tracking.state), " +
    "last_error = excluded.last_error, " +
    "updated_at = datetime('now')",
    [cardId, repoId, worktreePath, branch, prNumber, headSha, state, lastError]
  );
}

function importLegacyPrCacheRows() {
  var cached = agentdesk.db.query(
    "SELECT key, value FROM kv_meta WHERE key LIKE 'pr:%' AND (expires_at IS NULL OR expires_at > datetime('now'))",
    []
  );
  for (var i = 0; i < cached.length; i++) {
    var cardId = cached[i].key.replace("pr:", "");
    var existing = agentdesk.db.query(
      "SELECT 1 FROM pr_tracking WHERE card_id = ? LIMIT 1",
      [cardId]
    );
    if (existing.length > 0) continue;
    var info = parseJsonObject(cached[i].value);
    if (!info || (!info.number && !info.pr_number && !info.branch && !info.headRefName)) continue;
    var card = agentdesk.db.query(
      "SELECT repo_id, github_issue_url FROM kanban_cards WHERE id = ?",
      [cardId]
    );
    var repoId = info.repo || (card.length > 0 ? (card[0].repo_id || extractRepo(card[0].github_issue_url)) : null);
    upsertPrTracking(
      cardId,
      repoId,
      null,
      info.branch || info.headRefName || null,
      info.number || info.pr_number || null,
      info.sha || info.head_sha || null,
      info.state || ((info.number || info.pr_number) ? "merge" : "create-pr"),
      null
    );
  }
}

function listTrackedPrRows(whereClause, params) {
  importLegacyPrCacheRows();
  var query =
    "SELECT card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, last_error " +
    "FROM pr_tracking";
  if (whereClause) query += " WHERE " + whereClause;
  return agentdesk.db.query(query, params || []);
}

function findOpenPrByTrackedBranch(repoId, branch) {
  if (!repoId || !branch) return null;
  var prJson = agentdesk.exec("gh", [
    "pr", "list",
    "--head", branch,
    "--state", "open",
    "--json", "number,headRefName,headRefOid",
    "--limit", "1",
    "--repo", repoId
  ]);
  if (!prJson || prJson.indexOf("ERROR") === 0) return null;
  try {
    var prs = JSON.parse(prJson);
    if (!prs || prs.length === 0) return null;
    return {
      number: prs[0].number,
      branch: prs[0].headRefName,
      sha: prs[0].headRefOid,
      repo: repoId
    };
  } catch (e) {
    return null;
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

function hasActiveReworkDispatch(cardId) {
  var rows = agentdesk.db.query(
    "SELECT COUNT(*) AS count FROM task_dispatches " +
    "WHERE kanban_card_id = ? AND dispatch_type = 'rework' AND status IN ('pending', 'dispatched')",
    [cardId]
  );
  return rows.length > 0 && Number(rows[0].count || 0) > 0;
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

function buildCodexReviewMessage(pr, snapshot, reworkCreated, mergeGuarded) {
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
    if (reworkCreated) {
      lines.push("rework dispatch를 생성했습니다.");
    } else if (mergeGuarded) {
      lines.push("merge를 차단했습니다.");
    }
  } else {
    lines.push("✅ PR #" + pr.number + " Codex 리뷰 통과");
    lines.push("blocking inline comment 없음");
  }
  return lines.join("\n");
}

function notifyCodexReview(card, pr, snapshot, kind, reworkCreated, mergeGuarded) {
  var target = resolveCodexNotificationTarget(card);
  if (!target) return;

  var dedupKey = codexNotificationDedupKey(pr.repo || "", pr.number, snapshot.triggerReviewId || snapshot.latestReviewId, kind);
  if (agentdesk.kv.get(dedupKey)) return;

  agentdesk.message.queue(
    target,
    buildCodexReviewMessage(pr, snapshot, reworkCreated, mergeGuarded),
    "announce",
    "merge-automation"
  );
  agentdesk.kv.set(dedupKey, "true", CODEX_NOTIFICATION_TTL_SECONDS);
}

function buildCodexReworkTitle(card, pr, snapshot) {
  var issueNum = card.github_issue_number || "?";
  var lines = [
    "[Codex Rework] PR #" + pr.number + " #" + issueNum + " " + card.title,
    "",
    "Codex review found unresolved P1/P2 inline comments."
  ];

  if (snapshot.blockingFiles.length > 0) {
    lines.push("Files: " + snapshot.blockingFiles.join(", "));
  }

  lines.push("Comments:");
  for (var i = 0; i < snapshot.blockingComments.length && i < CODEX_MAX_CONTEXT_COMMENTS; i++) {
    var comment = snapshot.blockingComments[i];
    lines.push("- " + comment.path + ":" + comment.line + " — " + comment.body);
  }
  if (snapshot.blockingComments.length > CODEX_MAX_CONTEXT_COMMENTS) {
    lines.push("- 외 " + (snapshot.blockingComments.length - CODEX_MAX_CONTEXT_COMMENTS) + "건");
  }

  return lines.join("\n");
}

function processCodexBlockingReview(card, pr, snapshot) {
  if (!card || !card.assigned_agent_id || !snapshot.hasBlocking) return;

  var dedupKey = codexReviewDedupKey(pr.repo, pr.number, snapshot.triggerReviewId);
  if (agentdesk.kv.get(dedupKey)) return;

  var targets = getReviewTargets(card.id);
  var latestCard = loadCardContext(card.id);
  if (!latestCard) return;

  if (agentdesk.pipeline.isTerminal(latestCard.status, targets.cfg)) {
    agentdesk.kanban.reopen(card.id, targets.reviewReworkTarget);
    latestCard = loadCardContext(card.id) || latestCard;
  }

  var created = false;
  if (!hasActiveReworkDispatch(card.id)) {
    try {
      agentdesk.dispatch.create(
        card.id,
        latestCard.assigned_agent_id,
        "rework",
        buildCodexReworkTitle(latestCard, pr, snapshot)
      );
      created = true;
    } catch (e) {
      agentdesk.log.warn("[merge] Failed to create Codex rework dispatch for PR #" + pr.number + ": " + e);
    }
  }

  if (created || hasActiveReworkDispatch(card.id)) {
    agentdesk.reviewState.sync(card.id, "rework_pending", { last_verdict: "rework" });
    agentdesk.kanban.setReviewStatus(card.id, "rework_pending", { exclude_status: targets.terminalState });

    var currentCard = loadCardContext(card.id);
    if (currentCard && currentCard.status !== targets.reviewReworkTarget) {
      agentdesk.kanban.setStatus(card.id, targets.reviewReworkTarget);
    }

    agentdesk.kv.set(dedupKey, "true", CODEX_REVIEW_TTL_SECONDS);
    notifyCodexReview(latestCard, pr, snapshot, "blocking", created, false);
  }
}

function processCodexPassReview(card, pr, snapshot) {
  if (!card || snapshot.hasBlocking) return;
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
        tracking.state || "merge",
        readiness.reason
      );
      agentdesk.kv.set("merge_failed:" + trackingId, JSON.stringify({
        pr_number: prNumber,
        error: readiness.reason,
        timestamp: new Date().toISOString()
      }), 86400);
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
        tracking.state || "merge",
        result
      );
    }
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
  var tracked = listTrackedPrRows(
    "state = 'merge' AND pr_number IS NOT NULL AND repo_id IS NOT NULL",
    []
  );
  for (var i = 0; i < tracked.length; i++) {
    var row = tracked[i];
    var card = loadCardContext(row.card_id);
    if (!card) continue;
    var cfg = agentdesk.pipeline.resolveForCard(card.id);
    if (!agentdesk.pipeline.isTerminal(card.status, cfg)) continue;

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

function extractRepo(githubUrl) {
  if (!githubUrl) return null;
  // https://github.com/owner/repo/issues/42 → owner/repo
  var match = githubUrl.match(/github\.com\/([^\/]+\/[^\/]+)/);
  return match ? match[1] : null;
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

agentdesk.registerPolicy(mergeAutomation);
