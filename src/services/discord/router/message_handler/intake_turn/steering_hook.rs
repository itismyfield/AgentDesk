// This module's body calls unix-only tmux helpers (is_tmux_available,
// tui_busy_followup_diagnostic) and its sole call site in intake_turn.rs is
// #[cfg(unix)]-gated. Gate the whole module from within so the `mod steering_hook;`
// decl stays platform-neutral (net-zero for the intake_turn.rs giant ratchet)
// while the body is excluded on Windows, avoiding E0425 there.
#![cfg(unix)]

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
    pub(super) queued_drain: bool,
    pub(super) has_dispatch: bool,
    pub(super) is_voice_announcement: bool,
    pub(super) has_pending_uploads: bool,
}

fn steering_route_is_native(
    selection: &crate::services::provider_hosting::ProviderSessionSelection,
) -> bool {
    crate::services::tui_steering::route_input_by_session_driver(selection)
        == crate::services::tui_steering::SteeringRoute::NativeTui
}

fn steering_intake_eligible(
    provider: &ProviderKind,
    foreground: bool,
    local: bool,
    wait_for_completion: bool,
    queued_drain: bool,
    has_dispatch: bool,
    is_voice_announcement: bool,
    has_pending_uploads: bool,
) -> bool {
    foreground
        && matches!(provider, ProviderKind::Claude | ProviderKind::Codex)
        && local
        && (!wait_for_completion || queued_drain)
        && !has_dispatch
        && !is_voice_announcement
        && !has_pending_uploads
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
        queued_drain,
        has_dispatch,
        is_voice_announcement,
        has_pending_uploads,
    } = context;
    if !crate::services::tui_steering::tui_steering_enabled()
        || !steering_intake_eligible(
            provider,
            foreground,
            local,
            wait_for_completion,
            queued_drain,
            has_dispatch,
            is_voice_announcement,
            has_pending_uploads,
        )
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
    if !steering_route_is_native(&selection)
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
    fn queued_drain_waiting_for_completion_remains_steering_eligible() {
        assert!(steering_intake_eligible(
            &ProviderKind::Claude,
            true,
            true,
            true,
            true,
            false,
            false,
            false,
        ));
    }

    #[test]
    fn steering_intake_eligibility_rejects_non_drain_and_other_exclusions() {
        let eligible = |foreground,
                        local,
                        wait_for_completion,
                        queued_drain,
                        has_dispatch,
                        is_voice_announcement,
                        has_pending_uploads| {
            steering_intake_eligible(
                &ProviderKind::Claude,
                foreground,
                local,
                wait_for_completion,
                queued_drain,
                has_dispatch,
                is_voice_announcement,
                has_pending_uploads,
            )
        };

        assert!(!eligible(true, true, true, false, false, false, false));
        assert!(!eligible(true, true, false, false, true, false, false));
        assert!(!eligible(true, false, false, false, false, false, false));
        assert!(!eligible(true, true, false, false, false, true, false));
        assert!(!eligible(true, true, false, false, false, false, true));
        assert!(!eligible(false, true, false, false, false, false, false));

        assert!(!steering_intake_eligible(
            &ProviderKind::Gemini,
            true,
            true,
            false,
            false,
            false,
            false,
            false,
        ));
    }

    #[test]
    fn non_native_tui_driver_is_blocked_by_the_actual_hook_route_gate() {
        let selection = crate::services::provider_hosting::ProviderSessionSelection {
            provider_id: "claude".to_string(),
            requested_tui_hosting: false,
            driver: crate::services::provider_hosting::ProviderSessionDriver::ClaudeE,
            fallback_reason: None,
        };
        assert_eq!(
            crate::services::tui_steering::route_input_by_session_driver(&selection),
            crate::services::tui_steering::SteeringRoute::ExistingMailbox,
        );

        assert!(!steering_route_is_native(&selection));

        let native_selection = crate::services::provider_hosting::ProviderSessionSelection {
            provider_id: "claude".to_string(),
            requested_tui_hosting: true,
            driver: crate::services::provider_hosting::ProviderSessionDriver::TuiHosting,
            fallback_reason: None,
        };
        assert!(steering_route_is_native(&native_selection));
    }

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
