use super::*;

pub(super) struct StaleResumeExitContext<'a> {
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) http: &'a Arc<serenity::Http>,
    pub(super) watcher_provider: &'a ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) tmux_session_name: &'a str,
    pub(super) current_offset: u64,
    pub(super) placeholder_msg_id: Option<serenity::MessageId>,
}

pub(super) async fn handle_stale_resume_exit(
    context: StaleResumeExitContext<'_>,
    stale_resume_detected: bool,
    full_response: &mut String,
) {
    if !stale_resume_detected {
        return;
    }
    let StaleResumeExitContext {
        shared,
        http,
        watcher_provider,
        channel_id,
        tmux_session_name,
        current_offset,
        placeholder_msg_id,
    } = context;

    clear_provider_overload_retry_state(channel_id);
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::warn!(
        "  [{ts}] ⚠ Watcher detected stale session resume failure (channel {}), clearing session_id",
        channel_id
    );
    let stale_sid = {
        let mut data = shared.core.lock().await;
        let old = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.session_id.clone());
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
        }
        old
    };
    let hostname = crate::services::platform::hostname_short();
    let session_key = format!("{}:{}", hostname, tmux_session_name);
    crate::services::discord::adk_session::clear_provider_session_id(&session_key, shared.api_port)
        .await;
    if let Some(ref session_id) = stale_sid {
        let _ = crate::services::discord::internal_api::clear_stale_session_id(session_id).await;
    }

    if watcher_session_is_main_orchestration(tmux_session_name, channel_id) {
        tracing::error!(
            tmux_session = %tmux_session_name,
            channel_id = channel_id.get(),
            current_offset,
            decision_reason = "stale_resume_retry",
            "watcher blocked automatic stale-resume kill of main orchestration session"
        );
    } else {
        write_watcher_forced_kill_log(
            shared,
            channel_id,
            tmux_session_name,
            current_offset,
            "stale_resume_retry",
        );
        crate::services::termination_audit::record_termination_for_tmux(
            tmux_session_name,
            None,
            "tmux_watcher",
            "stale_resume_retry",
            Some("stale session resume detected — forcing fresh session before auto-retry"),
            None,
        );
        record_tmux_exit_reason(
            tmux_session_name,
            "stale session resume detected — forcing fresh session before auto-retry",
        );
        crate::services::platform::tmux::kill_session(
            tmux_session_name,
            "stale session resume detected — forcing fresh session before auto-retry",
        );
    }

    if let Some(message_id) = placeholder_msg_id {
        let _ = crate::services::discord::http::edit_channel_message(
            http,
            channel_id,
            message_id,
            "↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다.",
        )
        .await;
    }
    match crate::services::discord::inflight::load_inflight_state(
        watcher_provider,
        channel_id.get(),
    ) {
        Some(state) if state.rebind_origin || state.user_msg_id == 0 => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Watcher auto-retry skipped for channel {} — inflight has no user message to retry",
                channel_id
            );
        }
        Some(state) => {
            crate::services::discord::tmux_overload_retry::schedule_discord_retry_with_history_completion_release(
                Arc::clone(shared),
                Arc::clone(http),
                watcher_provider.clone(),
                channel_id,
                serenity::MessageId::new(state.user_msg_id),
                state.user_text,
            );
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ↻ Watcher auto-retry queued for channel {}",
                channel_id
            );
        }
        None => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Watcher auto-retry skipped: inflight state missing for channel {}",
                channel_id
            );
        }
    }
    full_response.clear();
}
