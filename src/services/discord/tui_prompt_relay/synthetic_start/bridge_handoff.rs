//! Transport the admitted synthetic actor allocation, not just its nonce.
use super::*;
use crate::services::discord::inflight::{GuardedSaveOutcome, InflightEpisodePin};

#[derive(Clone)]
struct Witness {
    episode: InflightEpisodePin,
    actor: std::sync::Weak<CancelToken>,
}

static CLAIMS: LazyLock<Mutex<std::collections::HashMap<(String, u64), Witness>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashMap::new()));

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

pub(super) fn record(row: &InflightTurnState, actor: Option<&Arc<CancelToken>>) {
    let Some(actor) = actor else { return };
    if row.effective_relay_owner_kind() != RelayOwnerKind::None
        || row.external_turn_id.as_deref().is_none_or(str::is_empty)
        || row.turn_nonce.as_deref() != actor.turn_nonce()
    {
        return;
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
        return;
    }
    claims.insert(
        (row.provider.clone(), row.channel_id),
        Witness {
            episode: InflightEpisodePin::from_state(row),
            actor: Arc::downgrade(actor),
        },
    );
}

pub(in crate::services::discord::tui_prompt_relay) struct BridgeClaim {
    pub(in crate::services::discord::tui_prompt_relay) row: InflightTurnState,
    pub(in crate::services::discord::tui_prompt_relay) actor: Arc<CancelToken>,
    // Drop the exact lease before releasing serialization to another adapter.
    _lease: TuiDirectExternalInputLeaseGuard,
    _serial: tokio::sync::OwnedMutexGuard<()>,
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
    if let (Some(pool), Some(session_key)) = (shared.pg_pool.as_ref(), row.session_key.as_deref()) {
        crate::db::dispatched_sessions::upsert_hook_session_pg(
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
        )
        .await?;
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
    record(&row, Some(&actor));
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
    if live_lease
        .as_ref()
        .is_some_and(|lease| lease.turn_id != row.external_turn_id)
    {
        return None;
    }
    let pin = InflightEpisodePin::from_state(row);
    let locked =
        super::super::super::inflight::lock_inflight_episode(&provider, row.channel_id, &pin)
            .ok()?;
    let current = locked.state();
    if current.response_sent_offset != 0
        || !current.full_response.is_empty()
        || current.last_watcher_relayed_offset.is_some()
        || current.terminal_delivery_committed
    {
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
        let actor = Arc::new(CancelToken::from_persisted_turn_nonce(
            row.turn_nonce.clone(),
        ));
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
    record(locked.state(), Some(&actor));
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
    drop(serial);
    Some(lease)
}
