use super::*;

#[cfg(all(test, unix))]
pub(in crate::services::discord::recovery_engine) struct PreparedClaim<'a> {
    registry: &'a TmuxWatcherRegistry,
    session: String,
    channel: ChannelId,
    output: String,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    gate: super::super::rebind_runtime::codex_spawn::StartGate,
    actor: Option<std::thread::JoinHandle<Result<(), String>>>,
    committed: bool,
}

#[cfg(all(test, unix))]
impl PreparedClaim<'_> {
    pub(in crate::services::discord::recovery_engine) fn is_current(&self) -> bool {
        self.registry.get(&self.channel).is_some_and(|h| {
            h.tmux_session_name == self.session
                && h.output_path == self.output
                && Arc::ptr_eq(&h.cancel, &self.cancel)
        })
    }
    pub(in crate::services::discord::recovery_engine) fn open(&mut self) -> Result<(), String> {
        let _lock = crate::services::discord::tmux_watcher_registry::lock_tmux_watcher_registry();
        if !self.is_current() {
            return Err("prepared watcher was superseded".into());
        }
        self.committed = true;
        self.gate.finish(true);
        Ok(())
    }
    pub(in crate::services::discord::recovery_engine) fn stop(&self) {
        self.registry.cancel_and_remove_channel_if_current(
            &self.channel,
            &self.session,
            &self.output,
            &self.cancel,
        );
    }
    pub(in crate::services::discord::recovery_engine) fn join(mut self) -> Result<(), String> {
        self.actor
            .take()
            .ok_or("watcher handle missing")?
            .join()
            .map_err(|_| "watcher panicked")?
    }
}

#[cfg(all(test, unix))]
impl Drop for PreparedClaim<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.gate.finish(false);
            self.registry.cancel_and_remove_channel_if_current(
                &self.channel,
                &self.session,
                &self.output,
                &self.cancel,
            );
            if let Some(actor) = self.actor.take() {
                let _ = actor.join();
            }
        }
    }
}

#[cfg(all(test, unix))]
pub(in crate::services::discord::recovery_engine) fn claim_verified<'a>(
    registry: &'a TmuxWatcherRegistry,
    channel: ChannelId,
    handle: TmuxWatcherHandle,
    expected: Option<(&str, &str, &Arc<std::sync::atomic::AtomicBool>)>,
    gate: super::super::rebind_runtime::codex_spawn::StartGate,
    work: impl FnOnce() -> Result<(), String> + Send + 'static,
) -> Result<PreparedClaim<'a>, String> {
    let session = handle.tmux_session_name.clone();
    let output = handle.output_path.clone();
    let cancel = handle.cancel.clone();
    let actor_gate = gate.clone();
    let permit = crate::services::discord::input_runtime::fence::effect::current();
    let actor = std::thread::Builder::new()
        .name("verified_replay_watcher".into())
        .spawn(move || {
            if actor_gate.wait() {
                crate::services::discord::input_runtime::fence::effect::synchronous(permit, work)
            } else {
                Ok(())
            }
        })
        .map_err(|e| e.to_string())?;
    let prepared = PreparedClaim {
        registry,
        session: session.clone(),
        channel,
        output,
        cancel,
        gate,
        actor: Some(actor),
        committed: false,
    };
    if let Some((expected_session, expected_output, expected_cancel)) = expected {
        if expected_session != session
            || !registry.cancel_and_remove_channel_if_current(
                &channel,
                expected_session,
                expected_output,
                expected_cancel,
            )
        {
            return Err("watcher changed during proof".into());
        }
    }
    let lock = crate::services::discord::tmux_watcher_registry::lock_tmux_watcher_registry();
    if registry
        .owner_channel_for_tmux_session(&session)
        .is_some_and(|owner| owner != channel)
        || registry.get(&channel).is_some()
    {
        return Err("watcher slot was claimed by another actor".into());
    }
    registry.insert_locked(&lock, channel, handle);
    drop(lock);
    Ok(prepared)
}

/// Claim under the single-watcher policy. Normal recovery reuses a live
/// same-session watcher; a proven crossed Codex turn forces a fresh generation.
/// `Err` is a withheld Herdr pane: nothing spawned, replaced or reused.
#[cfg(unix)]
pub(super) fn claim_rebind_watcher(
    watchers: &TmuxWatcherRegistry,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
    provider: &ProviderKind,
    crossed_codex_turn: bool,
    thread_parent: Option<super::tmux::ThreadFollowUpParent>,
    host: super::tmux::WatchHost,
) -> Result<(bool, bool), super::tmux::WatchWithheld> {
    let claim = if crossed_codex_turn {
        super::tmux::claim_or_replace_watcher_for_host(
            watchers,
            channel_id,
            handle,
            provider,
            "recovery_restore_inflight_crossed_codex_turn",
            thread_parent,
            host,
        )
    } else {
        super::tmux::claim_or_reuse_watcher_for_host(
            watchers,
            channel_id,
            handle,
            provider,
            "recovery_restore_inflight",
            thread_parent,
            host,
        )
    };
    claim.map(|claim| (claim.should_spawn(), claim.replaced_existing()))
}

/// The rebind's channel, judged before its first write: `ChannelIdZero` for a zero id, then the
/// Herdr admission below.
pub(super) async fn admit_channel(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: u64,
    tmux_session_override: &Option<String>,
) -> Result<ChannelId, RebindError> {
    let channel_id = crate::services::discord::inflight::opt_channel_id(channel_id)
        .ok_or(RebindError::ChannelIdZero)?;
    #[cfg(unix)]
    admitted(shared, provider, channel_id, tmux_session_override).await?;
    #[cfg(not(unix))]
    let _ = (shared, provider, tmux_session_override);
    Ok(channel_id)
}

/// `WatcherWithheld` when the Herdr admission would withhold the pane's claim now, read before the
/// rebind's first write so a repeat changes nothing; any other or still unnamed pane passes.
#[cfg(unix)]
async fn admitted(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_override: &Option<String>,
) -> Result<(), RebindError> {
    let name = match tmux_session_override {
        Some(name) => name.clone(),
        None => {
            let data = shared.core.lock().await;
            let session = data.sessions.get(&channel_id);
            let Some(channel_name) = session.and_then(|s| s.channel_name.clone()) else {
                return Ok(());
            };
            provider.build_tmux_session_name(&channel_name)
        }
    };
    let host = super::tmux::watch_host_of(shared, provider, channel_id.get(), &name).await;
    let herdr_host = host == super::tmux::WatchHost::Herdr;
    match crate::services::tui_prompt_dedupe::herdr_claim_admission(&name, herdr_host) {
        Ok(_) => Ok(()),
        Err(()) => Err(RebindError::WatcherWithheld { tmux_session: name }),
    }
}
