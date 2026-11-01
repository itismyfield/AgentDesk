(function() {
  var store = agentdesk.prTrackingStore ? agentdesk.prTrackingStore.create() : null;
  function parseJsonObject(raw) {
    if (!raw) return {};
    try {
      return JSON.parse(raw) || {};
    } catch (e) {
      return {};
    }
  }

  function extractRepoFromIssueUrl(url) {
    var match = String(url || "").match(/github\.com\/([^/]+\/[^/]+)/);
    return match ? match[1] : null;
  }

  function loadCardMeta(cardId) {
    var rows = agentdesk.db.query(
      "SELECT id, status, blocked_reason, repo_id, github_issue_url " +
      "FROM kanban_cards WHERE id = ? LIMIT 1",
      [cardId]
    );
    return rows.length > 0 ? rows[0] : null;
  }

  function loadPrTrackingCanonical(cardId) {
    var rows = agentdesk.db.query(
      "SELECT card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, last_error, " +
      "dispatch_generation, review_round, retry_count " +
      "FROM pr_tracking WHERE card_id = ?",
      [cardId]
    );
    return rows.length > 0 ? rows[0] : null;
  }

  function upsertPrTracking(cardId, repoId, worktreePath, branch, prNumber, headSha, state, lastError) {
    return store.upsert(cardId, repoId, worktreePath, branch, prNumber, headSha, state, lastError);
  }

  function markHandedOff(cardId, errorMsg) {
    return store.markHandedOff(cardId, String(errorMsg));
  }

  function touchGeneration(cardId, generation) {
    return store.touchGeneration(cardId, generation || "");
  }

  function findByRepoPr(repoId, prNumber) {
    var rows = agentdesk.db.query(
      "SELECT card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, last_error, " +
      "dispatch_generation, review_round, retry_count " +
      "FROM pr_tracking WHERE repo_id = ? AND pr_number = ? LIMIT 1",
      [repoId, prNumber]
    );
    return rows.length > 0 ? rows[0] : null;
  }

  function listTrackedPrRows(whereClause, params) {
    var query =
      "SELECT card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, last_error, " +
      "dispatch_generation, review_round, retry_count " +
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

  function repoForCard(cardId) {
    var card = loadCardMeta(cardId);
    if (!card) return null;
    return card.repo_id || extractRepoFromIssueUrl(card.github_issue_url);
  }

  function resolvePrInfoForCard(cardId, options) {
    var fallbackState = options && options.fallback_state ? options.fallback_state : null;
    var tracking = loadPrTrackingCanonical(cardId);
    if (!tracking) return null;

    var repo = tracking.repo_id || repoForCard(cardId);
    if (!repo) {
      var repos = agentdesk.db.query("SELECT id FROM github_repos LIMIT 1", []);
      if (repos.length > 0) repo = repos[0].id;
    }
    if (!repo) return null;

    if ((!tracking.pr_number || !tracking.head_sha) && tracking.branch) {
      var discovered = findOpenPrByTrackedBranch(repo, tracking.branch);
      if (discovered) {
        upsertPrTracking(
          cardId,
          repo,
          tracking.worktree_path,
          discovered.branch || tracking.branch,
          discovered.number,
          discovered.sha,
          tracking.state || fallbackState || "wait-ci",
          null
        );
        tracking = loadPrTrackingCanonical(cardId) || tracking;
      }
    }

    if (!tracking.pr_number || !tracking.branch) return null;
    return {
      number: tracking.pr_number,
      branch: tracking.branch,
      sha: tracking.head_sha,
      repo: repo,
      worktree_path: tracking.worktree_path
    };
  }

  agentdesk.prTracking = {
    parseJsonObject: parseJsonObject,
    extractRepoFromIssueUrl: extractRepoFromIssueUrl,
    loadCanonical: loadPrTrackingCanonical,
    load: loadPrTrackingCanonical,
    upsert: upsertPrTracking,
    markHandedOff: markHandedOff,
    touchGeneration: touchGeneration,
    list: listTrackedPrRows,
    findByRepoPr: findByRepoPr,
    findOpenPrByBranch: findOpenPrByTrackedBranch,
    repoForCard: repoForCard,
    resolvePrInfoForCard: resolvePrInfoForCard
  };
})();

agentdesk.registerPolicy({
  name: "pr-tracking",
  priority: 1
});
