/** @module policies/lib/kanban-scope-assessment
 *
 * #3605 (T2): scope-assessment side-path result recording. Extracted from
 * kanban-rules.js so the missed-hook fallback in timeouts/reconciliation.js can
 * call the SAME recorder instead of dropping the result. Both the live
 * onDispatchCompleted hook (kanban-rules.js) and the DB-fallback replay
 * (reconciliation.js) must record scope_depth + fall back to "full" identically;
 * keeping the logic in one module is the only way to guarantee parity.
 *
 * The recorder is inert: it writes metadata only and never advances the card.
 * It depends on the global `agentdesk` surface via kanban-card-metadata.
 */

var _cardMetadata = require("./kanban-card-metadata");
var _mergeCardMetadata = _cardMetadata._mergeCardMetadata;

// Canonical scope_depth whitelist. Any agent output outside this set
// (missing / unparsable / timeout / typo) falls back to "full" (most cautious).
var SCOPE_DEPTH_VALUES = { full: true, plan_only: true, direct: true };

function _normalizeScopeDepth(raw) {
  if (typeof raw !== "string") return null;
  var v = raw.trim().toLowerCase().replace(/-/g, "_");
  return SCOPE_DEPTH_VALUES[v] ? v : null;
}

// Record scope-assessment result on the card metadata. Fail-safe normalization:
// any scope_depth outside {full,plan_only,direct} → "full" (most cautious).
// Missing reason/risk become diagnostic strings. No flow change (inert) — depth
// is consumed by the T3 phase. `dispatch` only needs a `.result` field.
function _recordScopeAssessment(cardId, dispatch) {
  var parsed = {};
  try { parsed = JSON.parse(dispatch.result || "{}"); } catch (e) { parsed = {}; }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) parsed = {};

  var depth = _normalizeScopeDepth(parsed.scope_depth);
  var fellBack = depth === null;
  if (fellBack) depth = "full";

  var reason = (typeof parsed.scope_reason === "string" && parsed.scope_reason.trim() !== "")
    ? parsed.scope_reason
    : (fellBack ? "unspecified (fallback to full)" : "unspecified");
  var risk = (typeof parsed.scope_risk === "string" && parsed.scope_risk.trim() !== "")
    ? parsed.scope_risk
    : (fellBack ? "unspecified (fallback to full)" : "unspecified");

  _mergeCardMetadata(cardId, {
    scope_depth: depth,
    scope_reason: reason,
    scope_risk: risk,
    scope_assessment_status: "completed",
    scope_assessment_result: parsed
  });
  agentdesk.log.info(
    "[scope] Card " + cardId + " scope-assessment completed: depth=" + depth +
    (fellBack ? " (fallback)" : "")
  );
}

module.exports = {
  SCOPE_DEPTH_VALUES: SCOPE_DEPTH_VALUES,
  _normalizeScopeDepth: _normalizeScopeDepth,
  _recordScopeAssessment: _recordScopeAssessment
};
