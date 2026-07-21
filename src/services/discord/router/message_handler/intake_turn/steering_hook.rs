use super::*;

fn steering_injection_succeeded(outcome: &crate::services::tui_steering::SteeringOutcome) -> bool {
    matches!(
        outcome,
        crate::services::tui_steering::SteeringOutcome::Injected
    )
}

pub(super) struct IntakeSteeringContext<'a> {
    pub(super) http: &'a Arc<serenity::http::Http>,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) token: &'a str,
    pub(super) channel_id: ChannelId,
    pub(super) user_msg_id: MessageId,
    pub(super) placeholder_msg_id: MessageId,
    pub(super) provider: &'a ProviderKind,
    pub(super) provider_label: &'a str,
    pub(super) tmux_session_name: Option<&'a str>,
    pub(super) current_path: &'a str,
    pub(super) session_id: Option<&'a str>,
    pub(super) user_text: &'a str,
    pub(super) cancel_token: &'a Arc<CancelToken>,
    pub(super) intake_latency: &'a super::super::latency_spans::IntakeLatencySpans,
    pub(super) foreground: bool,
    pub(super) local: bool,
    pub(super) wait_for_completion: bool,
    pub(super) has_dispatch: bool,
    pub(super) is_voice_announcement: bool,
    pub(super) has_pending_uploads: bool,
}

pub(super) async fn maybe_handle_intake_steering(
    context: IntakeSteeringContext<'_>,
) -> Option<Result<(), Error>> {
    let IntakeSteeringContext {
        http,
        shared,
        token,
        channel_id,
        user_msg_id,
        placeholder_msg_id,
        provider,
        provider_label,
        tmux_session_name,
        current_path,
        session_id,
        user_text,
        cancel_token,
        intake_latency,
        foreground,
        local,
        wait_for_completion,
        has_dispatch,
        is_voice_announcement,
        has_pending_uploads,
    } = context;
    if !crate::services::tui_steering::tui_steering_enabled()
        || !foreground
        || !matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
        || !local
        || wait_for_completion
        || has_dispatch
        || is_voice_announcement
        || has_pending_uploads
    {
        return None;
    }
    let steering_tmux_name = tmux_session_name?;
    let selection =
        crate::services::provider_hosting::resolve_provider_session_selection_with_channel(
            provider,
            claude::is_tmux_available(),
            Some(channel_id.get()),
        );
    if crate::services::tui_steering::route_input_by_session_driver(&selection)
        != crate::services::tui_steering::SteeringRoute::NativeTui
        || !crate::services::tmux_diagnostics::tmux_session_has_live_pane(steering_tmux_name)
        || !tui_busy_followup_diagnostic(
            shared,
            provider,
            channel_id,
            Some(steering_tmux_name),
            false,
            Some(current_path),
            session_id,
        )
        .is_some_and(|diagnostic| diagnostic.transcript_turn_state.is_busy())
    {
        return None;
    }

    let steering_provider = provider.clone();
    let steering_session = steering_tmux_name.to_string();
    let steering_prompt = user_text.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        crate::services::tui_steering::inject_with_bounded_retry(
            &steering_provider,
            &selection,
            &steering_session,
            &steering_prompt,
        )
    })
    .await
    .unwrap_or_else(|error| {
        crate::services::tui_steering::SteeringOutcome::Failed(error.to_string())
    });
    let injected = steering_injection_succeeded(&outcome);
    let reaction = if injected { '🎯' } else { '⚠' };
    #[cfg(not(test))]
    let _ =
        super::super::super::super::reaction_lifecycle::try_add_reaction_raw_with_shared_detailed(
            http,
            shared,
            channel_id,
            user_msg_id,
            reaction,
        )
        .await;
    #[cfg(test)]
    let _ = reaction;
    if !injected {
        return None;
    }

    let bot_owner_provider = super::super::super::super::resolve_discord_bot_provider(token);
    let _ =
        release_mailbox_after_hosted_tui_busy_pre_submit(shared, &bot_owner_provider, channel_id)
            .await;
    let _ = channel_id.delete_message(http, placeholder_msg_id).await;
    tv_clear_current(shared, http, channel_id, user_msg_id, "intake_tui_steering").await;
    super::super::super::super::saturating_decrement_global_active(shared);
    shared.turn_start_times.remove(&channel_id);
    cancel_token
        .cancelled
        .store(true, std::sync::atomic::Ordering::Relaxed);
    super::super::super::super::clear_watchdog_deadline_override(channel_id.get()).await;
    intake_latency.log(channel_id.get(), provider_label, "tui_steered");
    Some(Ok(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tui_steering::SteeringOutcome;

    #[test]
    fn failed_or_unsafe_steering_falls_through_to_busy_followup_enqueue() {
        assert!(!steering_injection_succeeded(&SteeringOutcome::Failed(
            "submit failed".to_string()
        )));
        assert!(!steering_injection_succeeded(&SteeringOutcome::Unsafe(
            "composer changed"
        )));
        assert!(steering_injection_succeeded(&SteeringOutcome::Injected));

        let root_src = include_str!("../intake_turn.rs");
        assert!(root_src.contains("steering_hook::maybe_handle_intake_steering("));
        assert!(root_src.contains("enqueue_busy_tui_followup_for_retry("));
    }
}
