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

// #4250: Review-snapshot reads are safe to abandon and retry, so keep their
// individual timeout tight. Merge-readiness reads feed escalation decisions;
// allow the pre-#4250 budget there and surface deadline timeouts as transient.
var GH_EXEC_TIMEOUT_MS = 1500;
var GH_MERGE_READINESS_TIMEOUT_MS = 30000;

function execGh(args, timeoutMs) {
  return agentdesk.exec("gh", args, {
    timeout_ms: timeoutMs || GH_EXEC_TIMEOUT_MS
  });
}

function isGhTimeoutError(output) {
  return typeof output === "string" &&
    output.indexOf("ERROR") === 0 &&
    /timed out|timeout|bridge deadline/i.test(output);
}

function isCodexReviewer(login) {
  if (!login) return false;
  return !!_CODEX_REVIEWERS[String(login).toLowerCase()];
}

function getPrAuthor(prNumber, repo) {
  var json = execGh([
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
  var json = execGh([
    "pr", "view", String(prNumber),
    "--json", "headRefOid",
    "--jq", ".headRefOid",
    "--repo", repo
  ], GH_MERGE_READINESS_TIMEOUT_MS);
  if (json && json.indexOf("ERROR") !== 0) return json.trim();
  if (isGhTimeoutError(json)) return undefined;
  return null;
}

function getLatestCiRunForTrackedPr(repo, branch, headSha) {
  if (!repo || !branch) return null;
  var runsJson = execGh([
    "run", "list",
    "--branch", branch,
    "--repo", repo,
    "--json", "databaseId,status,conclusion,headSha,event",
    "--limit", "5"
  ], GH_MERGE_READINESS_TIMEOUT_MS);
  if (isGhTimeoutError(runsJson)) {
    return { transient: true, error: runsJson };
  }
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
  var prsJson = execGh([
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
  var json = execGh([
    "api",
    "repos/" + repo + "/pulls/" + prNumber + "/reviews"
  ]);
  if (!json || json.indexOf("ERROR") === 0) return null;

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
    return null;
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

  var json = execGh([
    "api", "graphql",
    "-f", "query=" + query,
    "-f", "owner=" + parts[0],
    "-f", "name=" + parts[1],
    "-F", "number=" + String(prNumber)
  ]);
  if (!json || json.indexOf("ERROR") === 0) return null;

  try {
    var parsed = JSON.parse(json);
    var repository = ((parsed || {}).data || {}).repository || {};
    var pullRequest = repository.pullRequest || {};
    var reviewThreads = pullRequest.reviewThreads || {};
    return reviewThreads.nodes || [];
  } catch (e) {
    agentdesk.log.warn("[merge] Failed to parse Codex review threads for PR #" + prNumber + ": " + e);
    return null;
  }
}

function ensureGitHubLabel(repo, name, color, description) {
  if (!repo || !name) return false;
  var output = execGh([
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
  GH_EXEC_TIMEOUT_MS: GH_EXEC_TIMEOUT_MS,
  GH_MERGE_READINESS_TIMEOUT_MS: GH_MERGE_READINESS_TIMEOUT_MS,
  execGh: execGh,
  isGhTimeoutError: isGhTimeoutError,
  isCodexReviewer: isCodexReviewer,
  getPrAuthor: getPrAuthor,
  getCurrentPrHeadSha: getCurrentPrHeadSha,
  getLatestCiRunForTrackedPr: getLatestCiRunForTrackedPr,
  listOpenPrs: listOpenPrs,
  fetchCodexReviews: fetchCodexReviews,
  fetchCodexReviewThreads: fetchCodexReviewThreads,
  ensureGitHubLabel: ensureGitHubLabel
};
