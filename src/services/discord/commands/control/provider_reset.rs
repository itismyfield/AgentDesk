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
    if let Some(reason) = verified_codex_reset_refusal(shared, provider, channel_id).await {
        return ManagedReset::Refused(reason.to_owned());
    }
    let refusal = crate::services::discord::admin_host_guard::managed_reset_refusal;
    let (reset, recreate) = (reset_provider_state, recreate_tmux);
    if let Some(reason) = refusal(shared, provider, channel_id, reset, recreate, None).await {
        return ManagedReset::Refused(reason);
    }
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
