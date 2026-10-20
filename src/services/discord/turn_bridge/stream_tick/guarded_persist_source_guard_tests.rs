//! Source guard for the production stream tick, loop exit and exit settlement order.

pub(super) fn production_tick_reconciles_anchor_dirty_flush_and_exit_candidate_merges() {
    let tick = [
        include_str!("../stream_tick.rs"),
        include_str!("output.rs"),
        include_str!("runtime.rs"),
    ]
    .map(|source| {
        source
            .split("#[cfg(test)]")
            .next()
            .expect("production stream tick prefix")
    })
    .join("\n");
    let production_tick = tick
        .split("#[cfg(test)]")
        .next()
        .expect("production stream tick prefix");
    let flush_predicate = production_tick
        .find("if state_dirty")
        .expect("production flush predicate remains present");
    let pending = production_tick[flush_predicate..]
        .find("|| pending_current_message_candidate.is_some()")
        .map(|offset| flush_predicate + offset)
        .expect("pending response candidate forces a guarded retry");
    let persist = production_tick[pending..]
        .find("persist_stream_tick_state_with_candidate_cleanup(")
        .map(|offset| pending + offset)
        .expect("forced retry reaches candidate-aware persistence");
    assert!(flush_predicate < pending && pending < persist);

    let anchor_preflight = production_tick
        .find("turn_bridge::stream_tick::anchor_preflight")
        .expect("unsaved response is persisted before anchor send");
    let ensure = production_tick
        .find("ensure_bridge_current_message_anchor(")
        .expect("production tick materializes an absent anchor");
    let reconcile = production_tick[ensure..]
        .find("reconcile_tick_runtime_from_inflight!(")
        .map(|offset| ensure + offset)
        .expect("anchor await refreshes every detached tick snapshot");
    let relay_gate = production_tick[reconcile..]
        .find("if !bridge_stream_relay_suppressed(")
        .map(|offset| reconcile + offset)
        .expect("refreshed relay ownership gates bridge output");
    assert!(anchor_preflight < ensure && ensure < reconcile && reconcile < relay_gate);

    let dirty_flush = production_tick
        .find("turn_bridge::stream_tick::dirty_flush")
        .expect("ordinary dirty flush remains guarded");
    let dirty_anchor = production_tick[..dirty_flush]
        .rfind("let current_msg_id_before_flush = current_msg_id;")
        .expect("dirty flush captures the pre-save edit-cache anchor");
    let saved_reconcile = production_tick[dirty_flush..]
        .find("reconcile_tick_runtime_from_inflight!(current_msg_id_before_flush);")
        .map(|offset| dirty_flush + offset)
        .expect("ordinary Saved flush refreshes detached tick state");
    let tick_writeback = production_tick[saved_reconcile..]
        .find("writeback_tick_state!();")
        .map(|offset| saved_reconcile + offset)
        .expect("reconciled tick state is written back");
    assert!(
        dirty_anchor < dirty_flush
            && dirty_flush < saved_reconcile
            && saved_reconcile < tick_writeback
    );

    let rollover_send = production_tick
        .find("TurnGateway::send_message(gateway.as_ref(), channel_id, visible_anchor)")
        .expect("rollover creates a fresh tail candidate");
    let immediate_rollover_bind = production_tick[rollover_send..]
        .find("turn_bridge::stream_tick::rollover_candidate_bind")
        .map(|offset| rollover_send + offset)
        .expect("every rollover candidate is guarded before another loop iteration");
    let post_rollover_panel = production_tick[immediate_rollover_bind..]
        .find("turn_bridge::stream_tick::panel_reanchor")
        .map(|offset| immediate_rollover_bind + offset)
        .expect("post-rollover work starts only after the candidate bind");
    assert!(rollover_send < immediate_rollover_bind);
    assert!(immediate_rollover_bind < post_rollover_panel);

    let parent_tick = include_str!("../stream_tick.rs")
        .split("#[cfg(test)]")
        .next()
        .expect("production parent tick prefix");
    let parent_relay_gate = parent_tick
        .find("if !bridge_stream_relay_suppressed(")
        .expect("parent relay gate remains present");
    let rollover_call = parent_tick
        .find("output::run_rollover_loop(")
        .expect("parent tick calls the rollover phase");
    let response_call = parent_tick
        .find("output::refresh_streaming_response(")
        .expect("parent tick calls the response refresh phase");
    assert!(
        parent_relay_gate < rollover_call && rollover_call < response_call,
        "parent tick gates rollover before refreshing the response"
    );

    let stream_loop = include_str!("../stream_loop.rs");
    let persistent_dirty = stream_loop
        .find("let mut state_dirty = false;")
        .expect("save retry state is initialized once");
    let outer = stream_loop
        .find("'outer: while")
        .expect("production stream loop remains present");
    let tick_call = stream_loop[outer..]
        .find("let tick_outcome = run_bridge_stream_tick(")
        .map(|offset| outer + offset)
        .expect("stream loop observes the explicit tick outcome");
    let authority_loss = stream_loop[tick_call..]
        .find("if tick_outcome == StreamTickOutcome::AuthorityLost")
        .map(|offset| tick_call + offset)
        .expect("tick authority loss reaches the stream loop");
    let authority_break = stream_loop[authority_loss..]
        .find("break 'outer;")
        .map(|offset| authority_loss + offset)
        .expect("authority loss immediately stops stream draining");
    let writeback = stream_loop[outer..]
        .find("*state.inflight_state = inflight_state;")
        .map(|offset| outer + offset)
        .expect("detached state is staged for exit settlement");
    let exit_settlement_call = stream_loop[writeback..]
        .find("settle_and_reconcile_exit_candidate(")
        .map(|offset| writeback + offset)
        .expect("stream-loop exit delegates remaining-candidate settlement");
    assert!(
        persistent_dirty < outer
            && outer < tick_call
            && tick_call < authority_loss
            && authority_loss < authority_break
            && authority_break < writeback
            && writeback < exit_settlement_call
    );

    let exit_reconcile = include_str!("../stream_loop/exit_reconcile.rs");
    let pre_settle_anchor = exit_reconcile
        .find("let current_msg_id_before_exit_settle = *context.state.current_msg_id;")
        .expect("candidate edit-cache identity is captured before settlement");
    let settle = exit_reconcile[pre_settle_anchor..]
        .find("settle_pending_current_message_candidate_on_loop_exit(")
        .map(|offset| pre_settle_anchor + offset)
        .expect("stream-loop exit settles a remaining candidate");
    let saved_reconcile = exit_reconcile[settle..]
        .find("reconcile_saved_exit_candidate(")
        .map(|offset| settle + offset)
        .expect("successful exit merge refreshes caller-owned state");
    assert!(pre_settle_anchor < settle && settle < saved_reconcile);
}
