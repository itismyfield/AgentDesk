/** @module policies/lib/auto-queue-config
 *
 * #1078: Extracted from auto-queue.js as part of the policy modularization pass.
 *
 * Runtime-config readers for the auto-queue policy. Every reader has a
 * safe fallback so policies keep running even if the corresponding config
 * key is unset or malformed. Kept here (not inline) so the config surface
 * stays discoverable and the SQL condition builder — which is the only
 * non-trivial derived value — can be unit-tested independently.
 */

function configuredAutoQueueMaxEntryRetries() {
  var configured = parseInt(agentdesk.config.get("maxEntryRetries"), 10);
  if (!configured || configured < 1) return 3;
  return configured;
}

function configuredStaleDispatchedGraceMinutes() {
  var configured = parseInt(agentdesk.config.get("staleDispatchedGraceMin"), 10);
  if (!configured || configured < 1) return 2;
  return configured;
}

function configuredStaleDispatchedTerminalStatuses() {
  var configured = agentdesk.config.get("staleDispatchedTerminalStatuses");
  var raw = typeof configured === "string" ? configured : "cancelled,failed";
  var statuses = raw
    .split(",")
    .map(function(status) { return String(status || "").trim().toLowerCase(); })
    .filter(function(status) { return /^[a-z_]+$/.test(status); });
  return statuses.length > 0 ? statuses : ["cancelled", "failed"];
}

function configuredStaleDispatchedRecoverNullDispatch() {
  var configured = agentdesk.config.get("staleDispatchedRecoverNullDispatch");
  if (configured === null || configured === undefined) return true;
  return configured === true || configured === "true";
}

function configuredStaleDispatchedRecoverMissingDispatch() {
  var configured = agentdesk.config.get("staleDispatchedRecoverMissingDispatch");
  if (configured === null || configured === undefined) return true;
  return configured === true || configured === "true";
}

function staleDispatchedRecoveryConditionsSql() {
  var conditions = [];
  if (configuredStaleDispatchedRecoverNullDispatch()) {
    conditions.push("e.dispatch_id IS NULL");
  }

  var terminalStatuses = configuredStaleDispatchedTerminalStatuses();
  if (terminalStatuses.length > 0) {
    conditions.push(
      "EXISTS (" +
        "SELECT 1 FROM task_dispatches td " +
        "WHERE td.id = e.dispatch_id " +
        "AND td.status IN (" + terminalStatuses.map(function(status) {
          return "'" + status + "'";
        }).join(", ") + ")" +
      ")"
    );
  }

  if (configuredStaleDispatchedRecoverMissingDispatch()) {
    conditions.push(
      "(" +
        "e.dispatch_id IS NOT NULL AND NOT EXISTS (" +
          "SELECT 1 FROM task_dispatches td WHERE td.id = e.dispatch_id" +
        ")" +
      ")"
    );
  }

  if (conditions.length === 0) return "0";
  return conditions.join(" OR ");
}

module.exports = {
  maxEntryRetries: configuredAutoQueueMaxEntryRetries,
  staleDispatchedGraceMinutes: configuredStaleDispatchedGraceMinutes,
  staleDispatchedTerminalStatuses: configuredStaleDispatchedTerminalStatuses,
  staleDispatchedRecoverNullDispatch: configuredStaleDispatchedRecoverNullDispatch,
  staleDispatchedRecoverMissingDispatch: configuredStaleDispatchedRecoverMissingDispatch,
  staleDispatchedRecoveryConditionsSql: staleDispatchedRecoveryConditionsSql
};
