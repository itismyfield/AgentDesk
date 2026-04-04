/**
 * ci-recovery.js — ADK Policy: CI Failure Auto-Recovery (#257)
 * priority: 46 (between deploy-pipeline=45 and review-automation=50)
 *
 * Hooks:
 *   onTick1min — Poll CI status for cards in wait-ci phase
 *
 * Flow:
 *   1. Cards enter wait-ci when create-pr dispatch completes (review-automation.js)
 *   2. This policy polls GitHub Actions for CI results
 *   3. On success: transition card to terminal (done)
 *   4. On failure: classify → retryable_transient / code_failure / ambiguous
 *      - retryable_transient: auto-rerun failed jobs (max 3 retries)
 *      - code_failure: create rework dispatch with log context
 *      - ambiguous/exhausted: escalate to pending_decision
 */

var CI_MAX_RETRIES = 3;
var CI_LOG_MAX_LINES = 50;

// Transient failure patterns in CI logs
var TRANSIENT_PATTERNS = [
  "runner shutdown",
  "lost communication",
  "cache service",
  "artifact download",
  "dns resolve",
  "tls handshake",
  "connection timed out",
  "connection reset",
  "network unreachable",
  "service unavailable",
  "rate limit",
  "RUNNER_TEMP",
  "runner provisioning",
  "no space left on device"
];

// Job name patterns that indicate code-related failures
var CODE_JOB_PATTERNS = [
  "check",
  "test",
  "lint",
  "build",
  "compile",
  "clippy",
  "scripts"
];

// ── Helper: Extract owner/repo from card's github_issue_url ──

function getRepoForCard(cardId) {
  var cards = agentdesk.db.query(
    "SELECT github_issue_url FROM kanban_cards WHERE id = ?", [cardId]
  );
  if (cards.length === 0 || !cards[0].github_issue_url) return null;
  // Extract "owner/repo" from "https://github.com/owner/repo/issues/123"
  var match = cards[0].github_issue_url.match(/github\.com\/([^/]+\/[^/]+)/);
  return match ? match[1] : null;
}

// ── Helper: Find PR branch for card (same pattern as merge-automation.js) ──

function findPrInfoForCard(cardId) {
  // Check kv cache first (merge-automation stores pr:{cardId})
  var cached = agentdesk.kv.get("pr:" + cardId);
  if (cached) {
    try {
      return JSON.parse(cached);
    } catch (e) {}
  }

  var cards = agentdesk.db.query(
    "SELECT assigned_agent_id, github_issue_url FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (cards.length === 0 || !cards[0].assigned_agent_id) return null;

  var agentId = cards[0].assigned_agent_id;
  var repo = getRepoForCard(cardId);
  if (!repo) {
    var repos = agentdesk.db.query("SELECT id FROM github_repos LIMIT 1", []);
    if (repos.length > 0) repo = repos[0].id;
  }
  if (!repo) return null;

  // Find agent's sessions with worktree paths
  var sessions = agentdesk.db.query(
    "SELECT cwd FROM sessions WHERE agent_id = ? AND cwd LIKE '%worktrees/%' ORDER BY last_heartbeat DESC LIMIT 5",
    [agentId]
  );

  for (var i = 0; i < sessions.length; i++) {
    var cwd = sessions[i].cwd;
    if (!cwd) continue;
    var branchResult = agentdesk.exec("git", ["-C", cwd, "branch", "--show-current"]);
    if (!branchResult || branchResult.indexOf("ERROR") === 0 || !branchResult.trim()) continue;
    var branch = branchResult.trim();

    var prJson = agentdesk.exec("gh", [
      "pr", "list",
      "--head", branch,
      "--state", "open",
      "--json", "number,headRefName,headRefOid",
      "--limit", "1",
      "--repo", repo
    ]);

    if (prJson && prJson.indexOf("ERROR") !== 0) {
      try {
        var prs = JSON.parse(prJson);
        if (prs.length > 0) {
          var pr = {
            number: prs[0].number,
            branch: prs[0].headRefName,
            sha: prs[0].headRefOid,
            repo: repo
          };
          agentdesk.kv.set("pr:" + cardId, JSON.stringify(pr), 86400);
          return pr;
        }
      } catch (e) {}
    }
  }

  // Fallback: search by card ID in PR title
  var searchJson = agentdesk.exec("gh", [
    "pr", "list",
    "--state", "open",
    "--search", cardId,
    "--json", "number,headRefName,headRefOid",
    "--limit", "1",
    "--repo", repo
  ]);

  if (searchJson && searchJson.indexOf("ERROR") !== 0) {
    try {
      var found = JSON.parse(searchJson);
      if (found.length > 0) {
        var pr = {
          number: found[0].number,
          branch: found[0].headRefName,
          sha: found[0].headRefOid,
          repo: repo
        };
        agentdesk.kv.set("pr:" + cardId, JSON.stringify(pr), 86400);
        return pr;
      }
    } catch (e) {}
  }

  return null;
}

// ── Helper: Get current PR head SHA ──

function getCurrentPrSha(prNumber, repo) {
  var result = agentdesk.exec("gh", [
    "pr", "view", String(prNumber),
    "--json", "headRefOid",
    "--jq", ".headRefOid",
    "--repo", repo
  ]);
  if (result && result.indexOf("ERROR") !== 0) {
    return result.trim();
  }
  return null;
}

// ── Failure classification ──

function classifyFailure(runId, repo, conclusion) {
  // Cancelled or timed_out are always retryable
  if (conclusion === "cancelled" || conclusion === "timed_out") {
    return { type: "retryable_transient", reason: "Run " + conclusion };
  }

  // Get failed jobs
  var jobsJson = agentdesk.exec("gh", [
    "run", "view", String(runId),
    "--repo", repo,
    "--json", "jobs"
  ]);

  var failedJobs = [];
  if (jobsJson && jobsJson.indexOf("ERROR") !== 0) {
    try {
      var parsed = JSON.parse(jobsJson);
      var jobs = parsed.jobs || [];
      for (var i = 0; i < jobs.length; i++) {
        if (jobs[i].conclusion === "failure") {
          failedJobs.push(jobs[i].name || "unknown");
        }
      }
    } catch (e) {
      agentdesk.log.warn("[ci-recovery] Failed to parse jobs for run " + runId + ": " + e);
    }
  }

  // Get log excerpt (last CI_LOG_MAX_LINES lines of failed log)
  var logExcerpt = "";
  var logResult = agentdesk.exec("gh", [
    "run", "view", String(runId),
    "--repo", repo,
    "--log-failed"
  ]);
  if (logResult && logResult.indexOf("ERROR") !== 0) {
    var lines = logResult.split("\n");
    // Cap to last CI_LOG_MAX_LINES lines, max ~2KB
    var startLine = Math.max(0, lines.length - CI_LOG_MAX_LINES);
    logExcerpt = lines.slice(startLine).join("\n");
    if (logExcerpt.length > 2048) {
      logExcerpt = logExcerpt.substring(logExcerpt.length - 2048);
    }
  }

  // Check if log matches transient patterns
  var isTransient = false;
  var logLower = logExcerpt.toLowerCase();
  for (var t = 0; t < TRANSIENT_PATTERNS.length; t++) {
    if (logLower.indexOf(TRANSIENT_PATTERNS[t].toLowerCase()) >= 0) {
      isTransient = true;
      break;
    }
  }

  if (isTransient) {
    return { type: "retryable_transient", reason: "Transient pattern in log", logExcerpt: logExcerpt };
  }

  // Check if failed jobs match code-related patterns
  var isCodeJob = false;
  for (var j = 0; j < failedJobs.length; j++) {
    var jobLower = failedJobs[j].toLowerCase();
    for (var p = 0; p < CODE_JOB_PATTERNS.length; p++) {
      if (jobLower.indexOf(CODE_JOB_PATTERNS[p]) >= 0) {
        isCodeJob = true;
        break;
      }
    }
    if (isCodeJob) break;
  }

  if (isCodeJob) {
    return {
      type: "code_failure",
      reason: "Code job failed: " + failedJobs.join(", "),
      failedJobs: failedJobs,
      logExcerpt: logExcerpt
    };
  }

  // Ambiguous — neither clearly transient nor clearly code
  return {
    type: "ambiguous",
    reason: "Cannot classify: jobs=" + failedJobs.join(", "),
    failedJobs: failedJobs,
    logExcerpt: logExcerpt
  };
}

// ── Resolve terminal state for card (same pattern as review-automation.js) ──

function resolveTerminalState(cardId) {
  var cfg = agentdesk.pipeline.resolveForCard(cardId);
  var init = agentdesk.pipeline.kickoffState(cfg);
  var ip = agentdesk.pipeline.nextGatedTarget(init, cfg);
  var rev = agentdesk.pipeline.nextGatedTarget(ip, cfg);
  var term = agentdesk.pipeline.nextGatedTargetWithGate(rev, "review_passed", cfg) || agentdesk.pipeline.terminalState(cfg);
  return term;
}

// ── PM decision escalation ──

function escalateToPendingDecision(cardId, reason) {
  var cfg = agentdesk.pipeline.resolveForCard(cardId);
  var init = agentdesk.pipeline.kickoffState(cfg);
  var ip = agentdesk.pipeline.nextGatedTarget(init, cfg);
  var forceTargets = agentdesk.pipeline.forceOnlyTargets(ip, cfg);
  var pendingState = forceTargets[0];

  agentdesk.kanban.setStatus(cardId, pendingState);
  agentdesk.db.execute(
    "UPDATE kanban_cards SET blocked_reason = ? WHERE id = ?",
    [reason, cardId]
  );
  agentdesk.log.warn("[ci-recovery] Card " + cardId + " escalated to " + pendingState + ": " + reason);

  // Notify PMD channel
  var pmdChannel = agentdesk.config.get("kanban_manager_channel_id");
  if (pmdChannel) {
    var cards = agentdesk.db.query(
      "SELECT title, github_issue_number FROM kanban_cards WHERE id = ?", [cardId]
    );
    var issueNum = (cards.length > 0) ? (cards[0].github_issue_number || "?") : "?";
    var title = (cards.length > 0) ? cards[0].title : cardId;
    agentdesk.message.queue(
      "channel:" + pmdChannel,
      "CI Recovery — PM 판단 필요\n#" + issueNum + " " + title + "\n\n사유: " + reason,
      "announce",
      "system"
    );
  }
}

// ── Process a single card in wait-ci ──

function processWaitingCard(cardId, blockedReason) {
  // Find PR info for this card
  var pr = findPrInfoForCard(cardId);
  if (!pr) {
    agentdesk.log.info("[ci-recovery] No PR found for card " + cardId + " — skipping");
    return;
  }

  var repo = pr.repo;
  var branch = pr.branch;

  // ── Head SHA change detection ──
  var storedSha = agentdesk.kv.get("ci:" + cardId + ":head_sha");
  var currentSha = getCurrentPrSha(pr.number, repo);
  if (currentSha && storedSha && currentSha !== storedSha) {
    agentdesk.log.info("[ci-recovery] Head SHA changed for card " + cardId + " — resetting recovery state");
    agentdesk.kv.set("ci:" + cardId + ":retry_count", "0", 86400);
    agentdesk.kv.delete("ci:" + cardId + ":last_run_id");
    agentdesk.db.execute(
      "UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = ?",
      [cardId]
    );
  }
  if (currentSha) {
    agentdesk.kv.set("ci:" + cardId + ":head_sha", currentSha, 86400);
  }

  // ── Get CI runs ──
  var runsJson = agentdesk.exec("gh", [
    "run", "list",
    "--branch", branch,
    "--repo", repo,
    "--json", "databaseId,status,conclusion,headSha,event",
    "--limit", "5"
  ]);

  if (!runsJson || runsJson.indexOf("ERROR") === 0) {
    agentdesk.log.warn("[ci-recovery] Failed to fetch CI runs for card " + cardId + ": " + (runsJson || "empty"));
    return;
  }

  var runs = [];
  try {
    runs = JSON.parse(runsJson);
  } catch (e) {
    agentdesk.log.warn("[ci-recovery] Failed to parse CI runs for card " + cardId + ": " + e);
    return;
  }

  if (runs.length === 0) {
    agentdesk.log.info("[ci-recovery] No CI runs found for card " + cardId + " branch " + branch);
    return;
  }

  // Use the most recent run
  var run = runs[0];
  var runId = run.databaseId;

  // ── Dedup: skip if we already processed this run ──
  var lastRunId = agentdesk.kv.get("ci:" + cardId + ":last_run_id");
  if (lastRunId && String(lastRunId) === String(runId) && blockedReason !== "ci:rerunning") {
    return; // Already processed
  }

  // ── Handle based on run status ──
  if (run.status !== "completed") {
    // Still running — update blocked reason if needed
    if (blockedReason !== "ci:rerunning") {
      agentdesk.db.execute(
        "UPDATE kanban_cards SET blocked_reason = 'ci:running' WHERE id = ?",
        [cardId]
      );
    }
    return;
  }

  // Mark as processed
  agentdesk.kv.set("ci:" + cardId + ":last_run_id", String(runId), 86400);

  // ── CI passed ──
  if (run.conclusion === "success") {
    agentdesk.log.info("[ci-recovery] CI passed for card " + cardId + " (run " + runId + ")");
    agentdesk.db.execute(
      "UPDATE kanban_cards SET blocked_reason = NULL WHERE id = ?",
      [cardId]
    );
    var termState = resolveTerminalState(cardId);
    agentdesk.kanban.setStatus(cardId, termState);
    agentdesk.log.info("[ci-recovery] Card " + cardId + " → " + termState);

    // Cleanup kv state
    agentdesk.kv.delete("ci:" + cardId + ":retry_count");
    agentdesk.kv.delete("ci:" + cardId + ":head_sha");
    agentdesk.kv.delete("ci:" + cardId + ":last_run_id");
    return;
  }

  // ── CI failed — classify and recover ──
  var classification = classifyFailure(runId, repo, run.conclusion);
  agentdesk.log.info("[ci-recovery] Card " + cardId + " run " + runId + " classified as: " + classification.type + " (" + classification.reason + ")");

  if (classification.type === "retryable_transient") {
    var retryCount = parseInt(agentdesk.kv.get("ci:" + cardId + ":retry_count") || "0", 10);

    if (retryCount < CI_MAX_RETRIES) {
      // Rerun failed jobs
      var rerunResult = agentdesk.exec("gh", [
        "run", "rerun", String(runId),
        "--repo", repo,
        "--failed"
      ]);

      if (rerunResult && rerunResult.indexOf("ERROR") === 0) {
        agentdesk.log.warn("[ci-recovery] Rerun failed for run " + runId + ": " + rerunResult);
        escalateToPendingDecision(cardId, "CI rerun failed: " + rerunResult);
        return;
      }

      agentdesk.kv.set("ci:" + cardId + ":retry_count", String(retryCount + 1), 86400);
      agentdesk.db.execute(
        "UPDATE kanban_cards SET blocked_reason = 'ci:rerunning' WHERE id = ?",
        [cardId]
      );
      // Clear last_run_id so we re-evaluate the new run
      agentdesk.kv.delete("ci:" + cardId + ":last_run_id");
      agentdesk.log.info("[ci-recovery] Rerunning failed jobs for card " + cardId + " (retry " + (retryCount + 1) + "/" + CI_MAX_RETRIES + ")");
    } else {
      escalateToPendingDecision(cardId,
        "CI transient failure — max retries (" + CI_MAX_RETRIES + ") exhausted for run " + runId);
    }

  } else if (classification.type === "code_failure") {
    // Create rework dispatch to assigned agent
    var cards = agentdesk.db.query(
      "SELECT assigned_agent_id, title, github_issue_number FROM kanban_cards WHERE id = ?",
      [cardId]
    );
    if (cards.length === 0 || !cards[0].assigned_agent_id) {
      escalateToPendingDecision(cardId, "CI code failure but no assigned agent");
      return;
    }

    var card = cards[0];
    var issueNum = card.github_issue_number || "?";
    var runUrl = "https://github.com/" + repo + "/actions/runs/" + runId;

    // Truncate log for dispatch title
    var logSnippet = classification.logExcerpt || "";
    if (logSnippet.length > 1000) {
      logSnippet = logSnippet.substring(logSnippet.length - 1000);
    }

    try {
      agentdesk.dispatch.create(
        cardId,
        card.assigned_agent_id,
        "rework",
        "[CI Fix] #" + issueNum + " " + card.title +
        "\n\nCI failed: " + classification.reason +
        "\nRun: " + runUrl +
        (logSnippet ? "\n\nLog excerpt:\n" + logSnippet : "")
      );
      agentdesk.log.info("[ci-recovery] Rework dispatch created for card " + cardId);
    } catch (e) {
      agentdesk.log.warn("[ci-recovery] Rework dispatch failed: " + e);
      escalateToPendingDecision(cardId, "CI code failure — rework dispatch failed: " + e);
      return;
    }

    // Move card back to in_progress for rework
    agentdesk.db.execute(
      "UPDATE kanban_cards SET blocked_reason = 'ci:rework' WHERE id = ?",
      [cardId]
    );
    var cfg = agentdesk.pipeline.resolveForCard(cardId);
    var init = agentdesk.pipeline.kickoffState(cfg);
    var ip = agentdesk.pipeline.nextGatedTarget(init, cfg);
    agentdesk.kanban.setStatus(cardId, ip);
    agentdesk.log.info("[ci-recovery] Card " + cardId + " → " + ip + " for CI rework");

    // Cleanup retry state since this is a code fix path
    agentdesk.kv.delete("ci:" + cardId + ":retry_count");
    agentdesk.kv.delete("ci:" + cardId + ":last_run_id");

  } else {
    // ambiguous
    escalateToPendingDecision(cardId,
      "CI failure — ambiguous classification for run " + runId + ": " + classification.reason);
  }
}

// ── Policy ──────────────────────────────────────────────────

var ciRecovery = {
  name: "ci-recovery",
  priority: 46,

  onTick1min: function() {
    // Find all cards waiting for CI
    var cards = agentdesk.db.query(
      "SELECT id, blocked_reason FROM kanban_cards WHERE blocked_reason LIKE 'ci:%'",
      []
    );

    if (cards.length === 0) return;

    for (var i = 0; i < cards.length; i++) {
      var cardId = cards[i].id;
      var blockedReason = cards[i].blocked_reason;

      // Skip cards in ci:rework — they are being fixed by the agent
      if (blockedReason === "ci:rework") continue;

      try {
        processWaitingCard(cardId, blockedReason);
      } catch (e) {
        agentdesk.log.error("[ci-recovery] Error processing card " + cardId + ": " + e);
      }
    }
  }
};

agentdesk.registerPolicy(ciRecovery);
