/** @module policies/lib/kanban-card-metadata
 *
 * #1078: Extracted from kanban-rules.js as part of the policy modularization pass.
 *
 * Provides kanban_cards.metadata read/write helpers and the
 * auto_queue_entries lookup used by the OnDispatchCompleted hook:
 *   - _loadCardMetadata / _writeCardMetadata / _mergeCardMetadata / _metadataParam
 *   - _findAutoQueueEntriesByDispatch
 *
 * The helpers intentionally depend on the global `agentdesk.db` surface — the
 * test harness injects mocks through the same global.
 */

function _loadCardMetadata(cardId) {
  var rows = agentdesk.db.query(
    "SELECT metadata FROM kanban_cards WHERE id = ?",
    [cardId]
  );
  if (rows.length === 0 || !rows[0].metadata) return {};
  try {
    var parsed = JSON.parse(rows[0].metadata);
    return parsed && typeof parsed === "object" ? parsed : {};
  } catch (e) {
    return {};
  }
}

function _mergeCardMetadata(cardId, patch) {
  var meta = _loadCardMetadata(cardId);
  for (var key in patch) {
    if (Object.prototype.hasOwnProperty.call(patch, key)) {
      meta[key] = patch[key];
    }
  }
  _writeCardMetadata(cardId, meta);
  return meta;
}

function _metadataParam(metadata) {
  return metadata && typeof metadata === "object" ? metadata : {};
}

function _writeCardMetadata(cardId, metadata) {
  agentdesk.db.execute(
    "UPDATE kanban_cards SET metadata = ? WHERE id = ?",
    [_metadataParam(metadata), cardId]
  );
}

function _findAutoQueueEntriesByDispatch(dispatchId, liveOnly) {
  var statusClause = liveOnly
    ? "e.status IN ('pending', 'dispatched')"
    : "e.status = 'dispatched'";
  return agentdesk.db.query(
    "SELECT DISTINCT e.id, e.agent_id FROM auto_queue_entries e " +
    "LEFT JOIN auto_queue_entry_dispatch_history h " +
    "  ON h.entry_id = e.id AND h.dispatch_id = ? " +
    "WHERE " + statusClause + " " +
    "  AND (e.dispatch_id = ? OR h.dispatch_id IS NOT NULL)",
    [dispatchId, dispatchId]
  );
}

module.exports = {
  _loadCardMetadata: _loadCardMetadata,
  _mergeCardMetadata: _mergeCardMetadata,
  _metadataParam: _metadataParam,
  _writeCardMetadata: _writeCardMetadata,
  _findAutoQueueEntriesByDispatch: _findAutoQueueEntriesByDispatch
};
