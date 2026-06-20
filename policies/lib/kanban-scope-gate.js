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
    // No linked auto-queue entry yet (manual/API card, or scope fired before the
    // generate transaction inserted entries). The Rust activate path will create
    // the proper dispatch once the impl-block clears (scope_assessment_status is
    // now "completed"). For plan/full this means the activate fallback creates an
    // IMPLEMENTATION dispatch — acceptable: the scope-gate's plan stage is a
    // best-effort enrichment on the auto-queue path, and the cautious default of
    // "still produce an implementation" preserves forward progress.
    agentdesk.log.info(
      "[scope-gate] " + cardId + " has no linked auto-queue entry for " +
      dispatchType + " — deferring to activate path"
    );
    return null;
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

// direct (or any terminal stage that resolves to "implement now"): create the
// implementation dispatch via the consultation-resume path.
function _createImplDispatch(cardId, dispatch, card, resumeReason) {
  return _resumeEntriesWithDispatch(
    cardId,
    dispatch,
    card,
    "implementation",
    (card && card.title) || "Implementation",
    resumeReason || "scope_gate_impl",
    null
  );
}

// plan_only / full: create the "plan" work-dispatch. The resolved depth rides in
// the dispatch context so the plan-completion arm can branch (plan_only→impl,
// full→plan-review) WITHOUT re-reading card metadata (which a concurrent path
// could have mutated).
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
function _createPlanReviewDispatch(cardId, dispatch, card, depth) {
  return _resumeEntriesWithDispatch(
    cardId,
    dispatch,
    card,
    "plan-review",
    "[Plan Review] " + ((card && card.title) || cardId),
    "scope_gate_plan_review",
    { scope_depth: depth }
  );
}

module.exports = {
  _resolveScopeFlow: _resolveScopeFlow,
  _resumeEntriesWithDispatch: _resumeEntriesWithDispatch,
  _createImplDispatch: _createImplDispatch,
  _createPlanDispatch: _createPlanDispatch,
  _createPlanReviewDispatch: _createPlanReviewDispatch
};
