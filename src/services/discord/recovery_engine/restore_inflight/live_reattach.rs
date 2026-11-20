//! Reuse the inflight watcher claim for a verified live Herdr execution.
use super::*;
use crate::db::dispatched_sessions::hosted_execution::{HostedObservation, HostedRecord};
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
    matches!(
        continue_channel(shared, provider, channel, &transition).await,
        Some(VerifiedReattach::Live | VerifiedReattach::AwaitingClear)
    )
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
    let Ok(inflight) = inflight::load_inflight_state_read_only_result(provider, channel_id.get())
    else {
        return false;
    };
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
