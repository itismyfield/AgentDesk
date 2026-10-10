use super::*;

pub(in crate::services::discord) async fn reset_channel_provider_state(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    reset_source: &str,
    reset_provider_state: bool,
    clear_history: bool,
    recreate_tmux: bool,
) -> ManagedReset {
    #[cfg(test)]
    if home_fence::mutant("sink_admission_removed") {
        return reset_body(
            http,
            shared,
            provider,
            channel_id,
            reset_source,
            reset_provider_state,
            clear_history,
            recreate_tmux,
        )
        .await;
    }
    let permit = match home_fence::admit(channel_id, provider.as_str()) {
        Ok(permit) => permit,
        Err(reason) => return ManagedReset::Refused(reason.0.to_string()),
    };
    crate::services::cluster::channel_home::command_scope(
        permit,
        reset_body(
            http,
            shared,
            provider,
            channel_id,
            reset_source,
            reset_provider_state,
            clear_history,
            recreate_tmux,
        ),
    )
    .await
}

pub(in crate::services::discord) async fn reset_channel_provider_state_for_home_drain(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    reset_source: &str,
    reset_provider_state: bool,
    clear_history: bool,
    recreate_tmux: bool,
) -> ManagedReset {
    #[cfg(test)]
    if home_fence::mutant("drain_reset_uses_admitted_sink") {
        return reset_channel_provider_state(
            http,
            shared,
            provider,
            channel_id,
            reset_source,
            reset_provider_state,
            clear_history,
            recreate_tmux,
        )
        .await;
    }
    reset_body(
        http,
        shared,
        provider,
        channel_id,
        reset_source,
        reset_provider_state,
        clear_history,
        recreate_tmux,
    )
    .await
}

async fn reset_body(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    reset_source: &str,
    reset_provider_state: bool,
    clear_history: bool,
    recreate_tmux: bool,
) -> ManagedReset {
    if let Some(reason) = verified_codex_reset_refusal(shared, provider, channel_id).await {
        return ManagedReset::Refused(reason.to_owned());
    }
    let refusal = crate::services::discord::admin_host_guard::managed_reset_refusal;
    let (reset, recreate) = (reset_provider_state, recreate_tmux);
    if let Some(reason) = refusal(shared, provider, channel_id, reset, recreate, None).await {
        return ManagedReset::Refused(reason);
    }
    #[cfg(test)]
    home_fence::pause("reset").await;
    crate::services::discord::turn_presence::entrypoints::withdraw(channel_id.get(), "reset");
    let tmux_name = {
        let mut data = shared.core.lock().await;
        data.sessions.get_mut(&channel_id).and_then(|session| {
            if reset_provider_state {
                session.session_id = None;
                session.clear_provider_session();
            }
            if clear_history {
                session.history.clear();
            }
            session
                .channel_name
                .as_ref()
                .map(|channel_name| provider.build_tmux_session_name(channel_name))
        })
    };

    if reset_provider_state
        && let Some(session_key) =
            resolve_session_key_for_clear(http, shared, channel_id, provider).await
    {
        crate::services::discord::adk_session::clear_provider_session_id(
            &session_key,
            shared.api_port,
        )
        .await;
    }

    if let Some(name) = tmux_name.as_deref() {
        if reset_provider_state {
            match managed_session_reset_behavior(provider) {
                ManagedSessionResetBehavior::ResetManagedProcess => {
                    reset_managed_process_session(name);
                }
                ManagedSessionResetBehavior::Noop => {}
            }
        }
        if recreate_tmux {
            crate::services::discord::commands::tmux_recreate::recreate_channel_tmux(
                shared,
                provider,
                channel_id,
                name,
                reset_source,
            )
            .await;
        }
    }

    ManagedReset::Applied(tmux_name)
}
