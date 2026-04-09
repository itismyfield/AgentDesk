(function() {
  var legacyScanStamp = null;

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
      "SELECT card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, last_error " +
      "FROM pr_tracking WHERE card_id = ?",
      [cardId]
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

  function inferLegacyState(card, info, fallbackState) {
    if (info && info.state) return String(info.state);
    if (fallbackState) return fallbackState;

    var blockedReason = card && card.blocked_reason ? String(card.blocked_reason) : "";
    if (blockedReason.indexOf("ci:") === 0) return "wait-ci";
    if (info && (info.number || info.pr_number)) return "merge";
    return "create-pr";
  }

  function importLegacyPrTracking(cardId, fallbackState) {
    var existing = loadPrTrackingCanonical(cardId);
    if (existing) return existing;

    var cached = agentdesk.kv.get("pr:" + cardId);
    if (!cached) return null;

    var info = parseJsonObject(cached);
    if (!info || (!info.number && !info.pr_number && !info.branch && !info.headRefName)) {
      return null;
    }

    var card = loadCardMeta(cardId);
    var repoId = info.repo || (card ? (card.repo_id || extractRepoFromIssueUrl(card.github_issue_url)) : null);
    upsertPrTracking(
      cardId,
      repoId,
      null,
      info.branch || info.headRefName || null,
      info.number || info.pr_number || null,
      info.sha || info.head_sha || null,
      inferLegacyState(card, info, fallbackState),
      null
    );
    return loadPrTrackingCanonical(cardId);
  }

  function legacyScanStampValue() {
    var rows = agentdesk.db.query(
      "SELECT COUNT(*) AS count, COALESCE(MIN(key), '') AS min_key, COALESCE(MAX(key), '') AS max_key " +
      "FROM kv_meta WHERE key LIKE 'pr:%' AND (expires_at IS NULL OR expires_at > datetime('now'))",
      []
    );
    if (rows.length === 0) return "0::";
    return String(rows[0].count || 0) + ":" + String(rows[0].min_key || "") + ":" + String(rows[0].max_key || "");
  }

  function importLegacyPrTrackingOnce(fallbackState) {
    var nextStamp = legacyScanStampValue();
    if (legacyScanStamp === nextStamp) return;

    var cached = agentdesk.db.query(
      "SELECT key FROM kv_meta WHERE key LIKE 'pr:%' AND (expires_at IS NULL OR expires_at > datetime('now'))",
      []
    );
    for (var i = 0; i < cached.length; i++) {
      var cardId = String(cached[i].key || "").replace("pr:", "");
      if (!cardId) continue;
      if (loadPrTrackingCanonical(cardId)) continue;
      importLegacyPrTracking(cardId, fallbackState);
    }
    legacyScanStamp = nextStamp;
  }

  function loadPrTracking(cardId, options) {
    var tracking = loadPrTrackingCanonical(cardId);
    if (tracking) return tracking;
    var fallbackState = options && options.fallback_state ? options.fallback_state : null;
    return importLegacyPrTracking(cardId, fallbackState);
  }

  function findByRepoPr(repoId, prNumber, options) {
    var rows = agentdesk.db.query(
      "SELECT card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, last_error " +
      "FROM pr_tracking WHERE repo_id = ? AND pr_number = ? LIMIT 1",
      [repoId, prNumber]
    );
    if (rows.length > 0) return rows[0];

    var fallbackState = options && options.fallback_state ? options.fallback_state : null;
    importLegacyPrTrackingOnce(fallbackState);
    rows = agentdesk.db.query(
      "SELECT card_id, repo_id, worktree_path, branch, pr_number, head_sha, state, last_error " +
      "FROM pr_tracking WHERE repo_id = ? AND pr_number = ? LIMIT 1",
      [repoId, prNumber]
    );
    return rows.length > 0 ? rows[0] : null;
  }

  function listTrackedPrRows(whereClause, params, options) {
    var fallbackState = options && options.fallback_state ? options.fallback_state : null;
    importLegacyPrTrackingOnce(fallbackState);

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

  function repoForCard(cardId) {
    var card = loadCardMeta(cardId);
    if (!card) return null;
    return card.repo_id || extractRepoFromIssueUrl(card.github_issue_url);
  }

  function resolvePrInfoForCard(cardId, options) {
    var fallbackState = options && options.fallback_state ? options.fallback_state : null;
    var tracking = loadPrTracking(cardId, options);
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
    load: loadPrTracking,
    upsert: upsertPrTracking,
    list: listTrackedPrRows,
    findByRepoPr: findByRepoPr,
    findOpenPrByBranch: findOpenPrByTrackedBranch,
    repoForCard: repoForCard,
    resolvePrInfoForCard: resolvePrInfoForCard,
    importLegacyForCard: importLegacyPrTracking,
    importLegacyOnce: importLegacyPrTrackingOnce
  };
})();

agentdesk.registerPolicy({
  name: "pr-tracking",
  priority: 1
});
