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
 *      - ambiguous/exhausted: escalate to manual intervention
 */

var prTracking = agentdesk.prTracking;

var CI_MAX_RETRIES = 3;
var CI_LOG_MAX_LINES = 50;
var CI_DISPATCH_CARD_TITLE_MAX_CHARS = 120;
var CI_DISPATCH_JOB_NAME_MAX_CHARS = 60;
var CI_RUN_SUMMARY_MAX_CHARS = 240;

function truncateText(text, maxChars) {
  var normalized = String(text || "");
  if (maxChars <= 0) {
    return "";
  }
  if (normalized.length <= maxChars) {
    return normalized;
  }
  if (maxChars === 1) {
    return "…";
  }
  return normalized.substring(0, maxChars - 1) + "…";
}

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
  "dashboard",
  "check",
  "test",
  "lint",
  "build",
  "compile",
  "clippy",
  "high-risk",
  "recovery",
  "scripts"
];

var MANUAL_SUMMARY_PATTERNS = [
  {
    code: "workflow_file_issue",
    pattern: "workflow file issue",
    detail: "workflow file issue"
  },
  {
    code: "workflow_invalid",
    pattern: "workflow is not valid",
    detail: "workflow is not valid"
  },
  {
    code: "workflow_missing",
    pattern: "workflow was not found",
    detail: "workflow was not found"
  },
  {
    code: "workflow_disabled",
    pattern: "workflow does not exist or does not have a workflow_dispatch trigger",
    detail: "workflow configuration does not permit rerun"
  }
];

function compactCiDetail(text) {
  var normalized = String(text || "").replace(/\s+/g, " ").trim();
  if (!normalized) return "";
  if (normalized.length <= CI_RUN_SUMMARY_MAX_CHARS) {
    return normalized;
  }
  return normalized.substring(0, CI_RUN_SUMMARY_MAX_CHARS - 1) + "…";
}

function buildClassificationReason(code, detail) {
  if (!detail) return code;
  return code + ": " + detail;
}

function classifyManualFromSummary(summaryText) {
  var normalized = compactCiDetail(summaryText);
  var summaryLower = normalized.toLowerCase();
  if (!summaryLower) return null;
  for (var i = 0; i < MANUAL_SUMMARY_PATTERNS.length; i++) {
    if (summaryLower.indexOf(MANUAL_SUMMARY_PATTERNS[i].pattern) >= 0) {
      return {
        type: "manual_intervention",
        reasonCode: MANUAL_SUMMARY_PATTERNS[i].code,
        reason: buildClassificationReason(
          MANUAL_SUMMARY_PATTERNS[i].code,
          MANUAL_SUMMARY_PATTERNS[i].detail
        ),
        summary: normalized
      };
    }
  }
  return null;
}

function getRepoForCard(cardId) {
  return prTracking.repoForCard(cardId);
}

function loadPrTracking(cardId) {
  return prTracking.load(cardId, { fallback_state: "wait-ci" });
}

function upsertPrTracking(cardId, repoId, worktreePath, branch, prNumber, headSha, state, lastError) {
  return prTracking.upsert(cardId, repoId, worktreePath, branch, prNumber, headSha, state, lastError);
}

// ── Helper: Find canonical PR info for card via pr_tracking ──

function findPrInfoForCard(cardId) {
  return prTracking.resolvePrInfoForCard(cardId, { fallback_state: "wait-ci" });
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
    return {
      type: "retryable_transient",
      reasonCode: "run_" + conclusion,
      reason: buildClassificationReason("run_" + conclusion, "run conclusion " + conclusion)
    };
  }

  // Get failed jobs
  var jobsJson = agentdesk.exec("gh", [
    "run", "view", String(runId),
    "--repo", repo,
    "--json", "jobs"
  ]);

  var failedJobs = [];
  var jobsUnavailable = false;
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
      jobsUnavailable = true;
      agentdesk.log.warn("[ci-recovery] Failed to parse jobs for run " + runId + ": " + e);
    }
  } else {
    jobsUnavailable = true;
  }

  // Get log excerpt (last CI_LOG_MAX_LINES lines of failed log)
  var logExcerpt = "";
  var logUnavailable = false;
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
    if (logExcerpt.toLowerCase().indexOf("log not found") >= 0) {
      logUnavailable = true;
    }
  } else {
    logUnavailable = true;
  }

  var runSummary = "";
  var runSummaryResult = agentdesk.exec("gh", [
    "run", "view", String(runId),
    "--repo", repo
  ]);
  if (runSummaryResult && runSummaryResult.indexOf("ERROR") !== 0) {
    runSummary = compactCiDetail(runSummaryResult);
  }

  var manualSummary = classifyManualFromSummary(runSummary);
  if (manualSummary) {
    manualSummary.failedJobs = failedJobs;
    manualSummary.logExcerpt = logExcerpt;
    return manualSummary;
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
    return {
      type: "retryable_transient",
      reasonCode: "transient_log_pattern",
      reason: buildClassificationReason(
        "transient_log_pattern",
        "matched transient failure pattern"
      ),
      logExcerpt: logExcerpt
    };
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
      reasonCode: "code_job_match",
      reason: buildClassificationReason(
        "code_job_match",
        "failed jobs=" + failedJobs.join(", ")
      ),
      failedJobs: failedJobs,
      logExcerpt: logExcerpt
    };
  }

  // No failed jobs found — keep this explicit so manual recovery gets the real cause.
  if (failedJobs.length === 0) {
    var details = [];
    if (jobsUnavailable) details.push("jobs unavailable");
    if (logUnavailable) details.push("failed log unavailable");
    if (runSummary) details.push("summary=" + runSummary);
    return {
      type: "manual_intervention",
      reasonCode: "missing_failed_job_metadata",
      reason: buildClassificationReason(
        "missing_failed_job_metadata",
        details.length > 0 ? details.join("; ") : "failed job metadata unavailable"
      ),
      logExcerpt: logExcerpt,
      summary: runSummary
    };
  }

  // Unknown jobs should be escalated with the job list inline instead of a generic ambiguous bucket.
  return {
    type: "manual_intervention",
    reasonCode: "unclassified_failed_jobs",
    reason: buildClassificationReason(
      "unclassified_failed_jobs",
      "jobs=" + failedJobs.join(", ")
    ),
    failedJobs: failedJobs,
    logExcerpt: logExcerpt,
    summary: runSummary
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

// ── Manual decision escalation ──

function escalateToManualDecision(cardId, reason) {
  escalateToManualIntervention(cardId, reason);
  agentdesk.log.warn("[ci-recovery] Card " + cardId + " escalated to manual intervention: " + reason);
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
  var tracked = loadPrTracking(cardId) || {};
  var storedSha = tracked.head_sha || agentdesk.kv.get("ci:" + cardId + ":head_sha");
  var currentSha = getCurrentPrSha(pr.number, repo);
  if (currentSha && storedSha && currentSha !== storedSha) {
    agentdesk.log.info("[ci-recovery] Head SHA changed for card " + cardId + " — resetting recovery state");
    agentdesk.kv.set("ci:" + cardId + ":retry_count", "0", 86400);
    agentdesk.kv.delete("ci:" + cardId + ":last_run_id");
    upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha, "wait-ci", null);
    agentdesk.db.execute(
      "UPDATE kanban_cards SET blocked_reason = 'ci:waiting' WHERE id = ?",
      [cardId]
    );
  }
  if (currentSha) {
    agentdesk.kv.set("ci:" + cardId + ":head_sha", currentSha, 86400);
    upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha, "wait-ci", null);
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

  // Prefer the most recent run for the tracked head SHA.
  var run = runs[0];
  if (currentSha) {
    for (var idx = 0; idx < runs.length; idx++) {
      if (runs[idx].headSha === currentSha) {
        run = runs[idx];
        break;
      }
    }
  }
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
    upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha || pr.sha, "merge", null);
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
        upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha || pr.sha, "escalated", "CI rerun failed: " + rerunResult);
        escalateToManualDecision(cardId, "CI rerun failed: " + rerunResult);
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
      upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha || pr.sha, "escalated", "CI transient failure — max retries exhausted");
      escalateToManualDecision(cardId,
        "CI transient failure — max retries (" + CI_MAX_RETRIES + ") exhausted for run " + runId);
    }

  } else if (classification.type === "code_failure") {
    // Create rework dispatch to assigned agent
    var cards = agentdesk.db.query(
      "SELECT assigned_agent_id, title, github_issue_number FROM kanban_cards WHERE id = ?",
      [cardId]
    );
    if (cards.length === 0 || !cards[0].assigned_agent_id) {
      upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha || pr.sha, "escalated", "CI code failure but no assigned agent");
      escalateToManualDecision(cardId, "CI code failure but no assigned agent");
      return;
    }

    var card = cards[0];
    var issueNum = card.github_issue_number || "?";
    var runUrl = "https://github.com/" + repo + "/actions/runs/" + runId;
    var failedJobName = (classification.failedJobs && classification.failedJobs.length > 0)
      ? classification.failedJobs[0]
      : "unknown job";
    var compactCardTitle = truncateText(card.title || "Untitled card", CI_DISPATCH_CARD_TITLE_MAX_CHARS);
    var compactFailedJobName = truncateText(failedJobName, CI_DISPATCH_JOB_NAME_MAX_CHARS);

    // Keep log excerpt in dispatch context, not in the Discord-visible title.
    var logSnippet = classification.logExcerpt || "";
    if (logSnippet.length > 1200) {
      logSnippet = logSnippet.substring(logSnippet.length - 1200);
    }

    var dispatchContext = {
      ci_recovery: {
        job_name: compactFailedJobName,
        reason: classification.reason,
        run_url: runUrl,
        log_excerpt: logSnippet
      },
      target_repo: repo
    };
    if (pr.worktree_path) {
      dispatchContext.worktree_path = pr.worktree_path;
    }
    if (branch) {
      dispatchContext.worktree_branch = branch;
    }

    try {
      agentdesk.dispatch.create(
        cardId,
        card.assigned_agent_id,
        "rework",
        "[CI Fix] #" + issueNum + " " + compactCardTitle + " — " + compactFailedJobName,
        dispatchContext
      );
      agentdesk.log.info("[ci-recovery] Rework dispatch created for card " + cardId);
    } catch (e) {
      agentdesk.log.warn("[ci-recovery] Rework dispatch failed: " + e);
      upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha || pr.sha, "escalated", "CI rework dispatch failed: " + e);
      escalateToManualDecision(cardId, "CI code failure — rework dispatch failed: " + e);
      return;
    }

    // Move card back to in_progress for rework
    upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha || pr.sha, "wait-ci", "CI code failure: " + classification.reason);
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

  } else if (classification.type === "manual_intervention") {
    upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha || pr.sha, "escalated", "CI manual intervention: " + classification.reason);
    escalateToManualDecision(cardId,
      "CI failure — manual intervention required for run " + runId + ": " + classification.reason);

  } else {
    // Final fallback — keep ambiguous as a last-resort bucket only.
    upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha || pr.sha, "escalated", "CI failure ambiguous: " + classification.reason);
    escalateToManualDecision(cardId,
      "CI failure — ambiguous classification for run " + runId + ": " + classification.reason);
  }
}

// ── Policy ──────────────────────────────────────────────────

var ciRecovery = {
  name: "ci-recovery",
  priority: 46,

  onTick1min: function() {
    prTracking.importLegacyOnce("wait-ci");

    // Find canonical PR lifecycle entries that are waiting for CI.
    var cards = agentdesk.db.query(
      "SELECT p.card_id AS id, c.blocked_reason AS blocked_reason " +
      "FROM pr_tracking p " +
      "JOIN kanban_cards c ON c.id = p.card_id " +
      "WHERE p.state = 'wait-ci'",
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
