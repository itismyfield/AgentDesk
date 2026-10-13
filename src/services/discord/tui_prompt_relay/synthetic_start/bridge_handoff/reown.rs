//! Hand a refused delegation back to the bridge: this turn's undelivered Watcher or
//! session-bound row ends in the state a BridgeAdapter claim would have left.
use super::*;

#[cfg(test)]
pub(in crate::services::discord::tui_prompt_relay::synthetic_start) mod reown_tests;

/// One prompt as the idle observer saw it: its source `E`, the prompt end in `E`,
/// and the lease the observer recorded for it.
pub(in crate::services::discord::tui_prompt_relay) struct ObservedTurn<'a> {
    pub(in crate::services::discord::tui_prompt_relay) provider: &'a ProviderKind,
    pub(in crate::services::discord::tui_prompt_relay) channel: ChannelId,
    pub(in crate::services::discord::tui_prompt_relay) tmux: &'a str,
    pub(in crate::services::discord::tui_prompt_relay) expected_output: &'a Path,
    pub(in crate::services::discord::tui_prompt_relay) prompt_end: u64,
    pub(in crate::services::discord::tui_prompt_relay) lease: &'a ExternalInputRelayLease,
}

#[cfg(test)]
pub(in crate::services::discord::tui_prompt_relay) static REOWN_PAUSE: Mutex<
    Option<(u64, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
> = Mutex::new(None);

/// Whether a delegated actor proof, live or dead, is still held for the channel.
#[cfg(test)]
pub(in crate::services::discord::tui_prompt_relay) fn delegated_proof_held(
    provider: &ProviderKind,
    channel: ChannelId,
) -> bool {
    (DELEGATED.lock().unwrap_or_else(|error| error.into_inner()))
        .contains_key(&(provider.as_str().to_owned(), channel.get()))
}

/// Re-own `own_row` for the bridge, or leave every row, witness and lease as it was.
/// Returns the BridgeAdapter lease now stored for the turn.
pub(in crate::services::discord::tui_prompt_relay) async fn reown_refused_delegation(
    shared: &Arc<SharedData>,
    turn: &ObservedTurn<'_>,
    own_row: &InflightEpisodePin,
) -> Result<ExternalInputRelayLease, &'static str> {
    let (provider, channel, tmux) = (turn.provider, turn.channel, turn.tmux);
    let turn_id = (turn.lease.turn_id.as_deref())
        .filter(|id| !id.is_empty())
        .ok_or("observed turn has no id")?;
    let expected_raw = turn
        .expected_output
        .to_str()
        .ok_or("observed source is not utf-8")?;
    let expected = std::fs::canonicalize(turn.expected_output)
        .map_err(|_| "observed source does not resolve")?;
    // Ordered after the inline or pending claim that adopted this owner, and before any capture.
    let _serial = super::super::super::super::tui_direct_pending_start::channel_lock(
        provider.as_str(),
        channel.get(),
    )
    .lock_owned()
    .await;
    let key = (provider.as_str().to_owned(), channel.get());
    let delegated = (DELEGATED.lock().unwrap_or_else(|error| error.into_inner()))
        .get(&key)
        .cloned()
        .ok_or("no delegated actor proof")?;
    let actor = delegated.actor.upgrade().ok_or("delegated actor is gone")?;
    // The refused episode may have progressed; a change between this read and the lock fails.
    let progressed = super::super::super::super::inflight::load_inflight_state_read_only(
        provider,
        channel.get(),
    )
    .map(|row| InflightEpisodePin::from_state(&row))
    .filter(|pin| own_row.is_same_episode_as(pin))
    .ok_or("refused row was replaced")?;
    let mut locked = super::super::super::super::inflight::lock_inflight_episode(
        provider,
        channel.get(),
        &progressed,
    )
    .map_err(|_| "refused row was replaced")?;
    let row = locked.state().clone();
    if !undelivered_delegation_of(&row, turn, turn_id, &actor)
        || !(delegated.episode).is_same_episode_as(&InflightEpisodePin::from_state(&row))
    {
        return Err("row is not this turn's undelivered delegation");
    }
    if body_committed(shared, &row) {
        return Err("committed frontier covers the turn body");
    }
    if actor.cancelled.load(Ordering::Relaxed)
        || !actor_is_current(shared, channel, MessageId::new(row.user_msg_id), &actor).await
    {
        return Err("delegated actor is not the current mailbox turn");
    }
    let live_lease = crate::services::tui_prompt_dedupe::external_input_relay_lease(
        provider.as_str(),
        tmux,
        channel.get(),
    )
    .filter(|live| live.turn_id == row.external_turn_id && live.session_key == row.session_key)
    .ok_or("live lease names another turn")?;
    let mut lease = live_lease;
    lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
    let mut updated = row.clone();
    updated.set_relay_owner_kind(RelayOwnerKind::None);
    if row.output_path.as_deref() != Some(expected_raw) {
        // Capture compares the raw path; only another file moves the start to the prompt end.
        if row
            .output_path
            .as_deref()
            .and_then(|path| std::fs::canonicalize(path).ok())
            != Some(expected.clone())
        {
            updated.turn_start_offset = Some(turn.prompt_end);
            updated.last_offset = turn.prompt_end;
        }
        updated.output_path = Some(expected_raw.to_owned());
    }
    updated.restamp_external_turn_lease(&lease);
    #[cfg(test)]
    pause_for_test(&REOWN_PAUSE, channel).await;
    {
        // Watcher inserts take this lock, so none can start reading `E` before the save.
        let _registry = super::super::super::super::lock_tmux_watcher_registry();
        if live_watcher_may_read(&shared.tmux_watchers, tmux, &expected) {
            return Err("a live watcher may read the observed source");
        }
        if locked.persist_verified_under_guard(&updated, "tui_direct_refused_delegation_reown")
            != GuardedSaveOutcome::Saved
        {
            return Err("re-owned row was not saved");
        }
    }
    if !record(locked.state(), Some(&actor), delegated.pg_pin.clone()) {
        let _ = locked.persist_verified_under_guard(&row, "tui_direct_refused_delegation_reown");
        return Err("bridge witness was refused");
    }
    (DELEGATED.lock().unwrap_or_else(|error| error.into_inner())).remove(&key);
    drop(locked);
    let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        provider.as_str(),
        tmux,
        lease,
    );
    tracing::info!(
        provider = provider.as_str(),
        channel_id = channel.get(),
        tmux_session_name = tmux,
        turn_id,
        previous_output = row.output_path.as_deref().unwrap_or(""),
        output = expected_raw,
        turn_start_offset = updated.turn_start_offset.unwrap_or(0),
        "re-owned a refused TUI-direct delegation for the bridge; delivery still goes through capture"
    );
    Ok(lease)
}

/// This turn's row, still delegated, with nothing of its body sent or committed.
fn undelivered_delegation_of(
    row: &InflightTurnState,
    turn: &ObservedTurn<'_>,
    turn_id: &str,
    actor: &CancelToken,
) -> bool {
    row.turn_source == TurnSource::ExternalInput
        && row.tmux_session_name.as_deref() == Some(turn.tmux)
        && row.external_turn_id.as_deref() == Some(turn_id)
        && row.session_key == turn.lease.session_key
        && matches!(
            row.effective_relay_owner_kind(),
            RelayOwnerKind::Watcher | RelayOwnerKind::SessionBoundRelay
        )
        && row.user_msg_id != 0
        && row.injected_prompt_message_id == Some(row.user_msg_id)
        && row.turn_nonce.is_some()
        && row.turn_nonce.as_deref() == actor.turn_nonce()
        && row.current_msg_id == 0
        && row.response_sent_offset == 0
        && row.full_response.is_empty()
        && row.last_watcher_relayed_offset.is_none()
        && !row.terminal_delivery_committed
        && !row.session_bound_delivered
        && row.restart_mode.is_none()
        && !row.rebind_origin
}

/// The orphan-reclaim test: a frontier past the start that reaches EOF already delivered the body.
fn body_committed(shared: &SharedData, row: &InflightTurnState) -> bool {
    let Some(provider) = row.provider_kind() else {
        return true;
    };
    let eof = (row.output_path.as_deref())
        .and_then(|path| std::fs::metadata(path).ok())
        .map(|metadata| metadata.len());
    let committed = crate::services::discord::outbound::delivery_record::effective_committed_offset(
        shared,
        &provider,
        ChannelId::new(row.channel_id),
        row.tmux_session_name.as_deref().unwrap_or(""),
        eof,
    );
    committed > row.turn_start_offset.unwrap_or(row.last_offset)
        && eof.is_none_or(|eof| committed >= eof)
}

/// A relay-live watcher on `tmux` whose file is `expected`, or cannot be told apart from it.
fn live_watcher_may_read(
    watchers: &super::super::super::super::TmuxWatcherRegistry,
    tmux: &str,
    expected: &Path,
) -> bool {
    watchers.by_tmux_session.get(tmux).is_some_and(|entry| {
        !entry.cancel.load(Ordering::Relaxed)
            && !entry.heartbeat_stale()
            && std::fs::canonicalize(&entry.output_path).map_or(true, |watched| watched == expected)
    })
}
