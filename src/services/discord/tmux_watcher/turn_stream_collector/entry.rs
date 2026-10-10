use super::*;

// A terminal already parsed by the outgoing task goes through the ordinary
// receipt/lease path. EOF is not a new terminal and no frame is forwarded twice.

// #3805 P2 (PR-C): this turn's status-panel generation epoch, SEEDED from
// the on-disk row so a restart re-hydrating an existing panel carries the
// SAME epoch it was created with (a stale-epoch completion is thus never
// falsely skipped). The two-message create bumps it (opens the epoch on a
// fresh bind); the completion guard proves it against the on-disk epoch.
// Inert on the default-OFF path (stays 0) and while no mid-turn re-anchor
// exists yet (PR-D) — this turn's epoch always equals the on-disk epoch.

// #3041 P1-3 (Part a, B1): DEFERRED forward of the outer-read chunk. We now
// know — from `initial_outcome.found_result` — whether THIS chunk is the
// RESULT-bearing (terminal) one. If so, forward it as a TERMINAL frame
// carrying the commit fence (`terminal_event_consumed_offset(..)` + the
// pinned turn identity loaded at turn start), so the SAME frame that
// triggers the sink's terminal delivery carries the consumed_end + identity
// (FIFO single-task: a separate later frame would arrive after the sink
// already dispatched). Non-terminal chunks forward exactly as before (no
// fence, no streaming-latency change beyond the synchronous parse reorder).
// The ACK target is captured from THIS forward, so the watcher's wait now
// correlates to the terminal frame's sequence (more precise).

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
