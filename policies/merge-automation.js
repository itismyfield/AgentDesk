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
    agentdesk.log.info("[merge] Card " + cardId + " terminal — checking for PR to merge");

    var pr = findPrForCard(cardId);
    if (!pr) {
      agentdesk.log.info("[merge] No open PR found for card " + cardId);
      return;
    }

    // Author check — only auto-merge PRs from allowed authors
    var author = getPrAuthor(pr.number, pr.repo);
    if (!isAllowedAuthor(author)) {
      agentdesk.log.info("[merge] PR #" + pr.number + " author '" + author + "' not in allowed list, skipping auto-merge");
      return;
    }

    enableAutoMerge(pr.number, pr.repo, cardId);
  },

  // ── Periodic: manual merge requests + conflicts + cleanup ────────────
  onTick5min: function() {
    if (!isEnabled()) return;

    processCodexReviewSignals();
    processManualMergeRequests();
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
  var cached = agentdesk.db.query(
    "SELECT key, value FROM kv_meta WHERE key LIKE 'pr:%' AND (expires_at IS NULL OR expires_at > datetime('now'))",
    []
  );
  for (var i = 0; i < cached.length; i++) {
    try {
      var info = JSON.parse(cached[i].value || "{}");
      if (String(info.number) !== String(pr.number)) continue;
      if (info.repo && info.repo !== repo) continue;
      var card = loadCardContext(cached[i].key.replace("pr:", ""));
      if (card) return card;
    } catch (e) {}
  }

  var issueNumber = extractIssueNumberFromText(pr.title);
  if (issueNumber) {
    var byIssue = agentdesk.db.query(
      "SELECT id, status, assigned_agent_id, title, github_issue_number, active_thread_id, repo_id " +
      "FROM kanban_cards WHERE github_issue_number = ? AND (repo_id = ? OR repo_id IS NULL) " +
      "ORDER BY updated_at DESC LIMIT 1",
      [issueNumber, repo]
    );
    if (byIssue.length > 0) return byIssue[0];
  }

  var branch = pr.headRefName || pr.branch || "";
  if (branch) {
    var dirSuffix = branch.replace(/^wt\//, "");
    var sessions = agentdesk.db.query(
      "SELECT agent_id FROM sessions WHERE cwd LIKE ? ORDER BY last_heartbeat DESC LIMIT 1",
      ["%/worktrees/%" + dirSuffix + "%"]
    );
    if (sessions.length > 0 && sessions[0].agent_id) {
      var byAgent = agentdesk.db.query(
        "SELECT id, status, assigned_agent_id, title, github_issue_number, active_thread_id, repo_id " +
        "FROM kanban_cards WHERE assigned_agent_id = ? AND status != 'archived' " +
        "ORDER BY updated_at DESC LIMIT 1",
        [sessions[0].agent_id]
      );
      if (byAgent.length > 0) return byAgent[0];
    }
  }

  return null;
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
  var repos = agentdesk.db.query("SELECT id FROM github_repos", []);
  for (var r = 0; r < repos.length; r++) {
    var repo = repos[r].id;
    var prs = listOpenPrs(repo);
    for (var i = 0; i < prs.length; i++) {
      var pr = prs[i] || {};
      pr.repo = repo;

      var snapshot = buildCodexReviewSnapshot(repo, pr.number);
      if (!snapshot) continue;

      var card = findCardForPr(repo, pr);
      if (!card) {
        agentdesk.log.info("[merge] No card mapping for Codex-reviewed PR #" + pr.number + " in " + repo);
        continue;
      }

      if (snapshot.hasBlocking) {
        processCodexBlockingReview(card, pr, snapshot);
      } else {
        processCodexPassReview(card, pr, snapshot);
      }
    }
  }
}

/**
 * Enable auto-merge on a PR (shared by auto and manual paths).
 * Returns true on success, false on failure.
 */
function enableAutoMerge(prNumber, repo, trackingKey) {
  var snapshot = buildCodexReviewSnapshot(repo, prNumber);
  if (snapshot && snapshot.hasBlocking) {
    agentdesk.log.warn("[merge] Blocking auto-merge for PR #" + prNumber + " due to unresolved Codex P1/P2 comments");
    agentdesk.kv.set("merge_blocked:" + trackingKey, JSON.stringify({
      pr_number: prNumber,
      review_id: snapshot.triggerReviewId,
      blocked_comments: snapshot.blockingComments.length,
      timestamp: new Date().toISOString()
    }), 86400);

    var guardKey = mergeGuardDedupKey(repo, prNumber, snapshot.triggerReviewId);
    if (!agentdesk.kv.get(guardKey)) {
      var card = findCardForPr(repo, { number: prNumber, repo: repo, title: "", headRefName: "" });
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
    agentdesk.kv.set("merge_failed:" + trackingKey, JSON.stringify({
      pr_number: prNumber,
      error: result,
      timestamp: new Date().toISOString()
    }), 86400);
    return false;
  }

  agentdesk.log.info("[merge] Auto-merge enabled for PR #" + prNumber + " (" + strategy + ")");
  agentdesk.kv.set("merge_pending:" + trackingKey, String(prNumber), 86400);
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
  // Check kv cache first
  var cached = agentdesk.kv.get("pr:" + cardId);
  if (cached) {
    try {
      return JSON.parse(cached);
    } catch(e) {}
  }

  // Get card's agent and repo info
  var cards = agentdesk.db.query(
    "SELECT assigned_agent_id, github_issue_url FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (!cards.length || !cards[0].assigned_agent_id) return null;

  var agentId = cards[0].assigned_agent_id;
  var repo = extractRepo(cards[0].github_issue_url);
  if (!repo) {
    // Fallback: use first registered repo
    var repos = agentdesk.db.query("SELECT id FROM github_repos LIMIT 1", []);
    if (repos.length) repo = repos[0].id;
  }
  if (!repo) return null;

  // Find agent's sessions with worktree paths (cwd contains "worktrees/")
  var sessions = agentdesk.db.query(
    "SELECT cwd FROM sessions WHERE agent_id = ? AND cwd LIKE '%worktrees/%' ORDER BY last_heartbeat DESC LIMIT 5",
    [agentId]
  );

  for (var i = 0; i < sessions.length; i++) {
    var branch = getBranchFromWorktree(sessions[i].cwd);
    if (!branch) continue;

    var prJson = agentdesk.exec("gh", [
      "pr", "list",
      "--head", branch,
      "--state", "open",
      "--json", "number,headRefName",
      "--limit", "1",
      "--repo", repo
    ]);

    if (prJson && prJson.indexOf("ERROR") !== 0) {
      try {
        var prs = JSON.parse(prJson);
        if (prs.length > 0) {
          var pr = { number: prs[0].number, branch: branch, repo: repo };
          // Cache for future lookups
          agentdesk.kv.set("pr:" + cardId, JSON.stringify(pr), 86400);
          return pr;
        }
      } catch(e) {}
    }
  }

  // Fallback: search by card ID in PR title
  var searchJson = agentdesk.exec("gh", [
    "pr", "list",
    "--state", "open",
    "--search", cardId,
    "--json", "number,headRefName",
    "--limit", "1",
    "--repo", repo
  ]);

  if (searchJson && searchJson.indexOf("ERROR") !== 0) {
    try {
      var found = JSON.parse(searchJson);
      if (found.length > 0) {
        var pr = { number: found[0].number, branch: found[0].headRefName, repo: repo };
        agentdesk.kv.set("pr:" + cardId, JSON.stringify(pr), 86400);
        return pr;
      }
    } catch(e) {}
  }

  return null;
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
  var repos = agentdesk.db.query("SELECT id FROM github_repos", []);
  for (var r = 0; r < repos.length; r++) {
    var repo = repos[r].id;
    var mergedJson = agentdesk.exec("gh", [
      "pr", "list",
      "--state", "merged",
      "--limit", "10",
      "--json", "number,headRefName,mergedAt",
      "--repo", repo
    ]);

    if (!mergedJson || mergedJson.indexOf("ERROR") === 0) continue;

    try {
      var merged = JSON.parse(mergedJson);
      for (var i = 0; i < merged.length; i++) {
        var branch = merged[i].headRefName;
        if (!branch || branch.indexOf("wt/") !== 0) continue;

        // Match worktree dir by the part after "wt/" — dir names don't have "wt/" prefix
        // Branch: "wt/claude-channel-20260329-070702" → dir suffix: "claude-channel-20260329-070702"
        var dirSuffix = branch.replace(/^wt\//, "");
        var sessions = agentdesk.db.query(
          "SELECT session_key, cwd FROM sessions WHERE cwd LIKE ?",
          ["%/worktrees/%" + dirSuffix + "%"]
        );

        if (sessions.length > 0) {
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

          var cwd = sessions[0].cwd;
          agentdesk.log.info("[merge] Cleaning up merged worktree: " + branch + " at " + cwd);

          // Discover the main repo root dynamically via git
          var mainRepo = agentdesk.exec("git", [
            "-C", cwd, "rev-parse", "--path-format=absolute", "--git-common-dir"
          ]);
          if (mainRepo && mainRepo.indexOf("ERROR") !== 0) {
            // git-common-dir returns e.g. "/path/to/repo/.git" — strip /.git
            mainRepo = mainRepo.replace(/\/.git\/?$/, "");
            agentdesk.exec("git", ["-C", mainRepo, "worktree", "remove", cwd, "--force"]);
            agentdesk.exec("git", ["-C", mainRepo, "branch", "-d", branch]);
            agentdesk.log.info("[merge] Worktree removed: " + cwd);
          } else {
            agentdesk.log.warn("[merge] Could not determine main repo for worktree: " + cwd);
          }
        }

        // Clear kv entries — find cardId by PR number stored in merge_pending
        // merge_pending is stored as merge_pending:{cardId} = prNumber
        // Scan for matching PR number to find the right key
        var prNum = String(merged[i].number);
        var pendingCards = agentdesk.db.query(
          "SELECT key FROM kv_meta WHERE key LIKE 'merge_pending:%' AND value = ?",
          [prNum]
        );
        for (var pc = 0; pc < pendingCards.length; pc++) {
          agentdesk.kv.delete(pendingCards[pc].key);
        }
        // merge_failed is also stored as merge_failed:{cardId}
        var failedCards = agentdesk.db.query(
          "SELECT key FROM kv_meta WHERE key LIKE 'merge_failed:%'",
          []
        );
        for (var fc = 0; fc < failedCards.length; fc++) {
          try {
            var val = agentdesk.kv.get(failedCards[fc].key);
            if (val && JSON.parse(val).pr_number === merged[i].number) {
              agentdesk.kv.delete(failedCards[fc].key);
            }
          } catch(e2) {}
        }
      }
    } catch(e) {
      agentdesk.log.warn("[merge] Cleanup error: " + e);
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
  var repos = agentdesk.db.query("SELECT id FROM github_repos", []);
  for (var r = 0; r < repos.length; r++) {
    var repo = repos[r].id;
    var prsJson = agentdesk.exec("gh", [
      "pr", "list",
      "--state", "open",
      "--json", "number,headRefName,mergeable,title",
      "--repo", repo
    ]);

    if (!prsJson || prsJson.indexOf("ERROR") === 0) continue;

    try {
      var prs = JSON.parse(prsJson);
      for (var i = 0; i < prs.length; i++) {
        if (prs[i].mergeable !== "CONFLICTING") continue;

        var prNum = prs[i].number;
        var branch = prs[i].headRefName;
        var title = prs[i].title;
        var dirSuffix = branch.replace(/^wt\//, "");

        agentdesk.log.warn("[merge] PR #" + prNum + " has conflicts: " + title);

        // Find session with thread info for this branch
        var sessions = agentdesk.db.query(
          "SELECT agent_id, thread_channel_id, status, session_key, cwd " +
          "FROM sessions WHERE cwd LIKE ? ORDER BY last_heartbeat DESC LIMIT 1",
          ["%/worktrees/%" + dirSuffix + "%"]
        );

        if (sessions.length > 0 && sessions[0].thread_channel_id) {
          var session = sessions[0];
          var isAlive = session.status === "working" || session.status === "idle";

          if (isAlive) {
            // Path A: thread session alive → send message directly
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
            agentdesk.kv.set(msgKey, "true", 1800); // 30min TTL
            agentdesk.log.info("[merge] Sent rebase message to thread " + session.thread_channel_id);
          } else {
            // Path B: thread session dead → create dispatch for new thread
            var dispKey = "conflict_dispatched:" + prNum;
            if (agentdesk.kv.get(dispKey)) continue;

            var card = findCardForAgent(session.agent_id);
            if (card) {
              try {
                agentdesk.dispatch.create(
                  card.id,
                  session.agent_id,
                  "implementation",
                  "[Rebase] PR #" + prNum + " — resolve merge conflicts with main"
                );
                agentdesk.kv.set(dispKey, "true", 7200); // 2h TTL
                agentdesk.log.info("[merge] Created rebase dispatch for agent " + session.agent_id);
              } catch(e) {
                agentdesk.log.info("[merge] Dispatch create failed (likely pending exists): " + e);
                // Fallback to main channel message
                notifyAgentMainChannel(session.agent_id, prNum, title);
              }
            } else {
              notifyAgentMainChannel(session.agent_id, prNum, title);
            }
          }
        } else if (sessions.length > 0) {
          // Session found but no thread — notify main channel
          notifyAgentMainChannel(sessions[0].agent_id, prNum, title);
        } else {
          agentdesk.log.info("[merge] No session found for branch " + branch);
        }
      }
    } catch(e) {
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
