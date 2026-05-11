var pipeline = {
  name: "pipeline",
  priority: 200,

  // Card transition — check if dispatchable cards should enter pipeline
  onCardTransition: function(payload) {
    // Pipeline-driven: check if target state is a dispatchable state
    var cfg = agentdesk.pipeline.resolveForCard(payload.card_id);
    if (!cfg || !cfg.states || !cfg.transitions) return;
    // #2051 Finding 13 (P2): legacy code only looked at gated outbound
    // transitions, which misses dispatchable states whose outbound flow is
    // `free`. Custom pipelines that model "free-but-still-dispatchable"
    // states (e.g. a manual review stage that triggers a pipeline_stage but
    // does not require a gate to leave) used to get no
    // `pipeline_stage_id` assignment, stalling cards after review_pass.
    // Accept either:
    //   - an explicit `dispatchable: true` flag on the state, OR
    //   - any outbound gated transition (legacy heuristic), OR
    //   - the state has pipeline_stages with trigger_after == state.id
    //     (deferred until after we know the repo)
    var matchState = null;
    for (var si = 0; si < cfg.states.length; si++) {
      var s = cfg.states[si];
      if (s.id !== payload.to || s.terminal) continue;
      matchState = s;
      break;
    }
    if (!matchState) return;

    var isDispatchable = !!matchState.dispatchable;
    if (!isDispatchable) {
      for (var ti = 0; ti < cfg.transitions.length; ti++) {
        if (cfg.transitions[ti].from === matchState.id && cfg.transitions[ti].type === "gated") {
          isDispatchable = true;
          break;
        }
      }
    }

    // Check if repo has pipeline stages triggered on this dispatchable state
    var card = agentdesk.cards.get(payload.card_id);
    if (!card) return;

    var stages = agentdesk.db.query(
      "SELECT id, stage_name, agent_override_id FROM pipeline_stages WHERE repo_id = ? AND trigger_after = ? ORDER BY stage_order ASC LIMIT 1",
      [card.repo_id, payload.to]
    );
    if (stages.length === 0) {
      // No stages bound to this state — fast path. Emit a diagnostic only
      // when the state was dispatchable by config but had no stages; that
      // mismatch usually indicates a pipeline_stages misconfiguration.
      if (isDispatchable) {
        agentdesk.log.info(
          "[pipeline] Card " + payload.card_id + " entered dispatchable state '" +
          payload.to + "' but no pipeline_stages are registered for repo " +
          (card.repo_id || "<no-repo>")
        );
      }
      return;
    }

    // If we got here via stages.length > 0 but isDispatchable was false (the
    // legacy gated-only heuristic missed the state), surface a warning so
    // operators can confirm the custom pipeline shape was intentional.
    if (!isDispatchable) {
      agentdesk.log.warn(
        "[pipeline] Card " + payload.card_id + " state '" + payload.to +
        "' has registered pipeline_stages but no gated outbound transitions " +
        "and no `dispatchable: true` flag — assigning stage anyway based on " +
        "registered stages; consider marking the state dispatchable in YAML"
      );
    }

    agentdesk.db.execute(
      "UPDATE kanban_cards SET pipeline_stage_id = ?, updated_at = datetime('now') WHERE id = ?",
      [stages[0].id, payload.card_id]
    );
    agentdesk.log.info("[pipeline] Card " + payload.card_id + " assigned to stage: " + stages[0].stage_name);
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
