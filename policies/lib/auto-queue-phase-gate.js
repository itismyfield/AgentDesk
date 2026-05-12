/** @module policies/lib/auto-queue-phase-gate
 *
 * #1078: Extracted from auto-queue.js as part of the policy modularization pass.
 *
 * Phase-gate verdict / state / failure-handling helpers. The set of helpers
 * here is kept intentionally large because every member of the group either:
 *   - reads or writes the same `auto_queue_phase_gates` DB rows,
 *   - shares the `PHASE_GATE_*` TTL / threshold constants,
 *   - or is part of the verdict-mismatch + auto-close fallback control flow.
 *
 * Splitting them across multiple files would force the cross-module call
 * graph (failure → state → fallback → dispatches) to be rewired and would
 * change the policy semantics by accident. So this module owns:
 *   - failure counter + escalation (`handlePhaseGateFailure`)
 *   - human-alert debounce (`_maybeAlertPhaseGateVerdictMismatch`)
 *   - verdict inference + autoclose fallback (`_inferPhaseGatePassVerdict`,
 *     `_phaseGateOnlyIssueClosedFailing`, `_attemptPhaseGateAutoCloseFallback`)
 *   - state load/save/clear and dispatch fanout (`loadPhaseGateState`,
 *     `savePhaseGateState`, `clearPhaseGateState`, `loadPhaseGateDispatches`,
 *     `_buildPhaseGateGroups`, `_phaseGateTitle`, `_createPhaseGateDispatches`)
 *   - run grace-window primitives (`beginPhaseGateGraceWindow`,
 *     `clearPhaseGateGraceWindow`, `runWithinPhaseGateGrace`,
 *     `runHasBlockingPhaseGate`, `_phaseGateRequired`)
 *   - the `pauseRun` bridge used by both phase-gate and lifecycle paths.
 *
 * Depends on the global `agentdesk` surface plus the globals injected by
 * the policy harness (`notifyHumanAlert`, `notifyCardOwner`, `autoQueueLog`).
 */

var _autoQueueLogLib = require("./auto-queue-log");
var autoQueueLog = _autoQueueLogLib.autoQueueLog;

var PHASE_GATE_HUMAN_ESCALATION_THRESHOLD = 3;
var PHASE_GATE_FAILURE_TTL_SEC = 7 * 24 * 60 * 60;
// #2035: debounce phase-gate verdict-mismatch discord alerts to one per
// run per hour so operators are not spammed when polling churns.
var PHASE_GATE_ALERT_DEBOUNCE_TTL_SEC = 60 * 60;
// #2035: prevent the auto-close fallback from running more than once per
// (card, phase, commit) so a misbehaving check cannot loop-close the issue.
var PHASE_GATE_AUTOCLOSE_TTL_SEC = 24 * 60 * 60;
// #747 round-2: Phase-gate race protection.
// Tick hooks now run on a separate `PolicyEngine` from `onCardTerminal`, so
// `onTick1min.finalizeRunWithoutPhaseGate` can see a run with no
// pending/dispatched entries AFTER Rust marks the last entry `done` but
// BEFORE the main engine's `onCardTerminal` has finished creating phase-gate
// dispatches. Mark a short grace window in the DB at the start of
// `continueRunAfterEntry` and respect it in finalization.
var PHASE_GATE_GRACE_WINDOW_MS = 30 * 1000; // 30s

// #699 (round 2): mirror of src/dispatch/dispatch_status.rs
// maybe_inject_phase_gate_verdict. Infers `pass_verdict` for a phase-gate
// result only when (a) no explicit verdict/decision is present, (b) every
// declared `context.phase_gate.checks` entry is present in `result.checks`
// and passes, and (c) every present entry passes. Returns the inferred
// verdict or null. Pure function — caller applies the value.
function _inferPhaseGatePassVerdict(ctx, result) {
  if (!result || typeof result !== "object") return null;
  var phaseGate = ctx && ctx.phase_gate;
  if (!phaseGate || typeof phaseGate !== "object") return null;

  var explicit = result.verdict || result.decision;
  if (explicit) return null;

  var checks = result.checks;
  if (!checks || typeof checks !== "object") return null;

  var checkNames = Object.keys(checks);
  if (checkNames.length === 0) return null;

  function entryIsPass(entry) {
    var entryStatus = null;
    if (entry && typeof entry === "object") {
      entryStatus = entry.status || entry.result || null;
    } else if (typeof entry === "string") {
      entryStatus = entry;
    }
    var normalized = entryStatus ? String(entryStatus).toLowerCase() : null;
    return normalized === "pass" || normalized === "passed";
  }

  var declared = Array.isArray(phaseGate.checks) ? phaseGate.checks : [];
  for (var di = 0; di < declared.length; di++) {
    var required = declared[di];
    if (!required) continue;
    var entry = checks[required];
    if (!entryIsPass(entry)) return null;
  }

  for (var ci = 0; ci < checkNames.length; ci++) {
    if (!entryIsPass(checks[checkNames[ci]])) return null;
  }

  return phaseGate.pass_verdict || "phase_gate_passed";
}

function phaseGateFailureKey(cardId, phase) {
  return "phase_gate_failure:" + cardId + ":" + phase;
}

function incrementPhaseGateFailureCount(cardId, phase) {
  if (!cardId) return 0;
  var key = phaseGateFailureKey(cardId, phase);
  var current = parseInt(agentdesk.kv.get(key) || "0", 10);
  if (!current || current < 0) current = 0;
  var next = current + 1;
  agentdesk.kv.set(key, String(next), PHASE_GATE_FAILURE_TTL_SEC);
  return next;
}

function resetPhaseGateFailureCount(cardId, phase) {
  if (!cardId) return;
  agentdesk.kv.delete(phaseGateFailureKey(cardId, phase));
}

function loadPhaseGateCardLabel(cardId) {
  if (!cardId) return "unknown card";
  var card = agentdesk.cards.get(cardId);
  if (!card) return cardId;
  if (card.github_issue_number) {
    return "#" + card.github_issue_number + " " + (card.title || card.id);
  }
  return card.title || card.id;
}

function handlePhaseGateFailure(cardId, phase, reason, context) {
  var failureCount = incrementPhaseGateFailureCount(cardId, phase);
  notifyCardOwner(cardId, reason, "auto-queue");
  if (failureCount >= PHASE_GATE_HUMAN_ESCALATION_THRESHOLD) {
    notifyHumanAlert(
      "⚠️ [Phase Gate] " + loadPhaseGateCardLabel(cardId) + "\n" +
        "phase " + phase + " 실패가 " + failureCount + "회 누적되었습니다.\n" +
        reason + "\n" +
        "사람 확인이 필요합니다.",
      "auto-queue"
    );
  }
  autoQueueLog("warn", "Phase gate failure recorded for card " + (cardId || "unknown") + " phase " + phase + " count=" + failureCount, context);
  return failureCount;
}

// #2035: emit a Discord alert when phase-gate evaluation pauseRun()s the
// queue due to a verdict mismatch. Debounced to one alert per run+phase
// per hour so periodic re-evaluations do not spam the channel.
function _maybeAlertPhaseGateVerdictMismatch(runId, phase, cardId, reason) {
  if (!runId) return;
  var key = "phase_gate_verdict_alert:" + runId + ":" + phase;
  var existing = agentdesk.kv.get(key);
  if (existing) return;
  agentdesk.kv.set(key, String(Date.now()), PHASE_GATE_ALERT_DEBOUNCE_TTL_SEC);
  notifyHumanAlert(
    "⏸ [Phase Gate] " + loadPhaseGateCardLabel(cardId) + "\n" +
      "run " + (runId || "?").substring(0, 8) + " phase " + phase + " 가 verdict mismatch 로 정지되었습니다.\n" +
      (reason || "(no reason)") + "\n" +
      "운영자 확인 후 /api/queue/resume 호출이 필요합니다.",
    "auto-queue"
  );
}

// #2035: inspect phase-gate result.checks and decide whether this is a
// failure caused ONLY by the issue_closed check (i.e. merge_verified and
// build_passed both passed but the GitHub issue is still open because the
// commit message did not contain a Closes/Fixes/Resolves keyword).
// Returns { eligible: bool, reason: string } so the caller can log the
// rationale even when fallback is declined.
function _phaseGateOnlyIssueClosedFailing(context, result) {
  if (!context || !context.phase_gate) {
    return { eligible: false, reason: "no_phase_gate_context" };
  }
  var checks = (result && result.checks) || null;
  if (!checks || typeof checks !== "object") {
    return { eligible: false, reason: "no_checks" };
  }
  function entryStatus(entry) {
    if (!entry) return null;
    if (typeof entry === "string") return entry.toLowerCase();
    if (typeof entry === "object") {
      var s = entry.status || entry.result || null;
      return s ? String(s).toLowerCase() : null;
    }
    return null;
  }
  var declared = Array.isArray(context.phase_gate.checks) ? context.phase_gate.checks : [];
  var names = declared.length > 0 ? declared : Object.keys(checks);
  var sawIssueClosedFail = false;
  var sawMergeVerifiedPass = false;
  var sawBuildPassedPass = false;
  for (var i = 0; i < names.length; i++) {
    var name = names[i];
    var status = entryStatus(checks[name]);
    if (name === "issue_closed") {
      if (status !== "fail" && status !== "failed") {
        return { eligible: false, reason: "issue_closed_not_failing" };
      }
      sawIssueClosedFail = true;
    } else if (name === "merge_verified") {
      if (status !== "pass" && status !== "passed") {
        return { eligible: false, reason: "merge_verified_not_passing" };
      }
      sawMergeVerifiedPass = true;
    } else if (name === "build_passed") {
      if (status !== "pass" && status !== "passed") {
        return { eligible: false, reason: "build_passed_not_passing" };
      }
      sawBuildPassedPass = true;
    } else if (status !== "pass" && status !== "passed") {
      // Any other unrecognized check failing → bail; we only auto-recover
      // the well-understood issue_closed-only case.
      return { eligible: false, reason: "other_check_failing:" + name };
    }
  }
  if (!sawIssueClosedFail || !sawMergeVerifiedPass || !sawBuildPassedPass) {
    return { eligible: false, reason: "missing_required_signals" };
  }
  return { eligible: true, reason: "issue_closed_only_failure" };
}

// #2035: locate the GitHub repo slug + commit hash for a card so the
// fallback close can be cross-checked (same commit hash) and the gh CLI
// can target the right repo.
function _loadCardForPhaseGateFallback(cardId) {
  if (!cardId) return null;
  /* legacy-raw-db: policy=auto-queue capability=phase_gate_autoclose source_event=verdict_mismatch */
  var rows = agentdesk.db.query(
    "SELECT id, github_issue_number, github_issue_url, last_commit_sha, pr_merge_commit_sha, " +
    "       issue_closed_at " +
    "FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (!rows || rows.length === 0) return null;
  return rows[0];
}

function _extractRepoSlugFromIssueUrl(url) {
  if (!url || typeof url !== "string") return null;
  var match = url.match(/github\.com\/([^\/]+\/[^\/]+)\/issues\//);
  return match ? match[1] : null;
}

// #2035: best-effort GitHub auto-close fallback. Requires (a) merge pass +
// build pass + ONLY issue_closed failing, (b) same commit hash present in
// kanban_cards (so we don't close on stale dispatch state), and
// (c) one-shot per (card, phase, commit). Returns true if the close
// command was issued (caller still re-evaluates the gate).
function _attemptPhaseGateAutoCloseFallback(runId, phase, dispatchId, context, result) {
  var eligibility = _phaseGateOnlyIssueClosedFailing(context, result);
  if (!eligibility.eligible) {
    return { attempted: false, reason: eligibility.reason };
  }
  var gate = context && context.phase_gate;
  var cardIds = (gate && Array.isArray(gate.card_ids)) ? gate.card_ids : [];
  if (cardIds.length === 0) {
    return { attempted: false, reason: "no_card_ids_in_phase_gate" };
  }
  var anyClosed = false;
  var attempted = false;
  for (var i = 0; i < cardIds.length; i++) {
    var cardId = cardIds[i];
    var card = _loadCardForPhaseGateFallback(cardId);
    if (!card || !card.github_issue_number || !card.github_issue_url) continue;
    if (card.issue_closed_at) {
      anyClosed = true;
      continue;
    }
    var commitHash = card.pr_merge_commit_sha || card.last_commit_sha || null;
    if (!commitHash) {
      autoQueueLog("info", "Phase gate autoclose skipped — no commit hash for card " + cardId, {
        run_id: runId,
        dispatch_id: dispatchId,
        card_id: cardId,
        batch_phase: phase
      });
      continue;
    }
    var dedupeKey = "phase_gate_autoclose:" + cardId + ":" + phase + ":" + commitHash;
    if (agentdesk.kv.get(dedupeKey)) {
      autoQueueLog("info", "Phase gate autoclose already attempted for card " + cardId + " commit " + commitHash.substring(0, 8), {
        run_id: runId,
        dispatch_id: dispatchId,
        card_id: cardId,
        batch_phase: phase
      });
      continue;
    }
    var repo = _extractRepoSlugFromIssueUrl(card.github_issue_url);
    if (!repo) {
      autoQueueLog("warn", "Phase gate autoclose skipped — could not parse repo from issue url for card " + cardId, {
        run_id: runId,
        dispatch_id: dispatchId,
        card_id: cardId,
        batch_phase: phase
      });
      continue;
    }
    agentdesk.kv.set(dedupeKey, String(Date.now()), PHASE_GATE_AUTOCLOSE_TTL_SEC);
    attempted = true;
    try {
      agentdesk.exec(
        "gh",
        [
          "issue", "close", String(card.github_issue_number),
          "--repo", repo,
          "--reason", "completed",
          "--comment",
          "Auto-closed by phase-gate fallback (#2035) — merge_verified=pass, build_passed=pass, commit " + commitHash.substring(0, 8) +
          ". Commit message did not contain a GitHub auto-close keyword."
        ],
        { timeout_ms: 15000 }
      );
      anyClosed = true;
      autoQueueLog("info", "Phase gate autoclose issued for card " + cardId + " issue #" + card.github_issue_number + " commit " + commitHash.substring(0, 8), {
        run_id: runId,
        dispatch_id: dispatchId,
        card_id: cardId,
        batch_phase: phase
      });
    } catch (e) {
      autoQueueLog("warn", "Phase gate autoclose gh exec failed for card " + cardId + ": " + e, {
        run_id: runId,
        dispatch_id: dispatchId,
        card_id: cardId,
        batch_phase: phase
      });
    }
  }
  return { attempted: attempted, anyClosed: anyClosed, reason: eligibility.reason };
}

function loadPhaseGateState(runId, phase) {
  var rows = agentdesk.db.query(
    "SELECT dispatch_id, status, verdict, pass_verdict, next_phase, final_phase, " +
    "anchor_card_id, failure_reason, created_at " +
    "FROM auto_queue_phase_gates " +
    "WHERE run_id = ? AND phase = ? " +
    "ORDER BY CASE WHEN dispatch_id IS NULL THEN 0 ELSE 1 END ASC, created_at ASC, dispatch_id ASC",
    [runId, phase]
  );
  if (rows.length === 0) return null;

  var dispatchIds = [];
  var status = "pending";
  var failedRow = null;
  var hasPassedRows = false;

  for (var i = 0; i < rows.length; i++) {
    var row = rows[i];
    if (row.dispatch_id) {
      dispatchIds.push(row.dispatch_id);
    }
    if (row.status === "failed" && !failedRow) {
      failedRow = row;
      status = "failed";
    } else if (row.status === "passed") {
      hasPassedRows = true;
    }
  }

  if (!failedRow && dispatchIds.length > 0 && hasPassedRows && rows.every(function(row) { return row.status === "passed"; })) {
    status = "passed";
  } else if (!failedRow) {
    status = rows[0].status || "pending";
  }

  var state = {
    run_id: runId,
    batch_phase: phase,
    next_phase: rows[0].next_phase,
    final_phase: !!rows[0].final_phase,
    anchor_card_id: rows[0].anchor_card_id || null,
    status: status,
    dispatch_ids: dispatchIds,
    gates: [],
    created_at: rows[0].created_at || null
  };

  if (failedRow) {
    state.failed_dispatch_id = failedRow.dispatch_id || null;
    state.failed_verdict = failedRow.verdict || null;
    state.failed_reason = failedRow.failure_reason || null;
  }

  return state;
}

function savePhaseGateState(runId, phase, state) {
  if (!state) return;
  agentdesk.autoQueue.savePhaseGateState(runId, phase, {
    status: state.status || "pending",
    verdict: state.failed_verdict || state.verdict || null,
    dispatch_ids: Array.isArray(state.dispatch_ids) ? state.dispatch_ids : [],
    pass_verdict: state.pass_verdict ||
      (state.gates && state.gates[0] && state.gates[0].pass_verdict) ||
      "phase_gate_passed",
    next_phase: state.next_phase !== undefined ? state.next_phase : null,
    final_phase: !!state.final_phase,
    anchor_card_id: state.anchor_card_id || null,
    failure_reason: state.failed_reason || null,
    created_at: state.created_at || null
  });
}

function clearPhaseGateState(runId, phase) {
  agentdesk.autoQueue.clearPhaseGateState(runId, phase);
}

function runHasBlockingPhaseGate(runId) {
  var rows = agentdesk.db.query(
    "SELECT COUNT(*) as cnt FROM auto_queue_phase_gates " +
    "WHERE run_id = ? AND status IN ('pending', 'failed')",
    [runId]
  );
  return rows.length > 0 && rows[0].cnt > 0;
}

function beginPhaseGateGraceWindow(runId) {
  if (!runId) return;
  var until = new Date(Date.now() + PHASE_GATE_GRACE_WINDOW_MS).toISOString();
  try {
    agentdesk.db.execute(
      "UPDATE auto_queue_runs SET phase_gate_grace_until = ? WHERE id = ?",
      [until, runId]
    );
  } catch (e) {
    autoQueueLog("warn", "Failed to begin phase-gate grace window for run " + runId + ": " + e, {
      run_id: runId
    });
  }
}

function clearPhaseGateGraceWindow(runId) {
  if (!runId) return;
  try {
    agentdesk.db.execute(
      "UPDATE auto_queue_runs SET phase_gate_grace_until = NULL WHERE id = ?",
      [runId]
    );
  } catch (e) {
    // Non-fatal: grace window will naturally expire.
  }
}

function runWithinPhaseGateGrace(runId) {
  if (!runId) return false;
  var rows = agentdesk.db.query(
    "SELECT phase_gate_grace_until FROM auto_queue_runs WHERE id = ?",
    [runId]
  );
  if (rows.length === 0 || !rows[0].phase_gate_grace_until) return false;
  var until = Date.parse(rows[0].phase_gate_grace_until);
  if (!isFinite(until)) return false;
  return Date.now() < until;
}

function pauseRun(runId, source) {
  if (!runId) return false;
  try {
    var result = agentdesk.autoQueue.pauseRun(runId, source || "policy_pause");
    return !!(result && result.changed);
  } catch (e) {
    autoQueueLog("warn", "Failed to pause run " + runId + ": " + e, {
      run_id: runId
    });
    return false;
  }
}

function loadPhaseGateDispatches(dispatchIds) {
  if (!dispatchIds || dispatchIds.length === 0) return [];
  var placeholders = dispatchIds.map(function() { return "?"; }).join(",");
  return agentdesk.db.query(
    "SELECT id, status, result, context FROM task_dispatches WHERE id IN (" + placeholders + ")",
    dispatchIds
  );
}

function _phaseGateRequired(runId, phase) {
  try {
    var rows = agentdesk.db.query(
      "SELECT COALESCE(review_mode, 'enabled') as review_mode FROM auto_queue_runs WHERE id = ?",
      [runId]
    );
    if (rows.length > 0 && rows[0].review_mode === "disabled") {
      autoQueueLog("info", "Skipping phase gate for review-disabled auto-queue run", {
        run_id: runId,
        batch_phase: phase
      });
      return false;
    }
  } catch (e) {
    autoQueueLog("warn", "Failed to load auto-queue review mode for phase gate decision: " + e, {
      run_id: runId,
      batch_phase: phase
    });
  }

  // General phase gates are required unless the run explicitly disabled review.
  return true;
}

function _buildPhaseGateGroups(runId, phase) {
  var rows = agentdesk.db.query(
    "SELECT e.id as entry_id, e.kanban_card_id, e.agent_id, e.status, e.priority_rank, " +
    "kc.title, kc.github_issue_number, kc.repo_id, " +
    "(SELECT td.result FROM task_dispatches td " +
    " WHERE td.kanban_card_id = e.kanban_card_id " +
    "   AND td.status = 'completed' " +
    "   AND td.result IS NOT NULL " +
    " ORDER BY td.completed_at DESC, td.rowid DESC LIMIT 1) as latest_result " +
    "FROM auto_queue_entries e " +
    "JOIN kanban_cards kc ON kc.id = e.kanban_card_id " +
    "WHERE e.run_id = ? AND COALESCE(e.batch_phase, 0) = ? " +
    "ORDER BY e.agent_id ASC, e.priority_rank ASC",
    [runId, phase]
  );
  var groups = {};
  var ordered = [];

  for (var i = 0; i < rows.length; i++) {
    var row = rows[i];
    var gate = agentdesk.pipeline.resolvePhaseGateForCard(row.kanban_card_id);
    var targetAgentId = gate.dispatch_to === "self" ? row.agent_id : gate.dispatch_to;
    var checks = Array.isArray(gate.checks) ? gate.checks.slice() : [];
    var key = [
      row.agent_id || "",
      targetAgentId || "",
      gate.dispatch_type || "phase-gate",
      gate.pass_verdict || "phase_gate_passed",
      checks.join("|")
    ].join("::");
    if (!groups[key]) {
      groups[key] = {
        source_agent_id: row.agent_id,
        target_agent_id: targetAgentId,
        dispatch_type: gate.dispatch_type || "phase-gate",
        pass_verdict: gate.pass_verdict || "phase_gate_passed",
        checks: checks,
        anchor_card_id: row.kanban_card_id,
        repo_id: row.repo_id || null,
        card_ids: [],
        issue_numbers: [],
        work_items: []
      };
      ordered.push(groups[key]);
    }

    var latestResult = {};
    try { latestResult = JSON.parse(row.latest_result || "{}"); } catch (e) { latestResult = {}; }

    groups[key].card_ids.push(row.kanban_card_id);
    if (row.github_issue_number !== null && row.github_issue_number !== undefined) {
      groups[key].issue_numbers.push(row.github_issue_number);
    }
    groups[key].work_items.push({
      entry_id: row.entry_id,
      card_id: row.kanban_card_id,
      agent_id: row.agent_id,
      status: row.status,
      title: row.title || row.kanban_card_id,
      issue_number: row.github_issue_number,
      completed_branch: latestResult.completed_branch || null,
      completed_worktree_path: latestResult.completed_worktree_path || null
    });
  }

  return ordered;
}

function _phaseGateTitle(group, phase, runId) {
  var issues = group.issue_numbers.filter(function(issue) {
    return issue !== null && issue !== undefined;
  });
  var issueLabel = issues.slice(0, 3).map(function(issue) {
    return "#" + issue;
  }).join(", ");
  if (issues.length > 3) {
    issueLabel += " +" + (issues.length - 3);
  }
  if (!issueLabel) {
    issueLabel = "run " + runId.substring(0, 8);
  }
  return "[" + group.dispatch_type + " P" + phase + "] " + issueLabel;
}

function _createPhaseGateDispatches(runId, phase, nextPhase, finalPhase, anchorCardId) {
  var existing = loadPhaseGateState(runId, phase);
  if (existing) {
    pauseRun(runId);
    autoQueueLog("info", "Phase gate already exists for run " + runId + " phase " + phase, {
      run_id: runId,
      card_id: anchorCardId,
      batch_phase: phase
    });
    return existing;
  }

  var groups = _buildPhaseGateGroups(runId, phase);
  var state = {
    run_id: runId,
    batch_phase: phase,
    next_phase: nextPhase,
    final_phase: !!finalPhase,
    anchor_card_id: anchorCardId,
    status: "pending",
    dispatch_ids: [],
    gates: [],
    created_at: new Date().toISOString()
  };
  pauseRun(runId);

  if (groups.length === 0) {
    state.status = "failed";
    state.failed_reason = "no phase gate targets found";
    savePhaseGateState(runId, phase, state);
    handlePhaseGateFailure(
      anchorCardId,
      phase,
      "[phase-gate] run " + runId.substring(0, 8) + " phase " + phase + " has no gate targets",
      {
        run_id: runId,
        card_id: anchorCardId,
        batch_phase: phase
      }
    );
    return state;
  }

  var errors = [];
  for (var i = 0; i < groups.length; i++) {
    var group = groups[i];
    try {
      var dispatchId = agentdesk.dispatch.create(
        group.anchor_card_id || anchorCardId,
        group.target_agent_id,
        group.dispatch_type,
        _phaseGateTitle(group, phase, runId),
        {
          auto_queue: true,
          sidecar_dispatch: true,
          phase_gate: {
            run_id: runId,
            batch_phase: phase,
            next_phase: nextPhase,
            final_phase: !!finalPhase,
            source_agent_id: group.source_agent_id,
            target_agent_id: group.target_agent_id,
            dispatch_type: group.dispatch_type,
            pass_verdict: group.pass_verdict,
            checks: group.checks,
            card_ids: group.card_ids,
            issue_numbers: group.issue_numbers,
            work_items: group.work_items,
            expected_gate_count: groups.length
          }
        }
      );
      state.dispatch_ids.push(dispatchId);
      state.gates.push({
        dispatch_id: dispatchId,
        source_agent_id: group.source_agent_id,
        target_agent_id: group.target_agent_id,
        dispatch_type: group.dispatch_type,
        pass_verdict: group.pass_verdict,
        checks: group.checks,
        card_ids: group.card_ids
      });
    } catch (e) {
      errors.push((group.target_agent_id || "unknown") + ": " + e);
    }
  }

  if (errors.length > 0 || state.dispatch_ids.length === 0) {
    state.status = "failed";
    state.failed_reason = errors.join("; ") || "phase gate dispatch creation failed";
    savePhaseGateState(runId, phase, state);
    handlePhaseGateFailure(
      anchorCardId,
      phase,
      "[phase-gate] run " + runId.substring(0, 8) + " phase " + phase + " setup failed: " + state.failed_reason,
      {
        run_id: runId,
        card_id: anchorCardId,
        batch_phase: phase
      }
    );
    autoQueueLog("warn", "Phase gate setup failed for run " + runId + " phase " + phase + ": " + state.failed_reason, {
      run_id: runId,
      card_id: anchorCardId,
      batch_phase: phase
    });
    return state;
  }

  savePhaseGateState(runId, phase, state);
  autoQueueLog("info", "Created " + state.dispatch_ids.length + " phase gate dispatch(es) for run " + runId + " phase " + phase, {
    run_id: runId,
    card_id: anchorCardId,
    batch_phase: phase
  });
  return state;
}

module.exports = {
  PHASE_GATE_HUMAN_ESCALATION_THRESHOLD: PHASE_GATE_HUMAN_ESCALATION_THRESHOLD,
  PHASE_GATE_FAILURE_TTL_SEC: PHASE_GATE_FAILURE_TTL_SEC,
  PHASE_GATE_ALERT_DEBOUNCE_TTL_SEC: PHASE_GATE_ALERT_DEBOUNCE_TTL_SEC,
  PHASE_GATE_AUTOCLOSE_TTL_SEC: PHASE_GATE_AUTOCLOSE_TTL_SEC,
  PHASE_GATE_GRACE_WINDOW_MS: PHASE_GATE_GRACE_WINDOW_MS,
  inferPhaseGatePassVerdict: _inferPhaseGatePassVerdict,
  phaseGateFailureKey: phaseGateFailureKey,
  incrementPhaseGateFailureCount: incrementPhaseGateFailureCount,
  resetPhaseGateFailureCount: resetPhaseGateFailureCount,
  loadPhaseGateCardLabel: loadPhaseGateCardLabel,
  handlePhaseGateFailure: handlePhaseGateFailure,
  maybeAlertPhaseGateVerdictMismatch: _maybeAlertPhaseGateVerdictMismatch,
  phaseGateOnlyIssueClosedFailing: _phaseGateOnlyIssueClosedFailing,
  loadCardForPhaseGateFallback: _loadCardForPhaseGateFallback,
  extractRepoSlugFromIssueUrl: _extractRepoSlugFromIssueUrl,
  attemptPhaseGateAutoCloseFallback: _attemptPhaseGateAutoCloseFallback,
  loadPhaseGateState: loadPhaseGateState,
  savePhaseGateState: savePhaseGateState,
  clearPhaseGateState: clearPhaseGateState,
  runHasBlockingPhaseGate: runHasBlockingPhaseGate,
  beginPhaseGateGraceWindow: beginPhaseGateGraceWindow,
  clearPhaseGateGraceWindow: clearPhaseGateGraceWindow,
  runWithinPhaseGateGrace: runWithinPhaseGateGrace,
  pauseRun: pauseRun,
  loadPhaseGateDispatches: loadPhaseGateDispatches,
  phaseGateRequired: _phaseGateRequired,
  buildPhaseGateGroups: _buildPhaseGateGroups,
  phaseGateTitle: _phaseGateTitle,
  createPhaseGateDispatches: _createPhaseGateDispatches
};
