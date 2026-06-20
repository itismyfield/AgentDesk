/** @module policies/lib/kanban-scope-gate
 *
 * #3594 (T3): depth-gated flow resolution + next-dispatch creation helpers.
 *
 * T2 (#3605) recorded a `scope_depth` ∈ {direct, plan_only, full} on the card
 * (or fell back to "full" when the assessment was missing/unparsable) but left
 * it INERT. T3 activates that depth in the scope-assessment completion handler
 * (kanban-rules.js onDispatchCompleted) to gate the lifecycle:
 *
 *   - direct    → skip plan + plan-review; create the implementation dispatch
 *                 directly (reusing the consultation-resume path).
 *   - plan_only → create a "plan" dispatch; its completion creates impl.
 *   - full      → create a "plan" dispatch; its completion publishes a
 *                 "plan-review"; plan-review pass → impl, rework → re-plan.
 *   - unknown/null → treated as "full" (most cautious), mirroring the
 *                 _recordScopeAssessment fallback in kanban-scope-assessment.js.
 *
 * This module owns ONLY the minimal flow-resolution + dispatch-creation glue.
 * It deliberately does NOT introduce a PhaseRun/ReviewRun kernel or a gate DSL
 * (G3) — plan/plan-review are ordinary `task_dispatches` rows whose follow-up is
 * decided by the kanban-rules onDispatchCompleted arms.
 *
 * The dispatch-creation helpers encapsulate the consultation-resume pattern
 * (#256: `_findAutoQueueEntriesByDispatch` + `agentdesk.dispatch.create` +
 * `agentdesk.autoQueue.updateEntryStatus(entry, "dispatched", ...)`) so the
 * scope-gate and the missed-hook fallback (timeouts/reconciliation.js) create
 * identical follow-up dispatches. On any creation failure they leave the linked
 * auto-queue entry untouched (pending) and warn, so the queue can retry rather
 * than stranding the entry in `dispatched`.
 *
 * Depends on the global `agentdesk` surface plus the card-metadata lookup lib.
 */

var _cardMetadata = require("./kanban-card-metadata");
var _findAutoQueueEntriesByDispatch = _cardMetadata._findAutoQueueEntriesByDispatch;

// #3594 (T3, codex Finding 2): the FIRST stage (scope-assessment) must CLAIM the
// card's pending auto-queue entry onto its own dispatch — exactly as consultation
// does at creation (record_consultation_dispatch_on_pg) — otherwise the entry is
// never linked to the staged dispatch chain and scope-completion's resume cannot
// find it, so the activate fallback creates a plain `implementation` dispatch and
// the depth gate (plan/plan-review) is bypassed entirely.
//
// Returns the claimed entry row {id, agent_id} or null when there is no pending
// entry to claim (manual/API card, or the entry has not been generated yet — in
// which case the Rust activate `scope_assessment_pending` gate holds the entry
// pending until scope completes, and the resume path's by-card fallback below
// re-discovers it). On any failure it warns and returns null (entry untouched).
//
// Unlike recordConsultationDispatch (which also stamps consultation_status on the
// card metadata) this uses the generic entry-status claim, so it links the entry
// via auto_queue_entry_dispatch_history + marks it `dispatched` WITHOUT polluting
// the card's scope metadata.
function _findPendingEntryForCard(cardId) {
  var rows = agentdesk.db.query(
    "SELECT e.id, e.agent_id FROM auto_queue_entries e " +
    "JOIN auto_queue_runs r ON r.id = e.run_id " +
    "WHERE e.kanban_card_id = ? AND e.status = 'pending' " +
    "  AND r.status IN ('active', 'paused') " +
    "ORDER BY e.priority_rank ASC, e.created_at ASC LIMIT 1",
    [cardId]
  );
  return rows.length > 0 ? rows[0] : null;
}

function _claimPendingEntryForDispatch(cardId, dispatchId, reason) {
  var entry = _findPendingEntryForCard(cardId);
  if (!entry) {
    return null;
  }
  try {
    agentdesk.autoQueue.updateEntryStatus(
      entry.id,
      "dispatched",
      reason,
      { dispatchId: dispatchId }
    );
    agentdesk.log.info(
      "[scope-gate] claimed pending entry " + entry.id + " for " + cardId +
      " onto dispatch " + dispatchId + " (" + reason + ")"
    );
    return entry;
  } catch (e) {
    agentdesk.log.warn(
      "[scope-gate] failed to claim pending entry " + entry.id + " for " + cardId +
      " onto dispatch " + dispatchId + ": " + e
    );
    return null;
  }
}

// Canonical depth → flow mapping. Anything not exactly "direct" or "plan_only"
// (including null/undefined/typos) resolves to the most cautious full flow:
// plan + plan-review. This mirrors kanban-scope-assessment._recordScopeAssessment,
// which already normalizes unknown depths to "full" on the way in — the double
// guard means a corrupted/absent metadata read still fails safe here.
function _resolveScopeFlow(depth) {
  if (depth === "direct") {
    return { needsPlan: false, needsPlanReview: false };
  }
  if (depth === "plan_only") {
    return { needsPlan: true, needsPlanReview: false };
  }
  // "full" and every unknown/missing value → most cautious.
  return { needsPlan: true, needsPlanReview: true };
}

// Resume the linked auto-queue entry (if any) with a freshly-created dispatch of
// `dispatchType`, reusing the consultation-resume semantics (#256, Finding 15):
// dispatch the FIRST linked entry and mark every other linked entry `skipped` so
// the queue does not loop duplicates. Returns the created dispatch id, or null
// on failure (entries left pending so a later sweep can retry).
//
// `extraContext` is merged into the auto_queue dispatch context (used to carry
// scope_depth on the "plan" dispatch so the plan-completion arm does not have to
// re-read card metadata).
function _resumeEntriesWithDispatch(cardId, dispatch, card, dispatchType, title, resumeReason, extraContext) {
  var aqEntries = _findAutoQueueEntriesByDispatch(dispatch.id, false);
  if (aqEntries.length === 0) {
    // The parent dispatch has no linked `dispatched` entry. With the Finding-2 fix
    // the scope-assessment claims the pending entry up-front, so the staged chain
    // (scope→plan→plan-review→impl) keeps the SAME entry linked at each hop and
    // this branch is normally unreachable on the auto-queue path. But the entry
    // may have been generated AFTER scope-assessment fired (held pending by the
    // Rust activate `scope_assessment_pending` gate). Recover it: find the card's
    // pending entry and claim it onto this staged dispatch so the chain proceeds
    // instead of yielding to the activate fallback (which would create a plain
    // `implementation`, bypassing the depth gate).
    var recovered = _findPendingEntryForCard(cardId);
    if (!recovered) {
      // Genuinely no auto-queue entry (manual/API card). There is no entry to
      // claim or resume, so this staged-resume helper creates nothing and returns
      // null — exactly as before T3. Manual/API cards drive their own dispatch
      // lifecycle outside the auto-queue resume path, so forward progress is not
      // this helper's responsibility here.
      agentdesk.log.info(
        "[scope-gate] " + cardId + " has no auto-queue entry for " +
        dispatchType + " — no staged dispatch created (manual/API card)"
      );
      return null;
    }
    aqEntries = [recovered];
    agentdesk.log.info(
      "[scope-gate] " + cardId + " recovered late pending entry " + recovered.id +
      " for " + dispatchType + " (claiming onto staged dispatch)"
    );
  }
  var primary = aqEntries[0];
  var context = {
    auto_queue: true,
    entry_id: primary.id,
    parent_dispatch_id: dispatch.id
  };
  if (extraContext && typeof extraContext === "object") {
    for (var k in extraContext) {
      if (Object.prototype.hasOwnProperty.call(extraContext, k)) {
        context[k] = extraContext[k];
      }
    }
  }
  var nextDispatchId = null;
  try {
    nextDispatchId = agentdesk.dispatch.create(
      cardId,
      primary.agent_id,
      dispatchType,
      title || (card && card.title) || dispatchType,
      context
    );
  } catch (e) {
    agentdesk.log.warn(
      "[scope-gate] " + cardId + " " + dispatchType +
      " dispatch creation threw — entry left pending: " + e
    );
    return null;
  }
  if (!nextDispatchId) {
    agentdesk.log.warn(
      "[scope-gate] " + cardId + " " + dispatchType +
      " dispatch creation returned no id — entry left pending"
    );
    return null;
  }
  try {
    agentdesk.autoQueue.updateEntryStatus(
      primary.id,
      "dispatched",
      resumeReason,
      { dispatchId: nextDispatchId }
    );
  } catch (e) {
    agentdesk.log.warn(
      "[scope-gate] " + cardId + " " + dispatchType + " dispatch " + nextDispatchId +
      " created but entry status update failed: " + e
    );
  }
  if (aqEntries.length > 1) {
    for (var aqi = 1; aqi < aqEntries.length; aqi++) {
      try {
        agentdesk.autoQueue.updateEntryStatus(
          aqEntries[aqi].id,
          "skipped",
          resumeReason + "_duplicate",
          { primaryEntryId: primary.id }
        );
      } catch (skipErr) {
        agentdesk.log.warn(
          "[scope-gate] could not mark duplicate aq entry " +
          aqEntries[aqi].id + " as skipped: " + skipErr
        );
      }
    }
    agentdesk.log.warn(
      "[scope-gate] " + dispatchType + " for " + cardId + " linked to " +
      aqEntries.length + " auto_queue entries — dispatched primary " + primary.id +
      " and skipped the remaining " + (aqEntries.length - 1)
    );
  }
  return nextDispatchId;
}

// #3594 (T3, codex Finding 3): build the downstream dispatch context, carrying
// the resolved depth and (when present) the parent PLAN text. The plan body is
// propagated as `parent_plan` so the prompt builder (render_dispatch_context_section)
// renders it into the plan-review / impl prompt — that is what lets a plan-review
// actually see the plan it must review, and a full→impl see the approved plan.
// Only a non-empty string is forwarded so plan_only/direct (no review) and
// manual cards stay clean.
function _stageContext(depth, planText) {
  var ctx = { scope_depth: depth };
  if (typeof planText === "string" && planText.trim() !== "") {
    ctx.parent_plan = planText;
  }
  return ctx;
}

// direct (or any terminal stage that resolves to "implement now"): create the
// implementation dispatch via the consultation-resume path. `planText` (optional)
// is the approved plan body forwarded so the impl agent sees it (full → impl).
function _createImplDispatch(cardId, dispatch, card, resumeReason, planText) {
  var extra = (typeof planText === "string" && planText.trim() !== "")
    ? { parent_plan: planText }
    : null;
  return _resumeEntriesWithDispatch(
    cardId,
    dispatch,
    card,
    "implementation",
    (card && card.title) || "Implementation",
    resumeReason || "scope_gate_impl",
    extra
  );
}

// plan_only / full: create the "plan" work-dispatch. The resolved depth rides in
// the dispatch context so the plan-completion arm can branch (plan_only→impl,
// full→plan-review) WITHOUT re-reading card metadata (which a concurrent path
// could have mutated). The plan dispatch itself has no parent plan (it IS the
// plan stage), so only `scope_depth` is carried.
function _createPlanDispatch(cardId, dispatch, card, depth) {
  return _resumeEntriesWithDispatch(
    cardId,
    dispatch,
    card,
    "plan",
    "[Plan] " + ((card && card.title) || cardId),
    "scope_gate_plan",
    { scope_depth: depth }
  );
}

// full: after a plan completes, publish the "plan-review". This is NOT routed
// through the counter-model review kernel (Option P) — it is an ordinary
// dispatch whose verdict (pass|rework) is read directly by the kanban-rules
// plan-review arm. Reuses the resume path so the bound entry stays attached to
// the in-flight review stage rather than being re-created by activate.
// `planText` is the completed plan dispatch's `result.plan`, forwarded as
// `parent_plan` so the reviewer actually sees the plan body (codex Finding 3).
function _createPlanReviewDispatch(cardId, dispatch, card, depth, planText) {
  return _resumeEntriesWithDispatch(
    cardId,
    dispatch,
    card,
    "plan-review",
    "[Plan Review] " + ((card && card.title) || cardId),
    "scope_gate_plan_review",
    _stageContext(depth, planText)
  );
}

module.exports = {
  _resolveScopeFlow: _resolveScopeFlow,
  _findPendingEntryForCard: _findPendingEntryForCard,
  _claimPendingEntryForDispatch: _claimPendingEntryForDispatch,
  _resumeEntriesWithDispatch: _resumeEntriesWithDispatch,
  _createImplDispatch: _createImplDispatch,
  _createPlanDispatch: _createPlanDispatch,
  _createPlanReviewDispatch: _createPlanReviewDispatch
};
