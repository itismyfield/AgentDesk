use super::*;

// A terminal already parsed by the outgoing task goes through the ordinary
// receipt/lease path. EOF is not a new terminal and no frame is forwarded twice.

// Keep reading until result or timeout
// Check if a Discord turn claimed this data since our epoch snapshot

pub(in crate::services::discord::tmux::tmux_watcher) async fn collect_turn_stream_until_terminal(
    ctx: &TurnStreamCollectorContext,
    io: TurnStreamCollectorIo,
    parser: &mut TurnParseState<'_>,
    relay: &mut SupervisorRelayState<'_>,
    monitor: &mut MonitorAutoTurnState,
    render_seed: &mut RenderSeedState,
) -> CollectOutcome {
    // Keep the collector's state on the heap instead of embedding its large future in callers.
    Box::pin(collect_turn_stream_body(
        ctx,
        io,
        parser,
        relay,
        monitor,
        render_seed,
    ))
    .await
}

pub(super) async fn emit_compaction(
    ctx: &TurnStreamCollectorContext,
    state: &StreamLineState,
) -> bool {
    emit_context_compacted_lifecycle_from_watcher(
        &ctx.shared,
        ctx.channel_id,
        &ctx.watcher_provider,
        state.last_model.as_deref(),
        stream_line_state_token_usage(state),
    )
    .await
}
