/** @module policies/lib/github-pr-adapter
 *
 * #1078: Extracted from merge-automation.js as part of the policy modularization pass.
 *
 * Thin wrappers around the `gh` CLI used by merge-automation to query PR
 * authorship, CI runs, open PR lists, Codex reviews/threads, and to ensure
 * labels exist. Each call returns plain JS structures (arrays/objects) or
 * null/empty fallbacks on error so callers can stay declarative.
 *
 * Depends on the global `agentdesk.exec` / `agentdesk.log` surfaces — the
 * test harness injects mocks through the same globals.
 */

var _CODEX_REVIEWERS = {
  "chatgpt-codex-connector": true,
  "chatgpt-codex-connector[bot]": true
};

function isCodexReviewer(login) {
  if (!login) return false;
  return !!_CODEX_REVIEWERS[String(login).toLowerCase()];
}

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

module.exports = {
  isCodexReviewer: isCodexReviewer,
  getPrAuthor: getPrAuthor,
  getCurrentPrHeadSha: getCurrentPrHeadSha,
  getLatestCiRunForTrackedPr: getLatestCiRunForTrackedPr,
  listOpenPrs: listOpenPrs,
  fetchCodexReviews: fetchCodexReviews,
  fetchCodexReviewThreads: fetchCodexReviewThreads,
  ensureGitHubLabel: ensureGitHubLabel
};
