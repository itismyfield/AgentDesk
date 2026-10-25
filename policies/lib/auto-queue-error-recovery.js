/** @module policies/lib/auto-queue-error-recovery
 *
 * #1078: Extracted from auto-queue.js as part of the policy modularization pass.
 *
 * Failure/escalation helpers driven from the tick recovery path:
 *   - `notifyAutoQueueEntryFailure(stuck, failure)` raises a human-alert
 *     WARN log line when an entry transitions to `failed` because of stuck
 *     dispatches (orphan/cancelled/failed/phantom).
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

module.exports = {
  notifyAutoQueueEntryFailure: notifyAutoQueueEntryFailure
};
