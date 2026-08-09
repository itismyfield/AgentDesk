use super::*;

pub(in crate::services::discord) struct RecoveryWatcherReservation {
    pub(super) should_spawn: bool,
    pub(super) requested_channel_id: ChannelId,
    pub(in crate::services::discord) handle: TmuxWatcherHandle,
    shared: Arc<SharedData>,
    adoption: Option<super::runtime::ReadoptionAdoption>,
    armed: bool,
}

fn consumer_reserved(
    should_spawn: bool,
    owner_channel_id: ChannelId,
    requested_channel_id: ChannelId,
) -> bool {
    should_spawn || owner_channel_id == requested_channel_id
}

fn require_consumer_reservation(
    should_spawn: bool,
    owner_channel_id: ChannelId,
    requested_channel_id: ChannelId,
) -> Option<ChannelId> {
    if !consumer_reserved(should_spawn, owner_channel_id, requested_channel_id) {
        return None;
    }
    Some(owner_channel_id)
}

impl RecoveryWatcherReservation {
    pub(in crate::services::discord) fn from_claim(
        shared: &Arc<SharedData>,
        provider: &ProviderKind,
        state: Option<&inflight::InflightTurnState>,
        requested_channel_id: ChannelId,
        candidate: TmuxWatcherHandle,
        should_spawn: bool,
        owner_channel_id: ChannelId,
    ) -> Option<Self> {
        let Some(owner_channel_id) =
            require_consumer_reservation(should_spawn, owner_channel_id, requested_channel_id)
        else {
            if let Some(state) = state {
                super::runtime::observe_readoption_adoption_abandon(
                    provider,
                    state,
                    "cross_channel_reuse",
                    None,
                );
            }
            return None;
        };
        let guard = lock_tmux_watcher_registry();
        let expected_cancel = should_spawn.then_some(&candidate.cancel);
        let proof_handle = shared.tmux_watchers.current_channel_handle_locked(
            &guard,
            &owner_channel_id,
            &candidate.tmux_session_name,
            &candidate.output_path,
            expected_cancel,
        );
        drop(guard);
        let Some(proof_handle) = proof_handle else {
            if should_spawn {
                shared.tmux_watchers.cancel_and_remove_channel_if_current(
                    &requested_channel_id,
                    &candidate.tmux_session_name,
                    &candidate.output_path,
                    &candidate.cancel,
                );
            }
            return None;
        };
        Some(Self {
            should_spawn,
            requested_channel_id,
            handle: proof_handle,
            shared: shared.clone(),
            adoption: None,
            armed: true,
        })
    }

    fn rollback_registry_if_minted(&self) -> bool {
        self.should_spawn
            && self
                .shared
                .tmux_watchers
                .cancel_and_remove_channel_if_current(
                    &self.requested_channel_id,
                    &self.handle.tmux_session_name,
                    &self.handle.output_path,
                    &self.handle.cancel,
                )
    }

    fn current_under_guard(&self, guard: &TmuxWatcherRegistryGuard) -> bool {
        self.shared
            .tmux_watchers
            .current_channel_handle_locked(
                guard,
                &self.requested_channel_id,
                &self.handle.tmux_session_name,
                &self.handle.output_path,
                Some(&self.handle.cancel),
            )
            .is_some()
    }

    fn attach_adoption(&mut self, adoption: super::runtime::ReadoptionAdoption) {
        self.adoption = Some(adoption);
    }

    pub(in crate::services::discord) async fn abandon(
        mut self,
        reason: &'static str,
        marker_outcome: Option<inflight::GuardedSaveOutcome>,
    ) -> bool {
        let rolled_back = self.rollback_registry_if_minted();
        self.armed = false;
        if let Some(adoption) = self.adoption.take() {
            let shared = self.shared.clone();
            let cleanup = tokio::spawn(async move {
                super::runtime::abandon_readoption_adoption(
                    &shared,
                    adoption,
                    reason,
                    marker_outcome,
                )
                .await;
            });
            let _ = cleanup.await;
        }
        rolled_back
    }

    pub(in crate::services::discord) fn disarm(mut self) -> (bool, TmuxWatcherHandle) {
        self.armed = false;
        self.adoption.take();
        (self.should_spawn, self.handle.clone())
    }
}

impl Drop for RecoveryWatcherReservation {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.rollback_registry_if_minted();
        let Some(adoption) = self.adoption.take() else {
            return;
        };
        let shared = self.shared.clone();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                super::runtime::abandon_readoption_adoption(
                    &shared,
                    adoption,
                    "reservation_dropped",
                    None,
                )
                .await;
            });
        }
    }
}

pub(super) fn reserve_recovery_watcher(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    state: &inflight::InflightTurnState,
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
        RecoveryWatcherReservation::from_claim(
            shared,
            provider,
            Some(state),
            channel_id,
            task_handle,
            outcome.should_spawn(),
            outcome.owner_channel_id(),
        )
    }
}

pub(in crate::services::discord) async fn begin_and_commit_reserved_watcher(
    shared: &Arc<SharedData>,
    state: &inflight::InflightTurnState,
    mut reservation: RecoveryWatcherReservation,
    episode_scoped_ledger: bool,
    locked_episode: Option<&mut inflight::LockedInflightEpisode>,
) -> Result<Option<(RecoveryWatcherReservation, bool)>, inflight::GuardedSaveOutcome> {
    let Some(adoption) =
        super::runtime::begin_readoption_adoption(shared, state, episode_scoped_ledger).await
    else {
        return Ok(None);
    };
    reservation.attach_adoption(adoption);

    commit_attached_reserved_watcher(shared, reservation, locked_episode).await
}

async fn commit_attached_reserved_watcher(
    shared: &Arc<SharedData>,
    reservation: RecoveryWatcherReservation,
    locked_episode: Option<&mut inflight::LockedInflightEpisode>,
) -> Result<Option<(RecoveryWatcherReservation, bool)>, inflight::GuardedSaveOutcome> {
    let commit = {
        let registry_guard = lock_tmux_watcher_registry();
        if reservation.current_under_guard(&registry_guard) {
            Some(super::runtime::commit_readoption_adoption(
                shared,
                reservation.adoption.as_ref().expect("attached adoption"),
                locked_episode,
            ))
        } else {
            None
        }
    };
    let Some(commit) = commit else {
        reservation
            .abandon("watcher_reservation_replaced", None)
            .await;
        return Ok(None);
    };
    match commit {
        Ok(finish) => Ok(Some((reservation, finish))),
        Err(outcome) => {
            reservation.abandon("commit_failure", Some(outcome)).await;
            Err(outcome)
        }
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
        state,
        tmux_session_name,
        output_path,
        source,
        thread_parent,
    )?;
    begin_and_commit_reserved_watcher(shared, state, reservation, true, Some(&mut locked_episode))
        .await
        .ok()
        .flatten()
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
        reservation.disarm();
        return;
    }
    let handle = reservation.handle.clone();
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
    reservation.disarm();
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn state(channel_id: u64) -> inflight::InflightTurnState {
        let mut state = inflight::InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            Some(format!("reservation-{channel_id}")),
            343_742_347_365_974_026,
            channel_id + 10,
            channel_id + 11,
            "watcher reservation lifecycle".to_string(),
            Some("provider-session".to_string()),
            Some(format!("AgentDesk-claude-{channel_id}")),
            Some(format!("/tmp/reservation-{channel_id}.jsonl")),
            None,
            0,
        );
        state.set_restart_mode(InflightRestartMode::DrainRestart);
        state
    }

    fn handle(session: &str, output_path: &str) -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            tmux_session_name: session.to_string(),
            output_path: output_path.to_string(),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(
                super::super::tmux_watcher_now_ms(),
            )),
        }
    }

    #[test]
    fn cross_channel_reuse_is_not_a_reservation_for_the_requester() {
        let owner = ChannelId::new(52_421);
        let requested = ChannelId::new(52_422);
        assert!(!consumer_reserved(false, owner, requested));
        assert_eq!(require_consumer_reservation(false, owner, requested), None);
        assert!(consumer_reserved(false, owner, owner));
        assert_eq!(
            require_consumer_reservation(false, owner, owner),
            Some(owner)
        );
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
        let commit = helper[reserve..]
            .find("begin_and_commit_reserved_watcher")
            .map(|offset| reserve + offset)
            .expect("BEGIN/COMMIT");
        assert!(reserve < commit);

        let restore = include_str!("../watchers/lifecycle/restore.rs");
        let pending = restore.find("// Spawn watchers").expect("E3 pending loop");
        let claim = restore[pending..]
            .find("try_claim_watcher_with_thread_parent")
            .expect("E3 RESERVE");
        let begin = restore[pending..]
            .find("begin_and_commit_reserved_watcher")
            .expect("E3 BEGIN/COMMIT guard");
        let spawn = restore[pending..]
            .find("spawn_observed_tmux_watcher")
            .expect("E3 SPAWN");
        assert!(claim < begin && begin < spawn);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn replacement_before_commit_abandons_without_clobbering_successor() {
        let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let root = tempfile::tempdir().expect("runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            root.path(),
        );
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let state = state(524_231);
        inflight::save_inflight_state(&state).expect("planned restart row");
        let channel = ChannelId::new(state.channel_id);
        let session = state.tmux_session_name.as_deref().expect("session");
        let output = state.output_path.as_deref().expect("output");
        let mut reservation = reserve_recovery_watcher(
            &shared,
            &ProviderKind::Claude,
            channel,
            &state,
            session,
            output.to_string(),
            "reservation_replacement_test",
            None,
        )
        .expect("initial reservation");
        let adoption = super::runtime::begin_readoption_adoption(&shared, &state, true)
            .await
            .expect("BEGIN");
        reservation.attach_adoption(adoption);

        let successor_channel = ChannelId::new(state.channel_id + 1);
        let successor = handle(session, output);
        let successor_cancel = successor.cancel.clone();
        let outcome = super::tmux::claim_or_replace_watcher(
            &shared.tmux_watchers,
            successor_channel,
            successor,
            &ProviderKind::Claude,
            "reservation_replacement_test_successor",
        );
        assert!(outcome.should_spawn());
        assert_eq!(outcome.owner_channel_id(), successor_channel);

        let mut episode = super::runtime::lock_readoption_episode(&ProviderKind::Claude, &state)
            .await
            .expect("episode lock");
        assert!(
            commit_attached_reserved_watcher(&shared, reservation, Some(&mut episode))
                .await
                .expect("replacement is a controlled refusal")
                .is_none()
        );
        drop(episode);

        let binding = shared
            .tmux_watchers
            .channel_binding(&successor_channel)
            .expect("successor binding survives stale rollback");
        assert_eq!(binding.tmux_session_name, session);
        assert!(!successor_cancel.load(std::sync::atomic::Ordering::Relaxed));
        let persisted = inflight::load_inflight_state(&ProviderKind::Claude, state.channel_id)
            .expect("abandoned durable row");
        assert!(!persisted.readopted_from_inflight);
        assert_eq!(
            persisted.restart_mode,
            Some(InflightRestartMode::DrainRestart)
        );
        assert!(
            super::mailbox_snapshot(&shared, channel)
                .await
                .cancel_token
                .is_none(),
            "replacement refusal must ABANDON the minted mailbox claim"
        );
    }

    #[test]
    fn dropping_reservation_rolls_back_only_its_exact_registry_handle() {
        let shared = super::super::make_shared_data_for_tests_with_storage(None);
        let state = state(524_241);
        let channel = ChannelId::new(state.channel_id);
        let reservation = reserve_recovery_watcher(
            &shared,
            &ProviderKind::Claude,
            channel,
            &state,
            state.tmux_session_name.as_deref().expect("session"),
            state.output_path.clone().expect("output"),
            "reservation_drop_test",
            None,
        )
        .expect("reservation");
        let cancel = reservation.handle.cancel.clone();
        assert!(shared.tmux_watchers.contains_key(&channel));

        drop(reservation);

        assert!(!shared.tmux_watchers.contains_key(&channel));
        assert!(cancel.load(std::sync::atomic::Ordering::Relaxed));
    }
}
