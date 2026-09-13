//! Transport the admitted synthetic actor allocation, not just its nonce.
use super::*;
use crate::db::dispatched_session_canonical_identity::{
    self as session_actor, HookSessionActorPin,
};
use crate::services::discord::inflight::{GuardedSaveOutcome, InflightEpisodePin};

#[derive(Clone)]
struct Witness {
    episode: InflightEpisodePin,
    actor: std::sync::Weak<CancelToken>,
    pg_pin: Option<HookSessionActorPin>,
}

static CLAIMS: LazyLock<Mutex<std::collections::HashMap<(String, u64), Witness>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

#[cfg(test)]
pub(in crate::services::discord::tui_prompt_relay) static ADMISSION_PAUSE: Mutex<
    Option<(u64, Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
> = Mutex::new(None);

#[cfg(test)]
pub(super) async fn pause_after_admission_for_test(channel: ChannelId) {
    let pause = {
        let mut slot = ADMISSION_PAUSE.lock().unwrap();
        if slot.as_ref().is_some_and(|(id, _, _)| *id == channel.get()) {
            slot.take()
        } else {
            None
        }
    };
    if let Some((_, entered, resume)) = pause {
        entered.notify_one();
        resume.notified().await;
    }
}

pub(super) async fn actor_is_current(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    anchor: MessageId,
    actor: &Arc<CancelToken>,
) -> bool {
    let current = super::super::super::mailbox_snapshot(shared, channel).await;
    current.active_user_message_id == Some(anchor)
        && current
            .cancel_token
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(active, actor))
}

pub(super) fn refresh_actor_matches(
    row: &InflightTurnState,
    actor: Option<&Arc<CancelToken>>,
    freshly_admitted: bool,
) -> bool {
    if row.effective_relay_owner_kind() != RelayOwnerKind::None {
        return true;
    }
    let Some(actor) = actor else { return false };
    let claims = CLAIMS.lock().unwrap_or_else(|error| error.into_inner());
    match claims
        .get(&(row.provider.clone(), row.channel_id))
        .and_then(|witness| witness.actor.upgrade().map(|saved| (witness, saved)))
    {
        Some((witness, saved)) => witness.episode.matches_state(row) && Arc::ptr_eq(&saved, actor),
        None => freshly_admitted,
    }
}

pub(super) async fn capture_session_pin(
    shared: &Arc<SharedData>,
    session_key: Option<&str>,
) -> Result<Option<HookSessionActorPin>, String> {
    match (shared.pg_pool.as_ref(), session_key) {
        (Some(pool), Some(key)) => session_actor::capture_hook_session_actor_pin_pg(pool, key)
            .await
            .map(Some)
            .map_err(|error| format!("{error:?}")),
        _ => Ok(None),
    }
}

/// Preserve a detached live allocation before entering a mailbox slot.
pub(super) async fn prepare_admission(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    anchor: MessageId,
    lease: &ExternalInputRelayLease,
) -> Result<(Arc<CancelToken>, Option<HookSessionActorPin>), String> {
    let pg_pin = capture_session_pin(shared, lease.session_key.as_deref()).await?;
    let retained =
        super::super::super::inflight::load_inflight_state_read_only(provider, channel.get())
            .filter(|row| row.user_msg_id == anchor.get())
            .map(|row| retained_actor(&row))
            .transpose()
            .map_err(|()| "synthetic original actor proof changed")?;
    Ok((
        retained
            .flatten()
            .unwrap_or_else(|| Arc::new(CancelToken::new())),
        pg_pin,
    ))
}

pub(super) async fn refresh_existing(
    shared: &Arc<SharedData>,
    mut row: InflightTurnState,
    lease: &ExternalInputRelayLease,
    relay_owner: ExternalInputRelayOwner,
    owner_kind: RelayOwnerKind,
    admission: (Option<&Arc<CancelToken>>, bool),
    pg_pin: Option<HookSessionActorPin>,
) -> TuiDirectSyntheticTurnClaim {
    let (actor, freshly_admitted) = admission;
    use super::super::super::inflight;
    let pg_pin = original_session_pin(&row, actor).unwrap_or(pg_pin);
    let expected = inflight::InflightTurnIdentity::from_state(&row);
    let start = row.turn_start_offset.unwrap_or(row.last_offset);
    row.turn_nonce = actor.and_then(|actor| actor.turn_nonce().map(str::to_owned));
    row.set_relay_owner_kind(owner_kind);
    row.restamp_external_turn_lease(lease);
    // Preserve the original source boundary and both consumed/delivered progress.
    let saved = inflight::save_inflight_state_if_identity_matches_allow_output_restamp(
        &row,
        &expected,
        "tui_direct_synthetic_refresh",
    );
    let claimed = if saved == GuardedSaveOutcome::Saved {
        record_admitted(shared, &row, actor, pg_pin, freshly_admitted).await
    } else {
        if freshly_admitted {
            release_unrecorded_actor(shared, &row, actor, false).await;
        }
        false
    };
    TuiDirectSyntheticTurnClaim::new(relay_owner, claimed, start)
}

pub(super) async fn record_admitted(
    shared: &Arc<SharedData>,
    row: &InflightTurnState,
    actor: Option<&Arc<CancelToken>>,
    pg_pin: Option<HookSessionActorPin>,
    freshly_admitted: bool,
) -> bool {
    let still_owned = match actor {
        Some(actor) => {
            actor_is_current(
                shared,
                ChannelId::new(row.channel_id),
                MessageId::new(row.user_msg_id),
                actor,
            )
            .await
        }
        None => false,
    };
    let claimed = still_owned && record(row, actor, pg_pin);
    if freshly_admitted {
        if claimed {
            super::super::super::increment_global_active(shared, "tui_direct_synthetic_claim");
            shared
                .turn_start_times
                .insert(ChannelId::new(row.channel_id), std::time::Instant::now());
        } else {
            release_unrecorded_actor(shared, row, actor, false).await;
        }
    }
    claimed
}

pub(super) async fn release_unrecorded_actor(
    shared: &Arc<SharedData>,
    row: &InflightTurnState,
    actor: Option<&Arc<CancelToken>>,
    counted: bool,
) {
    let Some(actor) = actor else { return };
    let provider = ProviderKind::from_str_or_unsupported(row.provider.as_str());
    let result = super::super::super::mailbox_finish::mailbox_finish_turn_if_matches_episode_started_before_with_actor_without_completion(
        shared, &provider, ChannelId::new(row.channel_id), MessageId::new(row.user_msg_id),
        row.turn_nonce.clone(), std::time::Instant::now(), Some(actor.clone()),
    ).await;
    if counted && result.removed_token.is_some() {
        super::super::super::saturating_decrement_global_active(shared);
    }
}

pub(super) fn original_session_pin(
    row: &InflightTurnState,
    actor: Option<&Arc<CancelToken>>,
) -> Option<Option<HookSessionActorPin>> {
    let claims = CLAIMS.lock().unwrap_or_else(|error| error.into_inner());
    let witness = claims.get(&(row.provider.clone(), row.channel_id))?;
    let saved = witness.actor.upgrade()?;
    (actor.is_some_and(|actor| Arc::ptr_eq(&saved, actor)) && witness.episode.matches_state(row))
        .then(|| witness.pg_pin.clone())
}

pub(super) fn retained_actor(row: &InflightTurnState) -> Result<Option<Arc<CancelToken>>, ()> {
    let claims = CLAIMS.lock().unwrap_or_else(|error| error.into_inner());
    let Some((witness, actor)) = claims
        .get(&(row.provider.clone(), row.channel_id))
        .and_then(|witness| witness.actor.upgrade().map(|actor| (witness, actor)))
    else {
        return Ok(None);
    };
    if !witness.episode.matches_state(row) || actor.cancelled.load(Ordering::Relaxed) {
        return Err(());
    }
    Ok(Some(actor))
}

/// Successful typed admission can learn the native SID and canonical source
/// path. Advance only its existing allocation witness under the same lock;
/// a missing witness or a successor allocation cannot be replaced here.
/// The admission caller still holds its durable row and source guards.
pub(in crate::services::discord) fn preserve_admitted_source(
    before: &InflightEpisodePin,
    admitted: &InflightTurnState,
    actor: &Arc<CancelToken>,
) {
    let mut claims = CLAIMS.lock().unwrap_or_else(|error| error.into_inner());
    if let Some(witness) = claims
        .get_mut(&(admitted.provider.clone(), admitted.channel_id))
        .filter(|witness| {
            witness.episode == *before
                && witness
                    .actor
                    .upgrade()
                    .is_some_and(|saved| Arc::ptr_eq(&saved, actor))
        })
    {
        witness.episode = InflightEpisodePin::from_state(admitted);
    }
}

pub(super) fn record(
    row: &InflightTurnState,
    actor: Option<&Arc<CancelToken>>,
    pg_pin: Option<HookSessionActorPin>,
) -> bool {
    let Some(actor) = actor else { return false };
    if row.effective_relay_owner_kind() != RelayOwnerKind::None {
        return true;
    }
    if row.external_turn_id.as_deref().is_none_or(str::is_empty)
        || row.turn_nonce.as_deref() != actor.turn_nonce()
    {
        return false;
    }
    let mut claims = CLAIMS.lock().unwrap_or_else(|error| error.into_inner());
    claims.retain(|_, witness| witness.actor.strong_count() > 0);
    if claims
        .get(&(row.provider.clone(), row.channel_id))
        .is_some_and(|witness| {
            witness.episode.matches_state(row)
                && witness
                    .actor
                    .upgrade()
                    .is_some_and(|saved| !Arc::ptr_eq(&saved, actor))
        })
    {
        return false;
    }
    claims.insert(
        (row.provider.clone(), row.channel_id),
        Witness {
            episode: InflightEpisodePin::from_state(row),
            actor: Arc::downgrade(actor),
            pg_pin,
        },
    );
    true
}

pub(in crate::services::discord::tui_prompt_relay) struct BridgeClaim {
    pub(in crate::services::discord::tui_prompt_relay) row: InflightTurnState,
    pub(in crate::services::discord::tui_prompt_relay) actor: Arc<CancelToken>,
    // Drop the exact lease before releasing serialization to another adapter.
    _lease: TuiDirectExternalInputLeaseGuard,
    _serial: tokio::sync::OwnedMutexGuard<()>,
}

impl BridgeClaim {
    /// A rollover keeps the same captured actor/source and records its old
    /// anchor among frozen chunks. Carry that exact transition into recovery.
    pub(in crate::services::discord::tui_prompt_relay) async fn preserve_continuation(
        &self,
        shared: &Arc<SharedData>,
    ) {
        let provider = ProviderKind::from_str_or_unsupported(&self.row.provider);
        let Some(row) = super::super::super::inflight::load_inflight_state_read_only(
            &provider,
            self.row.channel_id,
        ) else {
            return;
        };
        let mut before_rollover = row.clone();
        before_rollover.current_msg_id = self.row.current_msg_id;
        if !InflightEpisodePin::from_state(&self.row).matches_state(&before_rollover)
            || row.external_turn_id != self.row.external_turn_id
            || row.session_key != self.row.session_key
            || (row.current_msg_id != self.row.current_msg_id
                && !row
                    .streaming_rollover_frozen_msg_ids
                    .contains(&self.row.current_msg_id))
        {
            return;
        }
        let active =
            super::super::super::mailbox_snapshot(shared, ChannelId::new(row.channel_id)).await;
        if active
            .cancel_token
            .as_ref()
            .is_some_and(|active| !Arc::ptr_eq(active, &self.actor))
        {
            return;
        }
        let Some(pin) = original_session_pin(&self.row, Some(&self.actor)) else {
            return;
        };
        let Ok(locked) = super::super::super::inflight::lock_inflight_episode(
            &provider,
            row.channel_id,
            &InflightEpisodePin::from_state(&row),
        ) else {
            return;
        };
        record(locked.state(), Some(&self.actor), pin);
    }
}

pub(in crate::services::discord::tui_prompt_relay) async fn capture(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    tmux: &str,
    output: &Path,
    lease: &ExternalInputRelayLease,
) -> Result<BridgeClaim, String> {
    let failure = || "synthetic bridge has no verified delivery actor".to_string();
    let key = (provider.as_str().to_owned(), channel.get());
    let deadline = tokio::time::Instant::now()
        + super::super::super::tui_direct_pending_start::PENDING_START_BACKSTOP;
    let (serial, witness, actor) = loop {
        let serial = super::super::super::tui_direct_pending_start::channel_lock(
            provider.as_str(),
            channel.get(),
        )
        .lock_owned()
        .await;
        let witness = CLAIMS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
            .cloned();
        if let Some(witness) = witness
            && let Some(actor) = witness.actor.upgrade()
            && super::super::super::inflight::load_inflight_state_read_only(provider, channel.get())
                .is_some_and(|row| witness.episode.matches_state(&row))
        {
            break (serial, witness, actor);
        }
        drop(serial);
        if tokio::time::Instant::now() >= deadline {
            return Err(failure());
        }
        tokio::time::sleep(super::super::super::tui_direct_pending_start::PENDING_START_POLL).await;
    };
    let snapshot = super::super::super::mailbox_snapshot(shared, channel).await;
    if snapshot
        .cancel_token
        .as_ref()
        .is_none_or(|active| !Arc::ptr_eq(active, &actor))
    {
        return Err(failure());
    }
    let live_lease = crate::services::tui_prompt_dedupe::external_input_relay_lease(
        provider.as_str(),
        tmux,
        channel.get(),
    )
    .ok_or_else(failure)?;
    let locked = super::super::super::inflight::lock_inflight_episode(
        provider,
        channel.get(),
        &witness.episode,
    )
    .map_err(|_| failure())?;
    let mut row = locked.state().clone();
    if row.external_turn_id.as_deref().is_none_or(str::is_empty)
        || row.external_turn_id != lease.turn_id
        || row.external_turn_id != live_lease.turn_id
        || live_lease.relay_owner != ExternalInputRelayOwner::BridgeAdapter
        || row.session_key != lease.session_key
        || row.output_path.as_deref().map(Path::new) != Some(output)
        || row.tmux_session_name.as_deref() != Some(tmux)
        || row.turn_source != TurnSource::ExternalInput
        || row.terminal_delivery_committed
        || snapshot.active_user_message_id.map(MessageId::get) != Some(row.user_msg_id)
        || row.injected_prompt_message_id != Some(row.user_msg_id)
        || row.user_msg_id == 0
    {
        return Err(failure());
    }
    drop(locked);
    let mut pg_pin = witness.pg_pin.clone();
    if let (Some(pool), Some(session_key)) = (shared.pg_pool.as_ref(), row.session_key.as_deref()) {
        pg_pin = Some(
            session_actor::upsert_hook_session_with_actor_pin_pg(
                pool,
                crate::db::dispatched_sessions::HookSessionUpsert {
                    session_key,
                    provider: provider.as_str(),
                    status: "turn_active",
                    channel_id: Some(&channel.get().to_string()),
                    turn_start_nonce: row.turn_nonce.as_deref(),
                    instance_id: None,
                    agent_id: None,
                    session_info: None,
                    model: None,
                    tokens: None,
                    cwd: None,
                    active_dispatch_id: None,
                    thread_channel_id: None,
                    claude_session_id: None,
                    raw_provider_session_id: None,
                    dispatched_origin: false,
                },
                pg_pin.as_ref().ok_or_else(failure)?,
            )
            .await
            .map_err(|error| format!("{error:?}"))?,
        );
    }
    let current = super::super::super::mailbox_snapshot(shared, channel).await;
    if current
        .cancel_token
        .as_ref()
        .is_none_or(|active| !Arc::ptr_eq(active, &actor))
    {
        return Err(failure());
    }
    let mut saved = super::super::super::inflight::lock_inflight_episode(
        provider,
        channel.get(),
        &witness.episode,
    )
    .map_err(|_| failure())?;
    if saved.state().restart_mode.is_some()
        && saved.mark_readopted_under_guard() != GuardedSaveOutcome::Saved
    {
        return Err(failure());
    }
    if saved.state().current_msg_id == 0
        && saved.bind_synthetic_anchor_under_guard() != GuardedSaveOutcome::Saved
    {
        return Err(failure());
    }
    row = saved.state().clone();
    drop(saved);
    if !record(&row, Some(&actor), pg_pin) {
        return Err(failure());
    }
    Ok(BridgeClaim {
        row,
        actor,
        _lease: TuiDirectExternalInputLeaseGuard::new(provider.clone(), tmux, channel, &live_lease),
        _serial: serial,
    })
}

/// Resume a persisted, never-published synthetic episode using its original
/// source boundary. A registered mailbox actor must carry the saved allocation;
/// after restart an empty mailbox gets a freshly admitted allocation instead.
#[cfg(unix)]
pub(in crate::services::discord::tui_prompt_relay) async fn resume_unpublished(
    shared: &Arc<SharedData>,
    row: &InflightTurnState,
    output: &Path,
) -> Option<ExternalInputRelayLease> {
    let claim = capture_dormant(shared, row, output, true).await?;
    Some(claim.lease.clone())
}

pub(in crate::services::discord) struct DormantSyntheticClaim {
    pub(in crate::services::discord) row: InflightTurnState,
    pub(in crate::services::discord) actor: Arc<CancelToken>,
    lease: ExternalInputRelayLease,
    _lease: Option<TuiDirectExternalInputLeaseGuard>,
    _serial: tokio::sync::OwnedMutexGuard<()>,
}

#[cfg(unix)]
pub(in crate::services::discord) async fn capture_dormant_partial(
    shared: &Arc<SharedData>,
    row: &InflightTurnState,
    output: &Path,
) -> Option<DormantSyntheticClaim> {
    capture_dormant(shared, row, output, false).await
}

#[cfg(unix)]
async fn capture_dormant(
    shared: &Arc<SharedData>,
    row: &InflightTurnState,
    output: &Path,
    unpublished_only: bool,
) -> Option<DormantSyntheticClaim> {
    let provider = row.provider_kind()?;
    let channel = ChannelId::new(row.channel_id);
    let tmux = row.tmux_session_name.as_deref()?;
    let serial = super::super::super::tui_direct_pending_start::channel_lock(
        provider.as_str(),
        row.channel_id,
    )
    .try_lock_owned()
    .ok()?;
    if row.turn_source != TurnSource::ExternalInput
        || row.runtime_kind != Some(RuntimeHandoffKind::ClaudeTui)
        || row.user_msg_id == 0
        || row.request_owner_user_id != TUI_DIRECT_SYNTHETIC_OWNER_USER_ID
        || row.injected_prompt_message_id != Some(row.user_msg_id)
        || row.effective_relay_owner_kind() != RelayOwnerKind::None
        || row.output_path.as_deref().map(Path::new) != Some(output)
        || row.external_turn_id.as_deref().is_none_or(str::is_empty)
        || row.rebind_origin
        || row.turn_start_offset.is_none()
        || row.turn_nonce.as_deref().is_none_or(str::is_empty)
        || CLAUDE_IDLE_RESPONSE_TAILS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains(tmux)
        || tui_direct_watcher_can_own_output(&shared.tmux_watchers, tmux, Some(output))
        || crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
            .get_live_producer(tmux)
            .is_some()
    {
        return None;
    }
    let live_lease = crate::services::tui_prompt_dedupe::external_input_relay_lease(
        provider.as_str(),
        tmux,
        row.channel_id,
    );
    if live_lease.as_ref().is_some_and(|lease| {
        lease.turn_id != row.external_turn_id
            || lease.session_key != row.session_key
            || lease.relay_owner != ExternalInputRelayOwner::BridgeAdapter
    }) {
        return None;
    }
    let captured_pg_pin = capture_session_pin(shared, row.session_key.as_deref())
        .await
        .ok()?;
    let pin = InflightEpisodePin::from_state(row);
    let locked =
        super::super::super::inflight::lock_inflight_episode(&provider, row.channel_id, &pin)
            .ok()?;
    let current = locked.state();
    let resumable_body = if unpublished_only {
        current.response_sent_offset == 0
            && current.full_response.is_empty()
            && current.last_watcher_relayed_offset.is_none()
    } else {
        current.response_sent_offset < current.full_response.len()
    };
    if !resumable_body || current.terminal_delivery_committed {
        return None;
    }
    let snapshot = super::super::super::mailbox_snapshot(shared, channel).await;
    let actor = if let Some(actor) = snapshot.cancel_token {
        let proven = CLAIMS
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&(provider.as_str().to_owned(), row.channel_id))
            .is_some_and(|witness| {
                witness.episode == pin
                    && witness
                        .actor
                        .upgrade()
                        .is_some_and(|saved| Arc::ptr_eq(&saved, &actor))
            });
        if !proven || actor.cancelled.load(Ordering::Relaxed) {
            return None;
        }
        actor
    } else {
        // Re-admit the original allocation while its proof remains live.
        let retained = retained_actor(row).ok()?;
        if retained.is_none()
            && captured_pg_pin
                .as_ref()
                .is_some_and(|pin| pin.active_for_other_actor(row.turn_nonce.as_deref()))
        {
            return None;
        }
        let actor = retained.unwrap_or_else(|| {
            Arc::new(CancelToken::from_persisted_turn_nonce(
                row.turn_nonce.clone(),
            ))
        });
        if !super::super::super::mailbox_try_start_turn(
            shared,
            channel,
            actor.clone(),
            serenity::UserId::new(row.request_owner_user_id),
            MessageId::new(row.user_msg_id),
        )
        .await
        {
            return None;
        }
        super::super::super::increment_global_active(shared, "synthetic_bridge_resume");
        shared
            .turn_start_times
            .insert(channel, std::time::Instant::now());
        actor
    };
    let pg_pin = original_session_pin(locked.state(), Some(&actor)).unwrap_or(captured_pg_pin);
    if !record(locked.state(), Some(&actor), pg_pin) {
        release_unrecorded_actor(shared, locked.state(), Some(&actor), true).await;
        return None;
    }
    let current = locked.state().clone();
    drop(locked);
    let mut lease = ExternalInputRelayLease::unassigned(Some(row.channel_id));
    lease.turn_id = row.external_turn_id.clone();
    lease.session_key = row.session_key.clone();
    lease.runtime_kind = row.runtime_kind;
    lease.relay_owner = ExternalInputRelayOwner::BridgeAdapter;
    let lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
        provider.as_str(),
        tmux,
        lease,
    );
    Some(DormantSyntheticClaim {
        row: current,
        actor,
        _lease: (!unpublished_only)
            .then(|| TuiDirectExternalInputLeaseGuard::new(provider, tmux, channel, &lease)),
        lease,
        _serial: serial,
    })
}
