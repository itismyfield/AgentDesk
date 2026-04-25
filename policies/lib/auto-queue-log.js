/** @module policies/lib/auto-queue-log
 *
 * #1078: Extracted from auto-queue.js as part of the policy modularization pass.
 *
 * Provides the structured log-context helpers used throughout auto-queue:
 *   - hydration of run_id/entry_id/card_id/dispatch_id from DB when only one key is known
 *   - consistent ordering + formatting when appending context to log messages
 *   - the public `autoQueueLog(level, message, context)` entry point
 *
 * The helpers intentionally depend on the global `agentdesk.db` / `agentdesk.log`
 * surfaces — the test harness injects mocks through the same globals.
 */

function _autoQueueHasValue(value) {
  return value !== null && value !== undefined && !(typeof value === "string" && value.trim() === "");
}

function _autoQueueLogContextKeys() {
  return ["run_id", "entry_id", "card_id", "dispatch_id", "thread_group", "batch_phase", "slot_index", "agent_id"];
}

function _mergeAutoQueueLogContext(target, source) {
  if (!source) return target;
  var keys = _autoQueueLogContextKeys();
  for (var i = 0; i < keys.length; i++) {
    var key = keys[i];
    if (!_autoQueueHasValue(target[key]) && _autoQueueHasValue(source[key])) {
      target[key] = source[key];
    }
  }
  return target;
}

function _loadAutoQueueEntryLogContext(entryId) {
  if (!_autoQueueHasValue(entryId)) return null;
  var rows = agentdesk.db.query(
    "SELECT run_id, id as entry_id, kanban_card_id as card_id, dispatch_id, agent_id, " +
    "COALESCE(thread_group, 0) as thread_group, COALESCE(batch_phase, 0) as batch_phase, slot_index " +
    "FROM auto_queue_entries WHERE id = ? LIMIT 1",
    [entryId]
  );
  return rows.length > 0 ? rows[0] : null;
}

function _loadAutoQueueDispatchLogContext(dispatchId) {
  if (!_autoQueueHasValue(dispatchId)) return null;
  var rows = agentdesk.db.query(
    "SELECT " +
    "COALESCE(e.run_id, " +
    "json_extract(COALESCE(td.context, '{}'), '$.run_id'), " +
    "json_extract(COALESCE(td.context, '{}'), '$.phase_gate.run_id')) as run_id, " +
    "COALESCE(e.id, json_extract(COALESCE(td.context, '{}'), '$.entry_id')) as entry_id, " +
    "COALESCE(e.kanban_card_id, td.kanban_card_id, json_extract(COALESCE(td.context, '{}'), '$.phase_gate.anchor_card_id')) as card_id, " +
    "td.id as dispatch_id, " +
    "COALESCE(e.thread_group, CAST(json_extract(COALESCE(td.context, '{}'), '$.thread_group') AS INTEGER)) as thread_group, " +
    "COALESCE(e.batch_phase, " +
    "CAST(json_extract(COALESCE(td.context, '{}'), '$.batch_phase') AS INTEGER), " +
    "CAST(json_extract(COALESCE(td.context, '{}'), '$.phase_gate.batch_phase') AS INTEGER)) as batch_phase, " +
    "COALESCE(e.slot_index, CAST(json_extract(COALESCE(td.context, '{}'), '$.slot_index') AS INTEGER)) as slot_index, " +
    "COALESCE(e.agent_id, json_extract(COALESCE(td.context, '{}'), '$.agent_id'), " +
    "json_extract(COALESCE(td.context, '{}'), '$.target_agent_id'), " +
    "json_extract(COALESCE(td.context, '{}'), '$.source_agent_id')) as agent_id " +
    "FROM task_dispatches td " +
    "LEFT JOIN auto_queue_entries e ON e.dispatch_id = td.id " +
    "WHERE td.id = ? LIMIT 1",
    [dispatchId]
  );
  return rows.length > 0 ? rows[0] : null;
}

function _normalizeAutoQueueLogContext(context) {
  var merged = {};
  var hydratedEntryId = null;
  _mergeAutoQueueLogContext(merged, context || {});
  if (_autoQueueHasValue(merged.entry_id)) {
    hydratedEntryId = merged.entry_id;
    _mergeAutoQueueLogContext(merged, _loadAutoQueueEntryLogContext(merged.entry_id));
  }
  if (_autoQueueHasValue(merged.dispatch_id)) {
    _mergeAutoQueueLogContext(merged, _loadAutoQueueDispatchLogContext(merged.dispatch_id));
  }
  if (_autoQueueHasValue(merged.entry_id) && merged.entry_id !== hydratedEntryId) {
    _mergeAutoQueueLogContext(merged, _loadAutoQueueEntryLogContext(merged.entry_id));
  }
  return merged;
}

function _formatAutoQueueLogContext(context) {
  var orderedKeys = _autoQueueLogContextKeys();
  var parts = [];
  for (var i = 0; i < orderedKeys.length; i++) {
    var key = orderedKeys[i];
    if (_autoQueueHasValue(context[key])) {
      parts.push(key + "=" + context[key]);
    }
  }
  return parts.length > 0 ? " | " + parts.join(" ") : "";
}

function autoQueueLog(level, message, context) {
  if (!agentdesk.log || typeof agentdesk.log[level] !== "function") return;
  var merged = _normalizeAutoQueueLogContext(context || {});
  agentdesk.log[level]("[auto-queue] " + message + _formatAutoQueueLogContext(merged));
}

module.exports = {
  autoQueueLog: autoQueueLog,
  hasValue: _autoQueueHasValue,
  logContextKeys: _autoQueueLogContextKeys,
  mergeLogContext: _mergeAutoQueueLogContext,
  loadEntryLogContext: _loadAutoQueueEntryLogContext,
  loadDispatchLogContext: _loadAutoQueueDispatchLogContext,
  normalizeLogContext: _normalizeAutoQueueLogContext,
  formatLogContext: _formatAutoQueueLogContext
};
