/** @module policies/lib/auto-queue-error-recovery
 *
 * #1078: Extracted from auto-queue.js as part of the policy modularization pass.
 *
 * Failure/escalation helpers driven from the tick recovery path:
 *   - `notifyAutoQueueEntryFailure(stuck, failure)` raises a human-facing
 *     Discord alert when an entry transitions to `failed` because of stuck
 *     dispatches (orphan/cancelled/failed/phantom).
 *   - `_createConsultationDispatch(entry, agentId, preflightMeta)` creates
 *     a counterpart-provider consultation dispatch when preflight asks for
 *     one (used by #256).
 *
 * Depends on `policies/lib/auto-queue-log` and `policies/lib/auto-queue-phase-gate`
 * (for the shared `loadPhaseGateCardLabel` card-label formatter), plus the
 * `notifyHumanAlert` global injected by the policy harness.
 */

var _autoQueueLogLib = require("./auto-queue-log");
var _autoQueuePhaseGateLib = require("./auto-queue-phase-gate");

var autoQueueLog = _autoQueueLogLib.autoQueueLog;
var loadPhaseGateCardLabel = _autoQueuePhaseGateLib.loadPhaseGateCardLabel;

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

module.exports = {
  notifyAutoQueueEntryFailure: notifyAutoQueueEntryFailure,
  createConsultationDispatch: _createConsultationDispatch
};
