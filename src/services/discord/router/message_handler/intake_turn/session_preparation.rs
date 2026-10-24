//! Session restore keeps the original policy and side-effect order.
use super::*;

pub(in crate::services::discord) struct Restore<'a> {
    pub session_id: &'a mut Option<String>,
    pub memento_context_loaded: &'a mut bool,
    pub reason: &'a mut &'static str,
    pub force_fresh: bool,
    pub cleared: bool,
    pub reset: bool,
    pub recreate: bool,
    pub session_key: Option<&'a str>,
    pub tmux: Option<&'a str>,
}

pub(in crate::services::discord) async fn restore(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    state: Restore<'_>,
) {
    if state.session_id.is_none() {
        if state.force_fresh {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for channel {} due to /goal fresh session request",
                channel_id.get()
            );
        } else if state.cleared {
            *state.reason = "session_cleared_by_user";
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for channel {} due to prior /clear",
                channel_id.get()
            );
        } else if state.reset || state.recreate {
            *state.reason = dispatch_reset_lifecycle_code(state.reset, state.recreate);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for dispatch reset_provider_state={} recreate_tmux={}",
                state.reset,
                state.recreate
            );
        } else if let Some(key) = state.session_key {
            let restored = super::super::super::super::adk_session::fetch_provider_session_id(
                key,
                provider,
                shared.api_port,
            )
            .await;
            if let Some(restored_session_id) = restored {
                *state.reason = "db_provider_session_restored";
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ Restored provider session_id from DB for {}",
                    key
                );
                let mut data = shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.restore_provider_session(Some(restored_session_id.clone()));
                    *state.memento_context_loaded = session.memento_context_loaded;
                }
                *state.session_id = Some(restored_session_id);
            } else if let Some((recovered, recovered_memento_context_loaded)) =
                restore_live_tui_provider_session_from_binding(
                    shared,
                    channel_id,
                    provider,
                    state.tmux,
                    Some(key),
                )
                .await
            {
                *state.reason = "live_tui_runtime_binding_restored";
                *state.memento_context_loaded = recovered_memento_context_loaded;
                *state.session_id = Some(recovered);
            } else {
                *state.reason = "no_cached_provider_session";
                *state.session_id = None;
            }
        } else {
            *state.reason = "session_key_unavailable";
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn preparation_keeps_existing_and_explicit_restore_veto_states() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        for (existing, fresh, cleared, reset, expected) in [
            (Some("existing"), false, false, false, "before"),
            (None, true, false, false, "before"),
            (None, false, true, false, "session_cleared_by_user"),
            (None, false, false, true, "dispatch_provider_reset"),
            (None, false, false, false, "session_key_unavailable"),
        ] {
            let mut session = existing.map(str::to_owned);
            let mut memento = true;
            let mut reason = "before";
            restore(
                &shared,
                &ProviderKind::Claude,
                ChannelId::new(727374),
                Restore {
                    session_id: &mut session,
                    memento_context_loaded: &mut memento,
                    reason: &mut reason,
                    force_fresh: fresh,
                    cleared,
                    reset,
                    recreate: false,
                    session_key: None,
                    tmux: None,
                },
            )
            .await;
            assert_eq!(session.as_deref(), existing);
            assert_eq!(reason, expected);
            assert!(memento);
        }
    }
}

#[cfg(test)]
pub(in crate::services::discord) async fn selected_reply(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    context: crate::services::discord::router::response_format::retry_prompt_context::RetryPromptContext,
) -> (
    crate::services::discord::router::response_format::retry_prompt_context::RetryPromptContext,
    Option<String>,
) {
    let prepared = super::retry_prompt::prepare(shared, provider, channel, context).await;
    (prepared.context, prepared.rendered)
}
