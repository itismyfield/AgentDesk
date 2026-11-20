//! Reuse the inflight watcher claim for a verified live Herdr execution.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedObservation, HostedRecord,
};
use crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session;

pub(super) async fn restore_channel(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> bool {
    use super::super::herdr_reattach::{VerifiedReattach, continue_channel};
    use crate::services::discord::session_transition::SessionTransitionLease;
    let Ok(transition) = SessionTransitionLease::try_acquire(shared, channel) else {
        return false;
    };
    let restored = matches!(
        continue_channel(shared, provider, channel, &transition).await,
        Some(VerifiedReattach::Live | VerifiedReattach::AwaitingClear)
    );
    drop(transition);
    if restored {
        drop(shared.session_transition_completion(channel));
    }
    restored
}

pub(in crate::services::discord::recovery_engine) async fn attach(
    shared: &Arc<SharedData>,
    row: &HostedObservation,
) -> bool {
    let HostedRecord::Known(record) = &row.record else {
        return false;
    };
    let Some(channel_id) = record
        .owner
        .channel_id
        .parse::<u64>()
        .ok()
        .and_then(inflight::opt_channel_id)
    else {
        return false;
    };
    let provider = &shared.provider;
    let logical = &record.owner.logical_key;
    let Ok(mut inflight) =
        inflight::load_inflight_state_read_only_result(provider, channel_id.get())
    else {
        return false;
    };
    if let Some(state) = inflight.as_mut()
        && state.output_path.is_none()
    {
        let expected = state.clone();
        let record = record.clone();
        let id = row.session_id();
        let Ok(Some(saved)) =
            tokio::task::spawn_blocking(move || recover_pathless(id, &record, &expected)).await
        else {
            return false;
        };
        *state = saved;
    }
    let active = inflight.as_ref().filter(|state| {
        state.provider == record.owner.provider
            && state.tmux_session_name.as_deref() == Some(logical)
            && state.hosted_record_id == Some(row.session_id())
            && state.hosted_execution_nonce.as_deref() == Some(&record.execution_nonce)
            && state.tui_terminal_kind.is_none()
            && state.terminal_receipt.is_none()
            && state.herdr_before_start_stop.is_none()
            && !state.rebind_origin
    });
    if inflight.is_some() && active.is_none() {
        return false;
    }
    let Some(binding) = runtime_binding_for_tmux_session(logical) else {
        // An own Pending clear installs its source on the first prompt.
        return active.is_none();
    };
    let output_path = binding.relay_output_path();
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return active.is_none();
    };
    if active.is_some_and(|state| state.output_path.as_deref() != Some(output_path)) {
        return false;
    }
    let finish_mailbox = if let Some(state) = active {
        let adopted = reregister_active_turn_from_inflight(shared, state).await;
        if !adopted && state.request_owner_user_id != 0 {
            return false;
        }
        restore_admitted_native_stop_pin(shared, state).await;
        adopted
    } else {
        false
    };
    spawn(
        &http,
        shared,
        provider,
        channel_id,
        logical,
        output_path,
        (active, finish_mailbox, binding.relay_last_offset()),
    )
    .await
}

/// Recover the exact first prompt boundary of a Claude prelaunch episode before taking its lease.
fn recover_pathless(
    id: i64,
    record: &HostedExecution,
    state: &inflight::InflightTurnState,
) -> Option<inflight::InflightTurnState> {
    use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
    use crate::services::discord::tui_prompt_relay::herdr_source::nonce_baseline;
    use crate::services::session_host::{HostKind, HostedRuntimeLocator};
    use crate::services::tui_input::human_frame::history::v2::carried::{self, Consumed};
    use std::os::unix::fs::MetadataExt;

    let logical = &record.owner.logical_key;
    let nonce = &record.execution_nonce;
    if record.owner.provider != "claude"
        || state.provider != "claude"
        || state.runtime_kind != Some(ClaudeTui)
        || state.tmux_session_name.as_ref() != Some(logical)
        || state.hosted_record_id.is_some_and(|other| other != id)
        || state
            .hosted_execution_nonce
            .as_ref()
            .is_some_and(|other| other != nonce)
        || state.turn_nonce.as_deref().is_none_or(str::is_empty)
        || state.tui_terminal_kind.is_some()
        || state.terminal_receipt.is_some()
        || state.terminal_delivery_committed
        || state.herdr_before_start_stop.is_some()
        || state.rebind_origin
    {
        return None;
    }
    let at = record.location.as_ref()?;
    let epoch = crate::services::herdr_launch::clear_epoch::snapshot(
        &record.owner.provider,
        &record.owner.discord_token_hash,
        state.channel_id,
    );
    let source = nonce_baseline(state.channel_id, logical, nonce)?;
    let binding = runtime_binding_for_tmux_session(logical)?;
    let metadata = source.path.symlink_metadata().ok()?;
    if binding.runtime_kind != ClaudeTui
        || binding.session_id.as_ref() != Some(&source.session_id)
        || std::path::Path::new(binding.relay_output_path()) != source.path
        || state
            .session_id
            .as_ref()
            .is_some_and(|previous| previous != &source.session_id)
        || !metadata.is_file()
        || (metadata.dev(), metadata.ino()) != (source.dev, source.ino)
    {
        return None;
    }
    let target = serde_json::json!({"host":"herdr","owner":logical,"root":at.socket_addr,
        "execution_nonce":nonce});
    let receipt = carried::lookup_herdr(
        (
            state.user_msg_id,
            &format!("claude/{}", record.owner.discord_token_hash),
        ),
        &target,
        &source.session_id,
        &source.path,
        metadata.len(),
    )?;
    let Consumed::User { row } = &receipt else {
        return None;
    };
    if row.file != [source.dev, source.ino] {
        return None;
    }
    let mut guard = inflight::lock_inflight_episode(
        &ProviderKind::Claude,
        state.channel_id,
        &inflight::InflightEpisodePin::from_state(state),
    )
    .ok()?;
    let mut updated = guard.state().clone();
    if updated.save_generation != state.save_generation {
        return None;
    }
    updated.session_id = Some(source.session_id.clone());
    updated.output_path = Some(binding.relay_output_path().to_owned());
    updated.turn_start_offset = Some(row.range.0);
    updated.last_offset = updated.last_offset.max(row.range.0);
    updated.hosted_record_id = Some(id);
    updated.hosted_execution_nonce = Some(nonce.clone());
    match &updated.host_locator {
        Some(inflight::PersistedHostLocator::Known(locator))
            if locator.host_kind == HostKind::Herdr
                && locator.host_session_id == at.named_session
                && locator
                    .execution_node
                    .as_ref()
                    .is_none_or(|node| node == &at.execution_node)
                && (state.hosted_execution_nonce.is_some()
                    || locator.pane.as_ref() == Some(&at.pane_id)) => {}
        Some(_) => return None,
        None => {
            updated.host_locator = Some(inflight::PersistedHostLocator::Known(
                HostedRuntimeLocator {
                    execution_node: Some(at.execution_node.clone()),
                    host_kind: HostKind::Herdr,
                    host_session_id: at.named_session.clone(),
                    pane: Some(at.pane_id.clone()),
                },
            ));
        }
    }
    if nonce_baseline(state.channel_id, logical, nonce).as_ref() != Some(&source) {
        return None;
    }
    let permit = epoch.begin_effect()?;
    let saved = receipt.confirm(|| {
        (guard.persist_verified_under_guard(&updated, "herdr_pathless_continue")
            == inflight::GuardedSaveOutcome::Saved)
            .then(|| guard.state().clone())
    });
    drop(permit);
    saved
}

pub(super) async fn spawn(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &str,
    (state, finish_mailbox_on_completion, idle_offset): (
        Option<&inflight::InflightTurnState>,
        bool,
        u64,
    ),
) -> bool {
    if std::fs::metadata(output_path).is_err() {
        return false;
    }
    let (initial_offset, current_len, truncated) = match state {
        Some(state) => recovery_watcher_start_offset_for_state(output_path, state),
        None => super::super::terminal_watcher::recovery_watcher_start_offset(
            output_path,
            idle_offset,
            None,
        ),
    };
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let resume_offset = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
    let pause_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let turn_delivered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let last_heartbeat_ts_ms = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
        super::tmux_watcher_now_ms(),
    ));
    let handle = TmuxWatcherHandle {
        tmux_session_name: tmux_session_name.to_owned(),
        output_path: output_path.to_owned(),
        paused: paused.clone(),
        resume_offset: resume_offset.clone(),
        cancel: cancel.clone(),
        pause_epoch: pause_epoch.clone(),
        turn_delivered: turn_delivered.clone(),
        last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
    };
    let host =
        super::tmux::watch_host_of(shared, provider, channel_id.get(), tmux_session_name).await;
    let parent = state.and_then(|state| {
        super::tmux::thread_follow_up_parent_channel_id(
            channel_id,
            state.logical_channel_id,
            state.thread_id,
        )
    });
    let Ok(claim) = super::tmux::claim_or_reuse_watcher_for_host(
        &shared.tmux_watchers,
        channel_id,
        handle,
        provider,
        "inflight_recovery",
        parent,
        host,
    ) else {
        return false;
    };
    if !claim.should_spawn() {
        return true;
    }
    if state.is_some() {
        shared
            .restart
            .recovering_channels
            .insert(channel_id, std::time::Instant::now());
    }
    let ts2 = chrono::Local::now().format("%H:%M:%S");
    if truncated {
        let saved = state.map_or(idle_offset, |state| state.last_offset);
        tracing::info!(
            "  [{ts2}] ↻ recovery: output truncated for #{tmux_session_name} (saved offset {saved}, file len {current_len}), restarting watcher from 0",
        );
    }
    tracing::info!(
        "  [{ts2}] 👁 recovery: spawned watcher for #{tmux_session_name} at offset {initial_offset}"
    );
    let restored_turn = state.and_then(|state| {
        super::tmux::restored_watcher_turn_from_inflight(
            state,
            tmux_session_name,
            finish_mailbox_on_completion,
        )
    });
    shared.record_tmux_watcher_reconnect(channel_id);
    super::task_supervisor::spawn_observed_tmux_watcher(
        "recovery_restore_inflight_tmux_output_watcher_with_restore",
        shared.clone(),
        tmux_session_name.to_owned(),
        cancel.clone(),
        super::tmux::tmux_output_watcher_with_restore(
            channel_id,
            http.clone(),
            shared.clone(),
            output_path.to_owned(),
            tmux_session_name.to_owned(),
            initial_offset,
            cancel,
            paused,
            resume_offset,
            pause_epoch,
            turn_delivered,
            last_heartbeat_ts_ms,
            restored_turn,
        ),
    );
    true
}
