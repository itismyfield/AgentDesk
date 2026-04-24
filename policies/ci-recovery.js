/**
 * ci-recovery.js — ADK Policy: CI Failure Auto-Recovery (#257)
 * priority: 46 (between pipeline policies=45 and review-automation=50)
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
var CI_LOOP_SUPPRESS_ESCALATION_THRESHOLD = 3;

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

function ciLoopBaseFingerprint(cardId, headSha, runId) {
  return [
    String(cardId || "unknown-card"),
    String(headSha || "unknown-head"),
    String(runId || "unknown-run")
  ].join("::");
}

function ciLoopFingerprint(cardId, headSha, runId, classificationType) {
  return ciLoopBaseFingerprint(cardId, headSha, runId) + "::" + String(classificationType || "unknown");
}

function replaceCiLoopState(cardId, record) {
  var next = mergeObjectPatch({}, record || {});
  next.updated_at = loopGuardNowIso();
  return replaceLoopGuardRecord(cardId, "ci_recovery", next, LOOP_GUARD_TTL_SEC);
}

function noteCiLoopReset(cardId, headSha, reason) {
  return replaceCiLoopState(cardId, {
    status: "reset",
    action: "reset",
    fingerprint: null,
    base_fingerprint: null,
    classification: null,
    run_id: null,
    head_sha: headSha || null,
    suppress_count: 0,
    last_reason: reason,
    reset_reason: reason,
    last_seen_at: loopGuardNowIso(),
    escalated_at: null,
    escalation_reason: null
  });
}

function maybeSuppressDuplicateCodeFailure(cardId, repo, pr, branch, headSha, runId, classification, blockedReason) {
  var fingerprint = ciLoopFingerprint(cardId, headSha, runId, classification.type);
  var state = loadLoopGuardRecord(cardId, "ci_recovery");
  if (!state || state.fingerprint !== fingerprint || state.action !== "rework_dispatched") {
    return {
      suppressed: false,
      fingerprint: fingerprint,
      baseFingerprint: ciLoopBaseFingerprint(cardId, headSha, runId)
    };
  }

  var suppressCount = Number(state.suppress_count || 0) + 1;
  var nowIso = loopGuardNowIso();
  var next = replaceCiLoopState(cardId, {
    status: "suppressed",
    action: "rework_dispatched",
    fingerprint: fingerprint,
    base_fingerprint: ciLoopBaseFingerprint(cardId, headSha, runId),
    classification: classification.type,
    run_id: String(runId),
    head_sha: headSha || null,
    suppress_count: suppressCount,
    last_reason: classification.reason,
    last_seen_at: nowIso,
    blocked_reason: blockedReason || null,
    first_seen_at: state.first_seen_at || nowIso,
    escalation_reason: state.escalation_reason || null,
    escalated_at: state.escalated_at || null
  });
  agentdesk.log.warn(
    "[ci-recovery] Suppressed duplicate code_failure fingerprint for card " + cardId +
    ": " + fingerprint + " (count " + suppressCount + ")"
  );

  if (suppressCount < CI_LOOP_SUPPRESS_ESCALATION_THRESHOLD) {
    return {
      suppressed: true,
      escalated: false,
      fingerprint: fingerprint,
      baseFingerprint: next.base_fingerprint
    };
  }

  var shortSha = headSha ? String(headSha).substring(0, 12) : "unknown";
  var escalationReason =
    "CI loop guard: identical failed run kept re-entering recovery " +
    "(run " + runId + ", head " + shortSha + ", classification " + classification.type +
    ", suppress_count=" + suppressCount + ")";
  replaceCiLoopState(cardId, {
    status: "escalated",
    action: "rework_dispatched",
    fingerprint: fingerprint,
    base_fingerprint: next.base_fingerprint,
    classification: classification.type,
    run_id: String(runId),
    head_sha: headSha || null,
    suppress_count: suppressCount,
    last_reason: classification.reason,
    last_seen_at: nowIso,
    blocked_reason: blockedReason || null,
    first_seen_at: next.first_seen_at || nowIso,
    escalation_reason: escalationReason,
    escalated_at: nowIso
  });
  upsertPrTracking(
    cardId,
    repo,
    pr ? pr.worktree_path : null,
    branch,
    pr ? pr.number : null,
    headSha || (pr ? pr.sha : null),
    "escalated",
    escalationReason
  );
  // typed-facade-slice:start ci-recovery (#1007)
  var cardStatus = agentdesk.ciRecovery.getCardStatus(cardId);
  // typed-facade-slice:end ci-recovery
  var opts = {};
  if (cardStatus && cardStatus.status === "review") {
    opts.review = true;
  }
  escalateToManualIntervention(cardId, escalationReason, opts);
  return {
    suppressed: true,
    escalated: true,
    fingerprint: fingerprint,
    baseFingerprint: next.base_fingerprint
  };
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
    noteCiLoopReset(cardId, currentSha, "head_sha_changed");
    upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha, "wait-ci", null);
    agentdesk.ciRecovery.setBlockedReason(cardId, "ci:waiting");
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
      agentdesk.ciRecovery.setBlockedReason(cardId, "ci:running");
    }
    return;
  }

  // Mark as processed
  agentdesk.kv.set("ci:" + cardId + ":last_run_id", String(runId), 86400);

  // ── CI passed ──
  if (run.conclusion === "success") {
    agentdesk.log.info("[ci-recovery] CI passed for card " + cardId + " (run " + runId + ")");
    replaceCiLoopState(cardId, {
      status: "success",
      action: "ci_passed",
      fingerprint: ciLoopFingerprint(cardId, currentSha || run.headSha || pr.sha, runId, "success"),
      base_fingerprint: ciLoopBaseFingerprint(cardId, currentSha || run.headSha || pr.sha, runId),
      classification: "success",
      run_id: String(runId),
      head_sha: currentSha || run.headSha || pr.sha || null,
      suppress_count: 0,
      last_reason: "success",
      last_seen_at: loopGuardNowIso(),
      escalation_reason: null,
      escalated_at: null
    });
    upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha || pr.sha, "merge", null);
    agentdesk.ciRecovery.setBlockedReason(cardId, null);
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
  var effectiveHeadSha = currentSha || run.headSha || pr.sha || null;
  var baseFingerprint = ciLoopBaseFingerprint(cardId, effectiveHeadSha, runId);
  var failureFingerprint = ciLoopFingerprint(cardId, effectiveHeadSha, runId, classification.type);
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
      replaceCiLoopState(cardId, {
        status: "active",
        action: "rerun_requested",
        fingerprint: failureFingerprint,
        base_fingerprint: baseFingerprint,
        classification: classification.type,
        run_id: String(runId),
        head_sha: effectiveHeadSha,
        suppress_count: 0,
        last_reason: classification.reason,
        last_seen_at: loopGuardNowIso(),
        escalation_reason: null,
        escalated_at: null
      });
      agentdesk.ciRecovery.setBlockedReason(cardId, "ci:rerunning");
      // Clear last_run_id so we re-evaluate the new run
      agentdesk.kv.delete("ci:" + cardId + ":last_run_id");
      agentdesk.log.info("[ci-recovery] Rerunning failed jobs for card " + cardId + " (retry " + (retryCount + 1) + "/" + CI_MAX_RETRIES + ")");
    } else {
      upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha || pr.sha, "escalated", "CI transient failure — max retries exhausted");
      escalateToManualDecision(cardId,
        "CI transient failure — max retries (" + CI_MAX_RETRIES + ") exhausted for run " + runId);
    }

  } else if (classification.type === "code_failure") {
    var suppression = maybeSuppressDuplicateCodeFailure(
      cardId,
      repo,
      pr,
      branch,
      effectiveHeadSha,
      runId,
      classification,
      blockedReason
    );
    if (suppression.suppressed) {
      return;
    }

    // Create rework dispatch to assigned agent
    var card = agentdesk.ciRecovery.getReworkCardInfo(cardId);
    if (!card || !card.assigned_agent_id) {
      upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha || pr.sha, "escalated", "CI code failure but no assigned agent");
      escalateToManualDecision(cardId, "CI code failure but no assigned agent");
      return;
    }

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
    replaceCiLoopState(cardId, {
      status: "active",
      action: "rework_dispatched",
      fingerprint: suppression.fingerprint,
      base_fingerprint: suppression.baseFingerprint,
      classification: classification.type,
      run_id: String(runId),
      head_sha: effectiveHeadSha,
      suppress_count: 0,
      last_reason: classification.reason,
      last_seen_at: loopGuardNowIso(),
      blocked_reason: "ci:rework",
      first_seen_at: loopGuardNowIso(),
      escalation_reason: null,
      escalated_at: null
    });
    agentdesk.ciRecovery.setBlockedReason(cardId, "ci:rework");
    var cfg = agentdesk.pipeline.resolveForCard(cardId);
    var init = agentdesk.pipeline.kickoffState(cfg);
    var ip = agentdesk.pipeline.nextGatedTarget(init, cfg);
    agentdesk.kanban.setStatus(cardId, ip);
    agentdesk.log.info("[ci-recovery] Card " + cardId + " → " + ip + " for CI rework");

    // Cleanup retry state since this is a code fix path.
    // Keep last_run_id until the PR head SHA changes so the same failed run
    // cannot spawn another rework loop after the card briefly re-enters review.
    agentdesk.kv.delete("ci:" + cardId + ":retry_count");

  } else if (classification.type === "manual_intervention") {
    replaceCiLoopState(cardId, {
      status: "escalated",
      action: "manual_intervention",
      fingerprint: failureFingerprint,
      base_fingerprint: baseFingerprint,
      classification: classification.type,
      run_id: String(runId),
      head_sha: effectiveHeadSha,
      suppress_count: 0,
      last_reason: classification.reason,
      last_seen_at: loopGuardNowIso(),
      escalation_reason: classification.reason,
      escalated_at: loopGuardNowIso()
    });
    upsertPrTracking(cardId, repo, pr.worktree_path, branch, pr.number, currentSha || pr.sha, "escalated", "CI manual intervention: " + classification.reason);
    escalateToManualDecision(cardId,
      "CI failure — manual intervention required for run " + runId + ": " + classification.reason);

  } else {
    // Final fallback — keep ambiguous as a last-resort bucket only.
    replaceCiLoopState(cardId, {
      status: "escalated",
      action: "manual_intervention",
      fingerprint: failureFingerprint,
      base_fingerprint: baseFingerprint,
      classification: classification.type,
      run_id: String(runId),
      head_sha: effectiveHeadSha,
      suppress_count: 0,
      last_reason: classification.reason,
      last_seen_at: loopGuardNowIso(),
      escalation_reason: classification.reason,
      escalated_at: loopGuardNowIso()
    });
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
    var cards = agentdesk.ciRecovery.listWaitingForCi();

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
