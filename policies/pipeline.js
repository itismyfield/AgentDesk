var pipeline = {
  name: "pipeline",
  priority: 200,

  // Card transition — check if dispatchable cards should enter pipeline
  onCardTransition: function(payload) {
    // Pipeline-driven: check if target state is a dispatchable state
    var cfg = agentdesk.pipeline.resolveForCard(payload.card_id);
    if (!cfg || !cfg.states || !cfg.transitions) return;
    var isDispatchable = false;
    for (var si = 0; si < cfg.states.length; si++) {
      var s = cfg.states[si];
      if (s.id !== payload.to || s.terminal) continue;
      for (var ti = 0; ti < cfg.transitions.length; ti++) {
        if (cfg.transitions[ti].from === s.id && cfg.transitions[ti].type === "gated") {
          isDispatchable = true;
          break;
        }
      }
      break;
    }
    if (!isDispatchable) return;

    // Check if repo has pipeline stages triggered on this dispatchable state
    var cards = agentdesk.db.query(
      "SELECT repo_id FROM kanban_cards WHERE id = ?",
      [payload.card_id]
    );
    if (cards.length === 0) return;

    var stages = agentdesk.db.query(
      "SELECT id, stage_name, agent_override_id FROM pipeline_stages WHERE repo_id = ? AND trigger_after = ? ORDER BY stage_order ASC LIMIT 1",
      [cards[0].repo_id, payload.to]
    );
    if (stages.length > 0) {
      agentdesk.db.execute(
        "UPDATE kanban_cards SET pipeline_stage_id = ?, updated_at = datetime('now') WHERE id = ?",
        [stages[0].id, payload.card_id]
      );
      agentdesk.log.info("[pipeline] Card " + payload.card_id + " assigned to stage: " + stages[0].stage_name);
    }
  },

  // Dispatch completed — NO automatic stage advance.
  // Pipeline stage progression is driven ONLY by explicit lifecycle triggers:
  //   - trigger_after='ready'       → onCardTransition (above)
  //   - trigger_after='review_pass' → review-automation.js processVerdict
  // Implementation dispatch completion routes to review (via kanban-rules),
  // and the next stage dispatches only after review passes.
  // This prevents pipeline/review lifecycle conflicts (#110).
  //
  // Ordering (#1079): pipeline runs at priority 200; merge-automation at 201
  // (no onDispatchCompleted of its own). Kanban-rules (P10) fires first on
  // this hook to resolve the dispatch record; pipeline is intentionally a
  // no-op here so the hook sequence is deterministic.
  onDispatchCompleted: function(payload) {
    // No-op: stage advance removed. Review-automation handles post-review pipeline progression.
  }
};

agentdesk.registerPolicy(pipeline);
