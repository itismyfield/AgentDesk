use super::*;

pub(super) const VERIFIED_CODEX_RESET_REFUSAL: &str =
    "검증된 Codex 실행은 출력 처리가 확정될 때까지 초기화할 수 없어요. 세션과 대기열을 유지했어요";

pub(in crate::services::discord) async fn verified_codex_reset_refusal(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
) -> Option<&'static str> {
    verified_codex_reset_refusal_for_target(shared, provider, channel_id, None).await
}

pub(super) async fn refusal_for_session_key(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    key: Option<&str>,
) -> Option<&'static str> {
    let tmux = key.and_then(super::super::super::session_identity::tmux_name_from_session_key);
    verified_codex_reset_refusal_for_target(shared, provider, channel_id, tmux.as_deref()).await
}

pub(in crate::services::discord) async fn verified_codex_reset_refusal_for_target(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: serenity::ChannelId,
    approved_tmux: Option<&str>,
) -> Option<&'static str> {
    #[cfg(unix)]
    if matches!(provider, ProviderKind::Codex) {
        let tmux = {
            let data = shared.core.lock().await;
            data.sessions
                .get(&channel_id)
                .and_then(|session| session.channel_name.as_deref())
                .map(|name| provider.build_tmux_session_name(name))
        };
        let tmux = approved_tmux.or(tmux.as_deref());
        let canary = crate::services::codex_tui::canary::CANARY_TMUX;
        let canary_channel = crate::services::codex_tui::canary::CANARY_CHANNEL;
        // Hold the exact trial target before its in-progress launch can publish a pin.
        if channel_id.get() == canary_channel
            && crate::services::codex_tui::canary::enabled_for(canary)
            && (tmux == Some(canary)
                || (tmux.is_none()
                    && crate::services::tmux_common::read_tmux_channel_binding(canary)
                        == Some(canary_channel)))
        {
            return Some(VERIFIED_CODEX_RESET_REFUSAL);
        }
        // A lost session row cannot erase the incarnation's pinned source evidence.
        if tmux.is_some_and(crate::services::tui_prompt_dedupe::codex_verified_requires_proof)
            || crate::services::tui_prompt_dedupe::codex_verified_channel_requires_proof(
                channel_id.get(),
            )
        {
            return Some(VERIFIED_CODEX_RESET_REFUSAL);
        }
    }
    #[cfg(not(unix))]
    let _ = (shared, provider, channel_id, approved_tmux);
    None
}

pub(in crate::services::discord) fn reset_managed_process_session(session_name: &str) -> bool {
    #[cfg(unix)]
    if crate::services::tui_prompt_dedupe::codex_verified_requires_proof(session_name) {
        return false;
    }
    let mut reset = false;
    let lingering_pid =
        crate::services::session_backend::process_session_pid(session_name).map(|pid| pid as i32);
    if let Some(handle) = crate::services::session_backend::remove_process_session(session_name) {
        crate::services::session_backend::terminate_process_handle(handle);
        reset = true;
    } else if let Some(pid) = lingering_pid {
        if let Ok(pid) = u32::try_from(pid) {
            crate::services::process::kill_pid_tree(pid);
            reset = true;
        }
    }

    #[cfg(unix)]
    if crate::services::platform::tmux::has_session(session_name) {
        crate::services::tmux_diagnostics::record_tmux_exit_reason(
            session_name,
            "managed session reset",
        );
        if crate::services::platform::tmux::kill_session(session_name, "managed session reset") {
            crate::services::tmux_common::cleanup_session_temp_files(session_name);
            reset = true;
        }
    }

    reset
}
