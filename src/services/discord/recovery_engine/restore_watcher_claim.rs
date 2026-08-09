use super::*;

pub(super) struct RecoveryWatcherReservation {
    pub(super) consumer_reserved: bool,
    pub(super) should_spawn: bool,
    pub(super) requested_channel_id: ChannelId,
    pub(super) handle: TmuxWatcherHandle,
}

fn consumer_reserved(
    should_spawn: bool,
    owner_channel_id: ChannelId,
    requested_channel_id: ChannelId,
) -> bool {
    should_spawn || owner_channel_id == requested_channel_id
}

impl RecoveryWatcherReservation {
    pub(super) fn rollback_if_minted(&self, watchers: &TmuxWatcherRegistry) -> bool {
        self.should_spawn
            && watchers.cancel_and_remove_channel_if_current(
                &self.requested_channel_id,
                &self.handle.tmux_session_name,
                &self.handle.output_path,
                &self.handle.cancel,
            )
    }
}

pub(super) fn reserve_recovery_watcher(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: String,
    source: &'static str,
    thread_parent: Option<super::tmux::ThreadFollowUpParent>,
) -> Option<RecoveryWatcherReservation> {
    let handle = TmuxWatcherHandle {
        tmux_session_name: tmux_session_name.to_string(),
        output_path: output_path.clone(),
        paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        resume_offset: Arc::new(std::sync::Mutex::new(None)),
        cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
            super::tmux_watcher_now_ms(),
        )),
    };
    let task_handle = handle.clone();

    #[cfg(unix)]
    let outcome = super::tmux::claim_or_reuse_watcher_with_thread_parent(
        &shared.tmux_watchers,
        channel_id,
        handle,
        provider,
        source,
        thread_parent,
    );
    #[cfg(not(unix))]
    {
        let _ = (shared, provider, channel_id, source, thread_parent, handle);
        return None;
    }

    #[cfg(unix)]
    {
        let should_spawn = outcome.should_spawn();
        let owner_channel_id = outcome.owner_channel_id();
        Some(RecoveryWatcherReservation {
            consumer_reserved: consumer_reserved(should_spawn, owner_channel_id, channel_id),
            should_spawn,
            requested_channel_id: channel_id,
            handle: task_handle,
        })
    }
}

pub(super) async fn reserve_and_commit_recovery_watcher(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    state: &inflight::InflightTurnState,
    tmux_session_name: &str,
    output_path: String,
    source: &'static str,
    thread_parent: Option<super::tmux::ThreadFollowUpParent>,
) -> Option<(RecoveryWatcherReservation, bool)> {
    let mut locked_episode = match super::runtime::lock_readoption_episode(provider, state).await {
        Ok(guard) => guard,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                error = ?error,
                "recovery readoption skipped: exact episode lock unavailable"
            );
            return None;
        }
    };
    let reservation = reserve_recovery_watcher(
        shared,
        provider,
        channel_id,
        tmux_session_name,
        output_path,
        source,
        thread_parent,
    )?;
    if !reservation.consumer_reserved {
        super::runtime::observe_readoption_adoption_abandon(
            provider,
            state,
            "cross_channel_reuse",
            None,
        );
        return None;
    }
    match super::runtime::begin_and_commit_readoption_adoption(
        shared,
        state,
        true,
        Some(&mut locked_episode),
    )
    .await
    {
        Ok(Some(finish)) => Some((reservation, finish)),
        Ok(None) | Err(_) => {
            reservation.rollback_if_minted(&shared.tmux_watchers);
            None
        }
    }
}

pub(super) fn spawn_committed_recovery_watcher(
    task_name: &'static str,
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    state: &inflight::InflightTurnState,
    reservation: RecoveryWatcherReservation,
    initial_offset: u64,
    finish_mailbox_on_completion: bool,
) {
    if !reservation.should_spawn {
        return;
    }
    let handle = reservation.handle;
    let restored_turn = super::tmux::restored_watcher_turn_from_inflight(
        state,
        &handle.tmux_session_name,
        finish_mailbox_on_completion,
    );
    shared.record_tmux_watcher_reconnect(channel_id);
    super::task_supervisor::spawn_observed_tmux_watcher(
        task_name,
        shared.clone(),
        handle.tmux_session_name.clone(),
        handle.cancel.clone(),
        super::tmux::tmux_output_watcher_with_restore(
            channel_id,
            http.clone(),
            shared.clone(),
            handle.output_path,
            handle.tmux_session_name,
            initial_offset,
            handle.cancel,
            handle.paused,
            handle.resume_offset,
            handle.pause_epoch,
            handle.turn_delivered,
            handle.last_heartbeat_ts_ms,
            restored_turn,
        ),
    );
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn cross_channel_reuse_is_not_a_reservation_for_the_requester() {
        let owner = ChannelId::new(52_421);
        let requested = ChannelId::new(52_422);
        assert!(!consumer_reserved(false, owner, requested));
        assert!(consumer_reserved(false, owner, owner));
    }

    #[test]
    fn watcher_reservation_lexically_precedes_begin_commit_and_spawn() {
        let helper = include_str!("restore_watcher_claim.rs");
        let orchestration = helper
            .find("pub(super) async fn reserve_and_commit_recovery_watcher")
            .expect("shared recovery orchestration");
        let reserve = helper[orchestration..]
            .find("reserve_recovery_watcher(")
            .map(|offset| orchestration + offset)
            .expect("RESERVE");
        let receipt = helper[reserve..]
            .find("if !reservation.consumer_reserved")
            .map(|offset| reserve + offset)
            .expect("RESERVE receipt check");
        let commit = helper[reserve..]
            .find("begin_and_commit_readoption_adoption")
            .map(|offset| reserve + offset)
            .expect("BEGIN/COMMIT");
        assert!(reserve < receipt && receipt < commit);

        let restore = include_str!("../watchers/lifecycle/restore.rs");
        let pending = restore.find("// Spawn watchers").expect("E3 pending loop");
        let claim = restore[pending..]
            .find("try_claim_watcher_with_thread_parent")
            .expect("E3 RESERVE");
        let begin = restore[pending..]
            .find("begin_readoption_adoption")
            .expect("E3 BEGIN");
        let commit = restore[pending..]
            .find("commit_readoption_adoption")
            .expect("E3 COMMIT");
        let spawn = restore[pending..]
            .find("spawn_observed_tmux_watcher")
            .expect("E3 SPAWN");
        assert!(claim < begin && begin < commit && commit < spawn);
    }
}
