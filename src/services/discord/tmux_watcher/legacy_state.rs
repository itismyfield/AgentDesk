use super::*;

pub(super) fn initialize_watcher_mode(
    watcher_provider: &ProviderKind,
    channel_id: ChannelId,
) -> WatcherLegacyMode {
    let legacy_mode = WatcherLegacyMode::for_channel(watcher_provider, channel_id);
    #[cfg(test)]
    crate::services::discord::inflight::o_seed_observation::watcher_initialized(
        watcher_provider,
        channel_id.get(),
    );
    legacy_mode
}

pub(super) fn mode_restored_turn(
    legacy_mode: WatcherLegacyMode,
    restored_turn: Option<RestoredWatcherTurn>,
    _watcher_provider: &ProviderKind,
    _channel_id: ChannelId,
) -> Option<RestoredWatcherTurn> {
    #[cfg(test)]
    let watcher_provider = _watcher_provider;
    #[cfg(test)]
    let channel_id = _channel_id;
    #[cfg(test)]
    if !legacy_mode.is_legacy() && restored_turn.is_some() {
        crate::services::discord::inflight::o_seed_observation::record_event(
            watcher_provider,
            channel_id.get(),
            "restored_turn_argument_rejected",
        );
    }
    if legacy_mode.is_legacy() {
        restored_turn
    } else {
        None
    }
}

pub(super) fn checkpoint_row(
    context: &PollWatcherContext<'_>,
    turn_parse_state: &TurnParseState<'_>,
    watcher_turn_nonce: &Option<String>,
) -> Option<InflightTurnState> {
    let legacy_mode = context.legacy_mode;
    let watcher_provider = context.watcher_provider;
    let channel_id = context.channel_id;
    legacy_mode
        .is_legacy()
        .then(|| {
            crate::services::discord::inflight::load_inflight_state(
                watcher_provider,
                channel_id.get(),
            )
        })
        .flatten()
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
