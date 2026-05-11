/* giant-file-exemption: reason=auto-queue-phase-gate-pending-split ticket=#1078 */
// #1078: log-context + config readers extracted to policies/lib/auto-queue-{log,config}.js
var _autoQueueLogLib = require("./lib/auto-queue-log");
var _autoQueueConfigLib = require("./lib/auto-queue-config");

var _autoQueueHasValue = _autoQueueLogLib.hasValue;
var _autoQueueLogContextKeys = _autoQueueLogLib.logContextKeys;
var _mergeAutoQueueLogContext = _autoQueueLogLib.mergeLogContext;
var _loadAutoQueueEntryLogContext = _autoQueueLogLib.loadEntryLogContext;
var _loadAutoQueueDispatchLogContext = _autoQueueLogLib.loadDispatchLogContext;
var _normalizeAutoQueueLogContext = _autoQueueLogLib.normalizeLogContext;
var _formatAutoQueueLogContext = _autoQueueLogLib.formatLogContext;
var autoQueueLog = _autoQueueLogLib.autoQueueLog;

var PHASE_GATE_HUMAN_ESCALATION_THRESHOLD = 3;
var PHASE_GATE_FAILURE_TTL_SEC = 7 * 24 * 60 * 60;
// #2035: debounce phase-gate verdict-mismatch discord alerts to one per
// run per hour so operators are not spammed when polling churns.
var PHASE_GATE_ALERT_DEBOUNCE_TTL_SEC = 60 * 60;
// #2035: prevent the auto-close fallback from running more than once per
// (card, phase, commit) so a misbehaving check cannot loop-close the issue.
var PHASE_GATE_AUTOCLOSE_TTL_SEC = 24 * 60 * 60;

var configuredAutoQueueMaxEntryRetries = _autoQueueConfigLib.maxEntryRetries;
var configuredStaleDispatchedGraceMinutes = _autoQueueConfigLib.staleDispatchedGraceMinutes;
var configuredStaleDispatchedTerminalStatuses = _autoQueueConfigLib.staleDispatchedTerminalStatuses;
var configuredStaleDispatchedRecoverNullDispatch = _autoQueueConfigLib.staleDispatchedRecoverNullDispatch;
var configuredStaleDispatchedRecoverMissingDispatch = _autoQueueConfigLib.staleDispatchedRecoverMissingDispatch;
var staleDispatchedRecoveryConditionsSql = _autoQueueConfigLib.staleDispatchedRecoveryConditionsSql;

function notifyAutoQueueEntryFailure(stuck, failure) {
  if (!stuck || !failure || failure.to !== "failed" || failure.changed !== true) return;
  notifyHumanAlert(
    "⚠️ [Auto Queue] " + loadPhaseGateCardLabel(stuck.kanban_card_id) + "\n" +
      "entry " + stuck.id + "가 dispatch failure " + failure.retryCount + "/" + failure.retryLimit + "회 누적으로 failed 상태가 되었습니다.\n" +
      "dispatch " + (stuck.dispatch_id || "NULL") + " is orphan/cancelled/failed/phantom\n" +
      "수동 확인이 필요합니다.",
    "auto-queue"
  );
}

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

var autoQueue = {
  name: "auto-queue",
  priority: 500,

  // ── Auto-skip: detect cards progressed outside of auto-queue ──
  // If a pending queue entry's card gets dispatched externally (by PMD, user, etc.),
  // skip the entry so auto-queue doesn't try to dispatch it again.
  onCardTransition: function(payload) {
    if (payload.source === "auto-queue-walk" || payload.source === "auto-queue-generate") {
      return;
    }
    var aqCfg = agentdesk.pipeline.getConfig();
    var aqKickoff = agentdesk.pipeline.kickoffState(aqCfg);
    var aqNext = agentdesk.pipeline.nextGatedTarget(aqKickoff, aqCfg);
    if (payload.to !== aqKickoff && payload.to !== aqNext) return;
    var entries = agentdesk.db.query(
      "SELECT e.id FROM auto_queue_entries e " +
      "WHERE e.kanban_card_id = ? AND e.status = 'pending'",
      [payload.card_id]
    );
    for (var i = 0; i < entries.length; i++) {
      agentdesk.autoQueue.updateEntryStatus(
        entries[i].id,
        "skipped",
        "external_progress"
      );
      autoQueueLog("info", "Skipped entry " + entries[i].id + " — card " + payload.card_id + " progressed externally to " + payload.to, {
        entry_id: entries[i].id,
        card_id: payload.card_id
      });
    }
  },

  // ── Authoritative auto-queue continuation (#110, #140) ──────────────
  // This is the SINGLE path for done → next queued item.
  // Rust transition_status() already marks auto_queue_entries as 'done'
  // before firing OnCardTerminal and now defers final run completion here so
  // single-phase runs can still create a phase gate before they finish.
  // kanban-rules.js does NOT touch auto_queue_entries (removed in #110).
  // #140: Group-aware continuation — dispatches next entry in same group,
  //       and starts new groups when slots become available.
  onCardTerminal: function(payload) {
    var cards = agentdesk.db.query(
      "SELECT assigned_agent_id FROM kanban_cards WHERE id = ?",
      [payload.card_id]
    );
    if (cards.length === 0 || !cards[0].assigned_agent_id) return;

    var agentId = cards[0].assigned_agent_id;

    // #145/#295: Prefer the just-finished `done` entry for continuation. Sibling
    // runs may also be auto-skipped for the same card, but they must not steal
    // continuation from the originating run.
    var doneEntries = agentdesk.db.query(
      "SELECT e.run_id, COALESCE(e.thread_group, 0) as thread_group, COALESCE(e.batch_phase, 0) as batch_phase FROM auto_queue_entries e " +
      "JOIN auto_queue_runs r ON e.run_id = r.id " +
      "WHERE e.kanban_card_id = ? AND e.status IN ('done', 'skipped') " +
      "AND r.status IN ('active', 'paused') " +
      "ORDER BY CASE WHEN e.status = 'done' THEN 0 ELSE 1 END ASC, e.completed_at DESC LIMIT 1",
      [payload.card_id]
    );
    if (!doneEntries || doneEntries.length === 0 || !doneEntries[0] || !doneEntries[0].run_id) {
      return;
    }

    continueRunAfterEntry(
      doneEntries[0].run_id,
      agentId,
      doneEntries[0].thread_group,
      doneEntries[0].batch_phase || 0,
      payload.card_id
    );
  },

  onDispatchCompleted: function(payload) {
    var dispatches = agentdesk.db.query(
      "SELECT id, kanban_card_id, dispatch_type, result, context FROM task_dispatches WHERE id = ?",
      [payload.dispatch_id]
    );
    if (dispatches.length === 0) return;

    var dispatch = dispatches[0];
    var context = {};
    try { context = JSON.parse(dispatch.context || "{}"); } catch (e) { context = {}; }
    var result = {};
    try { result = JSON.parse(dispatch.result || "{}"); } catch (e) { result = {}; }
    var gate = context.phase_gate;
    if (!gate || !gate.run_id || gate.batch_phase == null) {
      return;
    }

    var phase = null;
    if (typeof gate.batch_phase === "number") {
      phase = gate.batch_phase;
    } else if (typeof gate.batch_phase === "string" && gate.batch_phase.trim() !== "") {
      phase = Number(gate.batch_phase);
    }
    if (!Number.isFinite(phase) || Math.floor(phase) !== phase || phase < 0) {
      autoQueueLog("warn", "Ignoring phase gate completion with invalid batch_phase", {
        run_id: gate.run_id,
        dispatch_id: dispatch.id,
        card_id: dispatch.kanban_card_id,
        batch_phase: gate.batch_phase
      });
      return;
    }
    var state = loadPhaseGateState(gate.run_id, phase);
    if (!state || !Array.isArray(state.dispatch_ids) || state.dispatch_ids.indexOf(dispatch.id) < 0) {
      return;
    }
    if (state.status === "failed") {
      autoQueueLog("info", "Ignoring phase gate completion for failed run " + gate.run_id + " phase " + phase, {
        run_id: gate.run_id,
        dispatch_id: dispatch.id,
        card_id: dispatch.kanban_card_id,
        batch_phase: phase
      });
      return;
    }

    var verdict = result.verdict || result.decision || null;
    var passVerdict = gate.pass_verdict || "phase_gate_passed";

    // #699 fallback: legacy callers may have persisted a phase-gate result
    // with every declared check passing but no explicit `verdict`. The
    // server finalize path now injects the verdict, but this guard recovers
    // rows stored before the fix shipped. Never infer "pass" when any check
    // reports fail, and never override an explicit verdict/decision.
    if (!verdict) {
      var inferred = _inferPhaseGatePassVerdict(context, result);
      if (inferred) {
        verdict = inferred;
        autoQueueLog("info", "Inferred phase gate verdict '" + inferred + "' for dispatch " + dispatch.id + " (no explicit verdict)", {
          run_id: gate.run_id,
          dispatch_id: dispatch.id,
          card_id: dispatch.kanban_card_id,
          batch_phase: phase
        });
      }
    }

    if (verdict !== passVerdict) {
      // #2035: before failing this gate, try the issue_closed-only fallback.
      // Conservative entry conditions: merge_verified=pass, build_passed=pass,
      // ONLY issue_closed failing, same commit hash recorded on the card, and
      // a per-(card, phase, commit) one-shot guard. The fallback issues a
      // `gh issue close` and then we restart this hook so the gate re-checks
      // against fresh `issue_closed_at` state. Failures fall through to the
      // existing pauseRun path so nothing is silently swallowed.
      var fallback = _attemptPhaseGateAutoCloseFallback(
        gate.run_id, phase, dispatch.id, context, result
      );
      if (fallback.attempted) {
        autoQueueLog("info", "Phase gate autoclose fallback attempted for run " + gate.run_id + " phase " + phase + " — anyClosed=" + !!fallback.anyClosed, {
          run_id: gate.run_id,
          dispatch_id: dispatch.id,
          card_id: state.anchor_card_id || dispatch.kanban_card_id,
          batch_phase: phase
        });
        if (fallback.anyClosed) {
          // Re-evaluate exactly once with the fresh issue_closed_at state.
          // We do NOT pauseRun here so the queue continues if all checks
          // now pass. If they still do not pass, the second invocation will
          // skip the fallback (one-shot guard) and fall through to the
          // original pauseRun path.
          try {
            autoQueue.onDispatchCompleted({ dispatch_id: dispatch.id });
          } catch (e) {
            autoQueueLog("warn", "Phase gate re-evaluation after autoclose threw: " + e, {
              run_id: gate.run_id,
              dispatch_id: dispatch.id,
              card_id: state.anchor_card_id || dispatch.kanban_card_id,
              batch_phase: phase
            });
          }
          return;
        }
      }
      state.status = "failed";
      state.failed_dispatch_id = dispatch.id;
      state.failed_verdict = verdict;
      state.failed_reason = result.summary || result.reason || ("expected " + passVerdict + ", got " + (verdict || "none"));
      savePhaseGateState(gate.run_id, phase, state);
      pauseRun(gate.run_id);
      // #2035: surface verdict mismatch as a discord alert (debounced 1/hr).
      _maybeAlertPhaseGateVerdictMismatch(
        gate.run_id, phase,
        state.anchor_card_id || dispatch.kanban_card_id,
        state.failed_reason
      );
      handlePhaseGateFailure(
        state.anchor_card_id || dispatch.kanban_card_id,
        phase,
        "[phase-gate] phase " + phase + " failed: " + state.failed_reason,
        {
          run_id: gate.run_id,
          dispatch_id: dispatch.id,
          card_id: state.anchor_card_id || dispatch.kanban_card_id,
          batch_phase: phase
        }
      );
      autoQueueLog("warn", "Phase gate failed for run " + gate.run_id + " phase " + phase + ": " + state.failed_reason, {
        run_id: gate.run_id,
        dispatch_id: dispatch.id,
        card_id: state.anchor_card_id || dispatch.kanban_card_id,
        batch_phase: phase
      });
      return;
    }

    var gateDispatches = loadPhaseGateDispatches(state.dispatch_ids);
    if (gateDispatches.length !== state.dispatch_ids.length) {
      state.status = "failed";
      state.failed_dispatch_id = dispatch.id;
      state.failed_reason = "missing phase gate dispatch rows";
      savePhaseGateState(gate.run_id, phase, state);
      pauseRun(gate.run_id);
      handlePhaseGateFailure(
        state.anchor_card_id || dispatch.kanban_card_id,
        phase,
        "[phase-gate] phase " + phase + " failed: missing gate dispatch rows",
        {
          run_id: gate.run_id,
          dispatch_id: dispatch.id,
          card_id: state.anchor_card_id || dispatch.kanban_card_id,
          batch_phase: phase
        }
      );
      return;
    }

    var pendingCount = 0;
    for (var i = 0; i < gateDispatches.length; i++) {
      var gateDispatch = gateDispatches[i];
      if (gateDispatch.status === "pending" || gateDispatch.status === "dispatched") {
        pendingCount++;
        continue;
      }
      var gateContext = {};
      var gateResult = {};
      try { gateContext = JSON.parse(gateDispatch.context || "{}"); } catch (e) { gateContext = {}; }
      try { gateResult = JSON.parse(gateDispatch.result || "{}"); } catch (e) { gateResult = {}; }
      var expectedVerdict = (gateContext.phase_gate && gateContext.phase_gate.pass_verdict) || "phase_gate_passed";
      var gateVerdict = gateResult.verdict || gateResult.decision || null;
      // #699 (round 2): sibling gate dispatches persisted before the server
      // fix shipped will still be missing `verdict`. Re-run the inference
      // here so the aggregate gate evaluation does not trip on legacy rows.
      if (!gateVerdict) {
        var siblingInferred = _inferPhaseGatePassVerdict(gateContext, gateResult);
        if (siblingInferred) {
          gateVerdict = siblingInferred;
          autoQueueLog("info", "Inferred sibling phase gate verdict '" + siblingInferred + "' for dispatch " + gateDispatch.id + " (legacy row, no explicit verdict)", {
            run_id: gate.run_id,
            dispatch_id: gateDispatch.id,
            card_id: state.anchor_card_id || dispatch.kanban_card_id,
            batch_phase: phase
          });
        }
      }
      if (gateDispatch.status !== "completed" || gateVerdict !== expectedVerdict) {
        // #2035: sibling gate path — try the issue_closed-only autoclose
        // fallback before failing. Same one-shot guard as the primary path.
        if (gateDispatch.status === "completed") {
          var siblingFallback = _attemptPhaseGateAutoCloseFallback(
            gate.run_id, phase, gateDispatch.id, gateContext, gateResult
          );
          if (siblingFallback.attempted && siblingFallback.anyClosed) {
            autoQueueLog("info", "Phase gate autoclose fallback attempted for sibling dispatch " + gateDispatch.id + " — anyClosed=true", {
              run_id: gate.run_id,
              dispatch_id: gateDispatch.id,
              card_id: state.anchor_card_id || dispatch.kanban_card_id,
              batch_phase: phase
            });
            try {
              autoQueue.onDispatchCompleted({ dispatch_id: gateDispatch.id });
            } catch (e) {
              autoQueueLog("warn", "Phase gate sibling re-evaluation after autoclose threw: " + e, {
                run_id: gate.run_id,
                dispatch_id: gateDispatch.id,
                card_id: state.anchor_card_id || dispatch.kanban_card_id,
                batch_phase: phase
              });
            }
            return;
          }
        }
        state.status = "failed";
        state.failed_dispatch_id = gateDispatch.id;
        state.failed_verdict = gateVerdict;
        state.failed_reason = gateResult.summary || gateResult.reason || ("gate verdict mismatch for dispatch " + gateDispatch.id);
        savePhaseGateState(gate.run_id, phase, state);
        pauseRun(gate.run_id);
        _maybeAlertPhaseGateVerdictMismatch(
          gate.run_id, phase,
          state.anchor_card_id || dispatch.kanban_card_id,
          state.failed_reason
        );
        handlePhaseGateFailure(
          state.anchor_card_id || dispatch.kanban_card_id,
          phase,
          "[phase-gate] phase " + phase + " failed: " + state.failed_reason,
          {
            run_id: gate.run_id,
            dispatch_id: gateDispatch.id,
            card_id: state.anchor_card_id || dispatch.kanban_card_id,
            batch_phase: phase
          }
        );
        return;
      }
    }

    if (pendingCount > 0) {
      autoQueueLog("info", "Phase gate pass waiting for remaining dispatches: run " + gate.run_id + " phase " + phase + " pending=" + pendingCount, {
        run_id: gate.run_id,
        dispatch_id: dispatch.id,
        card_id: dispatch.kanban_card_id,
        batch_phase: phase
      });
      return;
    }

    clearPhaseGateState(gate.run_id, phase);
    resetPhaseGateFailureCount(state.anchor_card_id || dispatch.kanban_card_id, phase);
    if (state.final_phase || gate.final_phase) {
      completeRunAndNotify(gate.run_id);
      autoQueueLog("info", "Phase gate passed, completed run " + gate.run_id + " at phase " + phase, {
        run_id: gate.run_id,
        dispatch_id: dispatch.id,
        card_id: dispatch.kanban_card_id,
        batch_phase: phase
      });
      return;
    }

    resumeRunAndActivate(gate.run_id, gate.next_phase);
    autoQueueLog("info", "Phase gate passed, resumed run " + gate.run_id + " from phase " + phase + " to " + (gate.next_phase || "next"), {
      run_id: gate.run_id,
      dispatch_id: dispatch.id,
      card_id: dispatch.kanban_card_id,
      batch_phase: phase
    });
  },

  // ── Periodic recovery: dispatch next entry for idle agents (#110, #140, #179) ──
  // Group-aware: finds idle agents with pending entries across all groups.
  // Uses 1min tick instead of 5min for faster recovery.
  onTick1min: function() {
    var tickCfg = agentdesk.pipeline.getConfig();
    var tickKickoff = agentdesk.pipeline.kickoffState(tickCfg);
    var tickInProgress = agentdesk.pipeline.nextGatedTarget(tickKickoff, tickCfg);
    var tickReview = agentdesk.pipeline.nextGatedTarget(tickInProgress, tickCfg);
    var tickActiveStates = [tickKickoff, tickInProgress, tickReview].filter(function(s) { return s; });
    var tickPlaceholders = tickActiveStates.map(function() { return "?"; }).join(",");
    var tickTerminalStates = terminalStatesFromConfig(tickCfg);
    var tickTerminalPlaceholders = tickTerminalStates.map(function() { return "?"; }).join(",");

    // Recovery path 1 (#295): terminal cards should never remain pending in
    // active/paused runs. Clean them before dispatch recovery so they do not
    // get re-dispatched or block their groups.
    var terminalPending = agentdesk.db.query(
      "SELECT e.id, e.kanban_card_id, kc.status, e.run_id " +
      "FROM auto_queue_entries e " +
      "JOIN auto_queue_runs r ON e.run_id = r.id " +
      "JOIN kanban_cards kc ON kc.id = e.kanban_card_id " +
      "WHERE e.status = 'pending' AND r.status IN ('active', 'paused') " +
      "AND kc.status IN (" + tickTerminalPlaceholders + ") " +
      "ORDER BY e.updated_at ASC LIMIT 100",
      tickTerminalStates
    );
    for (var tp = 0; tp < terminalPending.length; tp++) {
      var pending = terminalPending[tp];
      if (!agentdesk.pipeline.isTerminal(pending.status, tickCfg)) continue;
      autoQueueLog("info", "onTick1min: skipping terminal pending entry " + pending.id + " for card " + pending.kanban_card_id + " at " + pending.status, {
        run_id: pending.run_id,
        entry_id: pending.id,
        card_id: pending.kanban_card_id
      });
      agentdesk.autoQueue.updateEntryStatus(
        pending.id,
        "skipped",
        "tick_terminal_cleanup"
      );
    }

    var finishedRuns = agentdesk.db.query(
      "SELECT r.id " +
      "FROM auto_queue_runs r " +
      "WHERE r.status IN ('active', 'paused') " +
      "AND NOT EXISTS (" +
      "  SELECT 1 FROM auto_queue_entries e " +
      "  WHERE e.run_id = r.id AND e.status IN ('pending', 'dispatched')" +
      ") " +
      "AND NOT EXISTS (" +
      "  SELECT 1 FROM auto_queue_entries e " +
      "  WHERE e.run_id = r.id AND e.status = 'user_cancelled'" +
      ") " +
      "AND NOT EXISTS (" +
      "  SELECT 1 FROM auto_queue_phase_gates pg " +
      "  WHERE pg.run_id = r.id AND pg.status IN ('pending', 'failed')" +
      ") " +
      "AND (" +
      "  r.phase_gate_grace_until IS NULL " +
      "  OR datetime(r.phase_gate_grace_until) IS NULL " +
      "  OR datetime(r.phase_gate_grace_until) <= datetime('now')" +
      ") ORDER BY r.id ASC LIMIT 50",
      []
    );
    for (var fr = 0; fr < finishedRuns.length; fr++) {
      finalizeRunWithoutPhaseGate(finishedRuns[fr].id);
    }

    // Find active runs with pending entries.
    // #815: `user_cancelled` entries are deliberately excluded here — they
    // represent an explicit operator stop and must never be resurrected by
    // the tick. Only `pending` entries are re-dispatchable.
    var activeRuns = agentdesk.db.query(
      "SELECT r.id " +
      "FROM auto_queue_runs r " +
      "JOIN auto_queue_entries e ON e.run_id = r.id " +
      "WHERE r.status = 'active' AND e.status = 'pending' " +
      "GROUP BY r.id " +
      "ORDER BY MIN(e.updated_at) ASC LIMIT 50",
      []
    );

    for (var ri = 0; ri < activeRuns.length; ri++) {
      var run = activeRuns[ri];
      var activation = activateRun(run.id, null);
      if (!activationWasDeferred(activation) && activationDispatchCount(activation) === 0) {
        rotateActiveRunSweepCursor(run.id);
      }
    }

    // Recovery path 2 (#179/#191/#214/#952): dispatched entries whose dispatch is stuck.
    // Threshold and stuck conditions are runtime-configurable so cron recovery can
    // be tuned without policy edits.
    var staleDispatchedGraceMinutes = configuredStaleDispatchedGraceMinutes();
    var staleDispatchedConditions = staleDispatchedRecoveryConditionsSql();
    var stuckDispatched = agentdesk.db.query(
      "SELECT e.id, e.agent_id, e.dispatch_id, e.kanban_card_id " +
      "FROM auto_queue_entries e " +
      "JOIN auto_queue_runs r ON e.run_id = r.id " +
      "WHERE e.status = 'dispatched' AND r.status = 'active' " +
      "AND e.dispatched_at IS NOT NULL " +
      "AND e.dispatched_at < datetime('now', '-" + staleDispatchedGraceMinutes + " minutes') " +
      "AND (" + staleDispatchedConditions + ") " +
      "ORDER BY e.dispatched_at ASC LIMIT 50",
      []
    );

    // #214: pendingIntents check REMOVED — it caused permanent recovery block when
    // intent drain failed (dispatch never created in DB but intent stayed in array
    // across ticks, skipping recovery forever). The 2-min grace period on
    // dispatched_at is sufficient to avoid false detection within the same tick.

    for (var j = 0; j < stuckDispatched.length; j++) {
      var stuck = stuckDispatched[j];
      var failure = agentdesk.autoQueue.recordDispatchFailure(
        stuck.id,
        configuredAutoQueueMaxEntryRetries(),
        "tick_recovery"
      );
      autoQueueLog("info", "onTick1min: recovered stuck dispatched entry " + stuck.id + " (dispatch " + (stuck.dispatch_id || "NULL") + " is orphan/cancelled/failed/phantom) retry " + failure.retryCount + "/" + failure.retryLimit + " -> " + failure.to, {
        entry_id: stuck.id,
        card_id: stuck.kanban_card_id,
        dispatch_id: stuck.dispatch_id
      });
      notifyAutoQueueEntryFailure(stuck, failure);
    }
  }
};

function terminalStatesFromConfig(cfg) {
  var terminalStates = [];
  if (cfg && cfg.states) {
    for (var i = 0; i < cfg.states.length; i++) {
      var state = cfg.states[i];
      if (state && state.terminal && state.id) {
        terminalStates.push(state.id);
      }
    }
  }
  if (terminalStates.length === 0) {
    terminalStates.push("done");
  }
  return terminalStates;
}

function activationDispatchCount(result) {
  if (!result) return null;
  if (typeof result.count === "number") return result.count;
  if (typeof result.dispatched_count === "number") return result.dispatched_count;
  if (typeof result.dispatchedCount === "number") return result.dispatchedCount;
  if (typeof result.activated_count === "number") return result.activated_count;
  if (typeof result.activatedCount === "number") return result.activatedCount;
  return null;
}

function activationWasDeferred(result) {
  return result && result.deferred === true;
}

function rotateActiveRunSweepCursor(runId) {
  if (!runId) return;
  try {
    agentdesk.db.execute(
      "UPDATE auto_queue_entries SET updated_at = datetime('now') WHERE run_id = ? AND status = 'pending'",
      [runId]
    );
  } catch (e) {
    autoQueueLog("warn", "failed to rotate active run sweep cursor for " + runId + ": " + e, {
      run_id: runId
    });
  }
}

function _isDispatchableState(state, cfg) {
  if (!cfg || !cfg.transitions) return false;
  var hasGatedOut = false;
  var hasGatedIn = false;
  for (var i = 0; i < cfg.transitions.length; i++) {
    var t = cfg.transitions[i];
    if (t.from === state && t.type === "gated") hasGatedOut = true;
    if (t.to === state && t.type === "gated") hasGatedIn = true;
  }
  return hasGatedOut && !hasGatedIn;
}

function _dispatchableTargets(cfg) {
  if (!cfg || !cfg.states) return [];
  var targets = [];

  // #255: requested is the canonical preflight anchor when present.
  if (agentdesk.pipeline.hasState("requested", cfg)) {
    targets.push("requested");
  }

  for (var i = 0; i < cfg.states.length; i++) {
    var s = cfg.states[i];
    if (s.terminal) continue;
    if (!_isDispatchableState(s.id, cfg)) continue;
    if (targets.indexOf(s.id) === -1) targets.push(s.id);
  }
  return targets;
}

function _freePathToDispatchable(from, cfg) {
  var targets = _dispatchableTargets(cfg);
  if (targets.length === 0) return null;
  if (targets.indexOf(from) >= 0) return [];
  if (!cfg || !cfg.transitions) return null;

  var queue = [from];
  var visited = {};
  var parent = {};
  visited[from] = true;

  while (queue.length > 0) {
    var cur = queue.shift();
    for (var i = 0; i < cfg.transitions.length; i++) {
      var t = cfg.transitions[i];
      if (t.from !== cur || t.type !== "free" || visited[t.to]) continue;
      parent[t.to] = cur;
      if (targets.indexOf(t.to) >= 0) {
        var path = [t.to];
        var p = cur;
        while (p && p !== from) {
          path.unshift(p);
          p = parent[p];
        }
        return path;
      }
      visited[t.to] = true;
      queue.push(t.to);
    }
  }

  return null;
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

function loadRunInfo(runId) {
  var rows = agentdesk.db.query(
    "SELECT status, repo, unified_thread_id, unified_thread_channel_id, COALESCE(thread_group_count, 1) as group_count " +
    "FROM auto_queue_runs WHERE id = ?",
    [runId]
  );
  return rows.length > 0 ? rows[0] : null;
}

function runHasBlockingPhaseGate(runId) {
  var rows = agentdesk.db.query(
    "SELECT COUNT(*) as cnt FROM auto_queue_phase_gates " +
    "WHERE run_id = ? AND status IN ('pending', 'failed')",
    [runId]
  );
  return rows.length > 0 && rows[0].cnt > 0;
}

// #747 round-2: Phase-gate race protection.
// Tick hooks now run on a separate `PolicyEngine` from `onCardTerminal`, so
// `onTick1min.finalizeRunWithoutPhaseGate` can see a run with no
// pending/dispatched entries AFTER Rust marks the last entry `done` but
// BEFORE the main engine's `onCardTerminal` has finished creating phase-gate
// dispatches. Mark a short grace window in the DB at the start of
// `continueRunAfterEntry` and respect it in finalization.
var PHASE_GATE_GRACE_WINDOW_MS = 30 * 1000; // 30s

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

function finalizeRunWithoutPhaseGate(runId) {
  if (!runId) return false;

  if (runHasBlockingPhaseGate(runId)) return false;
  if (remainingRunnableEntryCount(runId) > 0) return false;
  // #815 P1: `user_cancelled` entries are operator-held terminal state.
  // They are intentionally non-runnable, but they must still block the
  // tick-side backstop from auto-completing the run; otherwise the next
  // minute tick would strand a user-stopped run in `completed`.
  if (runHasUserCancelledEntry(runId)) {
    autoQueueLog("info", "Deferring finalize for run " + runId + " — user_cancelled entry still present", {
      run_id: runId
    });
    return false;
  }
  // Phase-gate race guard: the main engine's `onCardTerminal` may still be
  // in the middle of creating gate dispatches. Respect the grace window so
  // we never mark a run completed before phase gates get registered.
  if (runWithinPhaseGateGrace(runId)) {
    autoQueueLog("info", "Deferring finalize for run " + runId + " — phase-gate grace window active", {
      run_id: runId
    });
    return false;
  }

  var completed = false;
  try {
    var result = agentdesk.autoQueue.completeRun(
      runId,
      "finalize_without_phase_gate",
      { releaseSlots: true }
    );
    completed = !!(result && result.changed);
  } catch (e) {
    autoQueueLog("warn", "Failed to finalize run " + runId + ": " + e, {
      run_id: runId
    });
    return false;
  }
  if (!completed) return false;

  autoQueueLog("info", "Finalized non-phase-gate run " + runId + " and released its slots", {
    run_id: runId
  });
  return true;
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

function completeRunAndNotify(runId) {
  if (!runId) return;
  try {
    var completed = agentdesk.autoQueue.completeRun(
      runId,
      "phase_gate_complete",
      { releaseSlots: true }
    );
    if (completed && completed.changed) return;
    autoQueueLog("warn", "Phase-gate completion did not mark run " + runId + " completed; falling back to resume", {
      run_id: runId
    });
  } catch (e) {
    autoQueueLog("warn", "Failed to complete final phase-gate run " + runId + ": " + e, {
      run_id: runId
    });
  }
  try {
    agentdesk.autoQueue.resumeRun(runId, "phase_gate_complete_resume_fallback");
  } catch (e) {
    autoQueueLog("warn", "Failed to resume final phase-gate run " + runId + ": " + e, {
      run_id: runId
    });
  }
  activateRun(runId, null);
}

function remainingRunnableEntryCount(runId, phase) {
  var sql =
    "SELECT COUNT(*) as cnt FROM auto_queue_entries " +
    "WHERE run_id = ? AND status IN ('pending', 'dispatched')";
  var params = [runId];
  if (phase !== null && phase !== undefined) {
    sql += " AND COALESCE(batch_phase, 0) = ?";
    params.push(phase);
  }
  var rows = agentdesk.db.query(sql, params);
  return (rows.length > 0) ? rows[0].cnt : 0;
}

function runHasUserCancelledEntry(runId) {
  var rows = agentdesk.db.query(
    "SELECT COUNT(*) as cnt FROM auto_queue_entries " +
    "WHERE run_id = ? AND status = 'user_cancelled'",
    [runId]
  );
  return (rows.length > 0) && rows[0].cnt > 0;
}

function continueRunAfterEntry(runId, agentId, doneGroup, donePhase, anchorCardId) {
  if (!runId || !agentId) return;

  // #747 round-2: Open the phase-gate grace window BEFORE we do any work so
  // an overlapping `onTick1min.finalizeRunWithoutPhaseGate` on the tick
  // engine cannot steal-complete the run. Cleared on all non-phase-gate
  // exits below; `_createPhaseGateDispatches` leaves the gate in place (the
  // pending phase-gate row itself now guards the run via
  // `runHasBlockingPhaseGate`).
  beginPhaseGateGraceWindow(runId);

  var remainingCount = remainingRunnableEntryCount(runId, null);

  var effectiveDonePhase = (donePhase !== null && donePhase !== undefined) ? donePhase : -1;
  if (effectiveDonePhase >= 0) {
    var currentPhaseDone = remainingRunnableEntryCount(runId, donePhase) === 0;
    if (currentPhaseDone) {
      var nextPhaseRows = agentdesk.db.query(
        "SELECT MIN(batch_phase) as next_phase FROM auto_queue_entries " +
        "WHERE run_id = ? AND status IN ('pending', 'dispatched') AND COALESCE(batch_phase, 0) > ?",
        [runId, donePhase]
      );
      var nextPhase = (nextPhaseRows.length > 0) ? nextPhaseRows[0].next_phase : null;
      if (_phaseGateRequired(runId, donePhase)) {
        var finalPhase = remainingCount === 0;
        _createPhaseGateDispatches(runId, donePhase, nextPhase, finalPhase, anchorCardId);
        // Intentionally leave grace window in place: the phase-gate row
        // created above now guards the run; grace naturally expires.
        return;
      }
      if (nextPhase !== null && nextPhase !== undefined) {
        var nextPhaseCountRows = agentdesk.db.query(
          "SELECT COUNT(*) as cnt FROM auto_queue_entries " +
          "WHERE run_id = ? AND status IN ('pending', 'dispatched') AND COALESCE(batch_phase, 0) = ?",
          [runId, nextPhase]
        );
        var nextPhaseCount = (nextPhaseCountRows.length > 0) ? nextPhaseCountRows[0].cnt : 0;
        agentdesk.log.info("[auto-queue] Phase " + donePhase + " 완료, Phase " + nextPhase + " 시작 (" + nextPhaseCount + " entries)");
        clearPhaseGateGraceWindow(runId);
        activateRun(runId, null);
        return;
      }
    }
  }

  if (remainingCount === 0) {
    // No more work AND no phase gate required → grace window no longer
    // needed. Clear it so finalization can proceed immediately.
    clearPhaseGateGraceWindow(runId);
    if (!finalizeRunWithoutPhaseGate(runId)) {
      completeRunAndNotify(runId);
    }
    return;
  }

  var groupRemaining = agentdesk.db.query(
    "SELECT COUNT(*) as cnt FROM auto_queue_entries WHERE run_id = ? AND COALESCE(thread_group, 0) = ? AND status IN ('pending', 'dispatched')",
    [runId, doneGroup || 0]
  );
  var groupDone = groupRemaining.length > 0 && groupRemaining[0].cnt === 0;

  var tCfg = agentdesk.pipeline.getConfig();
  var tKickoff = agentdesk.pipeline.kickoffState(tCfg);
  var tInProgress = agentdesk.pipeline.nextGatedTarget(tKickoff, tCfg);
  var tReview = agentdesk.pipeline.nextGatedTarget(tInProgress, tCfg);
  var activeStates = [tKickoff, tInProgress, tReview].filter(function(s) { return s; });
  var agentBusy = false;
  if (activeStates.length > 0) {
    var placeholders = activeStates.map(function() { return "?"; }).join(",");
    var active = agentdesk.db.query(
      "SELECT COUNT(*) as cnt FROM kanban_cards WHERE assigned_agent_id = ? AND status IN (" + placeholders + ")",
      [agentId].concat(activeStates)
    );
    agentBusy = active.length > 0 && active[0].cnt > 0;
  }

  // Grace window no longer needed past this point — either we keep
  // dispatching in the same group (no phase transition happened) or we
  // move to the next group. Either way, there is no phase-gate dispatch
  // window to protect.
  clearPhaseGateGraceWindow(runId);

  if (!groupDone) {
    if (!agentBusy) {
      activateRun(runId, doneGroup || 0, agentId);
    } else {
      agentdesk.log.info("[auto-queue] Agent " + agentId + " still busy, deferring group " + (doneGroup || 0) + " next dispatch");
    }
    return;
  }

  activateRun(runId, null, agentId);
}

function resumeRunAndActivate(runId, nextPhase) {
  try {
    agentdesk.autoQueue.resumeRun(runId, "phase_gate_resume");
  } catch (e) {
    autoQueueLog("warn", "Failed to resume run " + runId + ": " + e, {
      run_id: runId,
      batch_phase: nextPhase !== undefined ? nextPhase : null
    });
  }
  if (nextPhase !== null && nextPhase !== undefined) {
    autoQueueLog("info", "Resuming run " + runId + " for phase " + nextPhase, {
      run_id: runId,
      batch_phase: nextPhase
    });
  }
  activateRun(runId, null);
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

function activateRun(runId, threadGroup, agentId) {
  if (!runId) return null;
  try {
    if (agentId !== null && agentId !== undefined) {
      var body = {
        run_id: runId,
        active_only: true,
        agent_id: agentId
      };
      if (threadGroup !== null && threadGroup !== undefined) {
        body.thread_group = threadGroup;
      }
      return agentdesk.autoQueue.activate(body);
    }
    return agentdesk.autoQueue.activate(runId, threadGroup);
  } catch (e) {
    autoQueueLog("warn", "activate bridge failed for run " + runId + ": " + e, {
      run_id: runId,
      thread_group: threadGroup,
      agent_id: agentId || null
    });
    return null;
  }
}

// ── Consultation dispatch helper (#256) ─────────────────────────
function _createConsultationDispatch(entry, agentId, preflightMeta) {
  // Find the counterpart agent for consultation
  var agent = agentdesk.db.query(
    "SELECT cli_provider FROM agents WHERE id = ?",
    [agentId]
  );
  var provider = (agent.length > 0) ? agent[0].cli_provider : "claude";
  var counterProvider = (provider === "claude") ? "codex" : "claude";
  var counterAgent = agentdesk.db.query(
    "SELECT id FROM agents WHERE cli_provider = ? LIMIT 1",
    [counterProvider]
  );
  var consultAgentId = (counterAgent.length > 0) ? counterAgent[0].id : agentId;

  try {
    var dispatchId = agentdesk.dispatch.create(
      entry.kanban_card_id,
      consultAgentId,
      "consultation",
      "[Consultation] " + entry.title
    );
    if (dispatchId) {
      agentdesk.autoQueue.recordConsultationDispatch(
        entry.id,
        entry.kanban_card_id,
        dispatchId,
        "consultation_dispatch_created",
        preflightMeta
      );
      autoQueueLog("info", "Created consultation dispatch " + dispatchId + " for " + entry.kanban_card_id, {
        entry_id: entry.id,
        card_id: entry.kanban_card_id,
        dispatch_id: dispatchId
      });
    }
  } catch (e) {
    autoQueueLog("warn", "Consultation dispatch failed for " + entry.kanban_card_id + ": " + e, {
      entry_id: entry.id,
      card_id: entry.kanban_card_id
    });
  }
}

if (typeof agentdesk !== "undefined" && agentdesk && typeof agentdesk.registerPolicy === "function") {
  agentdesk.registerPolicy(autoQueue);
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = {
    policy: autoQueue,
    __test: {
      inferPhaseGatePassVerdict: _inferPhaseGatePassVerdict,
      dispatchableTargets: _dispatchableTargets,
      freePathToDispatchable: _freePathToDispatchable
    }
  };
}
