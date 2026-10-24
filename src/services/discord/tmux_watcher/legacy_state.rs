use super::*;

#[cfg(test)]
pub(super) fn record_watcher_initialized(watcher_provider: &ProviderKind, channel_id: ChannelId) {
    crate::services::discord::inflight::o_seed_observation::watcher_initialized(
        watcher_provider,
        channel_id.get(),
    );
}

pub(super) fn checkpoint_row(
    context: &PollWatcherContext<'_>,
    turn_parse_state: &TurnParseState<'_>,
    watcher_turn_nonce: &Option<String>,
) -> Option<InflightTurnState> {
    crate::services::discord::inflight::load_inflight_state(
        context.watcher_provider,
        context.channel_id.get(),
    )
    .filter(|row| {
        turn_parse_state
            .watcher_turn_identity
            .as_ref()
            .is_some_and(|identity| identity.matches_state(row))
            && &row.turn_nonce == watcher_turn_nonce
    })
}

#[cfg(test)]
pub(super) fn record_checkpoint(
    context: &PollWatcherContext<'_>,
    checkpoint_turn: Option<&CollectedTurnStream>,
) {
    let watcher_provider = context.watcher_provider;
    let channel_id = context.channel_id;
    if let Some(turn) = checkpoint_turn {
        if turn.startup_inflight_snapshot.is_some() {
            crate::services::discord::inflight::o_seed_observation::record_event(
                watcher_provider,
                channel_id.get(),
                "legacy_row_seed_adopted",
            );
        }
        crate::services::discord::inflight::o_seed_observation::record_parser_response(
            watcher_provider,
            channel_id.get(),
            &turn.full_response,
        );
    }
    crate::services::discord::inflight::o_seed_observation::record_event(
        watcher_provider,
        channel_id.get(),
        "checkpoint",
    );
}
