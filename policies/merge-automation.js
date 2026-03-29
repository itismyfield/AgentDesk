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

    processManualMergeRequests();
    cleanupMergedWorktrees();
    detectConflictingPrs();
  }
};

// ── Helpers ───────────────────────────────────────────────────────────

function isEnabled() {
  return agentdesk.config.get("merge_automation_enabled") === "true";
}

/**
 * Enable auto-merge on a PR (shared by auto and manual paths).
 * Returns true on success, false on failure.
 */
function enableAutoMerge(prNumber, repo, trackingKey) {
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
          "SELECT cwd FROM sessions WHERE cwd LIKE ?",
          ["%/worktrees/%" + dirSuffix + "%"]
        );

        if (sessions.length > 0) {
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
 * Detect open PRs with merge conflicts.
 * If a PR is conflicting, notify the assigned agent's channel.
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

        var kvKey = "conflict_notified:" + prs[i].number;
        if (agentdesk.kv.get(kvKey)) continue; // Already notified

        agentdesk.log.warn("[merge] PR #" + prs[i].number + " has conflicts: " + prs[i].title);

        // Find the card associated with this PR's branch
        var branch = prs[i].headRefName;
        var sessions = agentdesk.db.query(
          "SELECT agent_id FROM sessions WHERE cwd LIKE ? LIMIT 1",
          ["%" + branch.replace("wt/", "") + "%"]
        );

        if (sessions.length > 0) {
          var agent = agentdesk.db.query(
            "SELECT discord_channel_id FROM agents WHERE id = ?",
            [sessions[0].agent_id]
          );
          if (agent.length > 0) {
            agentdesk.message.queue(
              agent[0].discord_channel_id,
              "⚠️ PR #" + prs[i].number + " (" + prs[i].title + ") has merge conflicts with main. Please rebase: `git rebase main`"
            );
          }
        }

        // Mark as notified (24h TTL)
        agentdesk.kv.set(kvKey, "true", 86400);
      }
    } catch(e) {
      agentdesk.log.warn("[merge] Conflict detection error: " + e);
    }
  }
}

agentdesk.registerPolicy(mergeAutomation);
