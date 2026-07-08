//! Recovery terminal delivery + visible completion cluster (#3834 decompose split).
//!
//! Behavior-preserving extraction from `recovery_engine.rs`: terminal recovery
//! relay helpers, visible completion/status-panel completion helpers, and their
//! unit tests. The root facade re-imports/re-exports the entry points so existing
//! call sites keep their `recovery_engine::...` paths. Moved verbatim except for
//! module-local visibility required by the new child-module boundary.

use super::*;

pub(super) fn should_advance_recovery_dispatch_after_relay(relay_ok: bool) -> bool {
    relay_ok
}

pub(super) async fn relay_recovery_terminal_notice(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &super::inflight::InflightTurnState,
    text: &str,
) -> RecoveryRelayOutcome {
    let recovery_context = RecoveryDeliveryContext::from_state(
        provider,
        state,
        None,
        shared.restart.current_generation,
    );
    relay_recovered_terminal_text_to_placeholder(
        http,
        shared,
        ChannelId::new(state.channel_id),
        super::inflight::optional_message_id(state.current_msg_id),
        text,
        Some(&recovery_context),
    )
    .await
}

/// Deliver the recovered terminal text to Discord: edit the placeholder in
/// place when one was anchored, else (`placeholder == None`, e.g. TUI-direct —
/// `MessageId::new(0)` would panic) send a NEW message. Only `Delivered` lets
/// callers advance recovery (Codex P1); #3293 classifies failures so a
/// permanent Discord rejection (404/403/410) can end the retry loop. #3297
/// finding 2: the anchored path flattens errors into Strings, so a transient
/// classification gets a second opinion from an active channel probe.
// #3610 PR-2: surfaced to `recovery_paths::restart` so the anchor-repost fallback
// can drive the same delivery path (send-NEW via the `placeholder == None` arm).
pub(in crate::services::discord) async fn relay_recovered_terminal_text_to_placeholder(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    placeholder: Option<MessageId>,
    text: &str,
    recovery_context: Option<&RecoveryDeliveryContext>,
) -> RecoveryRelayOutcome {
    let mut reused_recorded_anchor = false;
    let placeholder = match placeholder {
        Some(placeholder) => Some(placeholder),
        None => match recovery_context.and_then(RecoveryDeliveryContext::anchor_reuse_decision) {
            Some(terminal_text_idempotency::RecoveryAnchorReuse::DurableAlreadyDelivered(
                anchor,
            )) => {
                tracing::info!(
                    channel_id = channel_id.get(),
                    anchor_msg_id = anchor.get(),
                    "recovery no-anchor delivery: durable range already delivered; skipping Discord POST"
                );
                return RecoveryRelayOutcome::Delivered;
            }
            Some(terminal_text_idempotency::RecoveryAnchorReuse::InflightAnchor(anchor)) => {
                reused_recorded_anchor = true;
                Some(anchor)
            }
            None => None,
        },
    };
    if let Some(placeholder) = placeholder {
        footer_view_reconciler::note_footer_suppressed_for_message_takeover(
            channel_id,
            placeholder,
        );
        if reused_recorded_anchor {
            tracing::info!(
                channel_id = channel_id.get(),
                anchor_msg_id = placeholder.get(),
                "recovery no-anchor delivery: reusing recorded anchor"
            );
        }
    }
    let delivery = match placeholder {
        Some(placeholder) => {
            use super::recovery_paths::controller_cutover as cc;
            // #3089 A6a/#3998 S1-f2: anchored short-replace via the unified
            // controller; the adapter maps the verdict to `RecoveryRelayOutcome`
            // AND re-runs the #3297 probe. None / empty stay legacy.
            if cc::recovery_short_replace_should_cutover(true, text) {
                let gateway =
                    DiscordGateway::new(http.clone(), shared.clone(), ProviderKind::Claude, None);
                return cc::deliver_recovery_replace_via_controller(
                    &gateway,
                    shared,
                    &ProviderKind::Claude,
                    http,
                    channel_id,
                    placeholder,
                    text,
                    recovery_context,
                )
                .await;
            }
            terminal_text_idempotency::replace_anchored_terminal_text(
                http,
                channel_id,
                placeholder,
                text,
                shared,
                recovery_context,
            )
            .await
        }
        None => {
            return terminal_text_idempotency::relay_no_anchor_terminal_text(
                http,
                shared,
                channel_id,
                text,
                recovery_context,
            )
            .await;
        }
    };
    match delivery {
        Ok(()) => RecoveryRelayOutcome::Delivered,
        Err(error) => {
            let classified =
                super::recovery_paths::shared::classify_recovery_relay_error(error.as_ref());
            super::recovery_paths::shared::escalate_transient_relay_outcome_with_probe(
                classified,
                || super::recovery_paths::restart::probe_channel_liveness(http, channel_id),
            )
            .await
        }
    }
}

/// Outcome of `complete_recovery_visible_turn` exposed to callers so they can
/// tell whether recovery emitted the visible completion UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RecoveryCompletionOutcome {
    /// Visible completion emitted (or status-panel-v2 was disabled / no
    /// status message id was wired). Callers may proceed with downstream
    /// dispatch / analytics / mailbox finalization as before.
    Emitted,
}

impl RecoveryCompletionOutcome {
    /// `true` when callers should proceed with downstream side effects.
    pub(super) fn should_proceed(self) -> bool {
        matches!(self, RecoveryCompletionOutcome::Emitted)
    }
}

/// #3099: a TUI-injected external-input (task-notification) turn can complete
/// via recovery with `user_msg_id == 0`. The `⏳ → ✅` reaction step is then
/// skipped (there is no anchored Discord user message), but the hourglass was
/// added to a real notify-bot message tracked by the prompt anchor. Such a turn
/// needs the anchor-lifecycle cleanup so the `⏳` is removed from the injected
/// message instead of going stale.
fn recovery_inflight_needs_anchor_lifecycle_cleanup(
    state: &super::inflight::InflightTurnState,
) -> bool {
    state.user_msg_id == 0
        && state.tmux_session_name.is_some()
        && matches!(
            state.turn_source,
            super::inflight::TurnSource::ExternalInput
                | super::inflight::TurnSource::ExternalAdopted
        )
}

pub(super) async fn complete_recovery_visible_turn(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &super::inflight::InflightTurnState,
    background: bool,
    source: &'static str,
) -> RecoveryCompletionOutcome {
    complete_recovery_visible_turn_with_sniffer(
        http,
        shared,
        provider,
        state,
        background,
        source,
        |tmux_session_name| async move {
            // #4353: `super::tmux` is cfg(unix). No tmux pane means nothing can be
            // pending in one.
            #[cfg(unix)]
            {
                super::tmux::sniff_background_agent_pending_for_completion(
                    tmux_session_name.as_deref(),
                )
                .await
            }
            #[cfg(not(unix))]
            {
                let _ = tmux_session_name;
                false
            }
        },
    )
    .await
}

async fn complete_recovery_visible_turn_with_sniffer<S, SniffFuture>(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    state: &super::inflight::InflightTurnState,
    background: bool,
    source: &'static str,
    sniff_background_agent_pending: S,
) -> RecoveryCompletionOutcome
where
    S: FnOnce(Option<String>) -> SniffFuture,
    SniffFuture: std::future::Future<Output = bool>,
{
    let channel_id = ChannelId::new(state.channel_id);
    // A recovery/orphan turn may carry no user message (user_msg_id == 0,
    // e.g. a TUI-direct turn). There is then no user message to react against,
    // so the ⏳→✅ reaction step is skipped while the quiescence gate and
    // status-panel completion still run. `MessageId::new(0)` would panic.
    let user_msg_id = super::inflight::optional_message_id(state.user_msg_id);

    // #4047: recovery completes from terminal evidence, not pane quiescence.
    // The TUI gate is retained as observation-only liveness/strict-signal
    // telemetry; it must not suppress the visible completion event or reaction.
    #[cfg(unix)]
    if let Some(tmux_session_name) = state.tmux_session_name.as_deref() {
        let outcome = super::tmux::run_tui_completion_gate(
            provider,
            channel_id,
            tmux_session_name,
            state.task_notification_kind,
        )
        .await;
        let _ = outcome;
    }

    let generation = state.born_generation;
    if let Some(user_msg_id) = user_msg_id {
        tv_done(shared, http, channel_id, user_msg_id, generation, "done").await;
    } else if recovery_inflight_needs_anchor_lifecycle_cleanup(state) {
        // #3099: user_msg_id==0 recovery completes this turn's own injected
        // prompt anchor, not the shared slot a later turn may own.
        if let Some(tmux_session_name) = state.tmux_session_name.as_deref() {
            let _ = super::tui_prompt_relay::complete_tui_direct_anchor_lifecycle_for_inflight(
                shared,
                provider.as_str(),
                tmux_session_name,
                channel_id,
                state.injected_prompt_message_id,
                generation,
                "recovery_task_notification_anchor_cleanup_user_msg_zero",
            )
            .await;
        }
    }

    if !shared.ui.status_panel_v2_enabled {
        return RecoveryCompletionOutcome::Emitted;
    }
    // #2427 D wire: explicit completion signal — most recovery paths
    // already call `clear_inflight_state` unconditionally; this is a
    // safety net for any branch that emits TurnCompleted without doing
    // so. user_msg_id guard defeats Pitfall #1 (next turn race).
    #[cfg(unix)]
    super::tmux::emit_explicit_inflight_cleanup_signal(
        provider,
        channel_id,
        state.user_msg_id,
        "turn_completed_recovery",
    );
    let started_at_unix = super::inflight::parse_started_at_unix(&state.started_at)
        .unwrap_or_else(|| chrono::Utc::now().timestamp());
    let Some(status_msg_id) = recovery_status_panel::completion_target(
        shared.ui.status_panel_v2_enabled,
        state,
        provider,
        channel_id,
    ) else {
        return RecoveryCompletionOutcome::Emitted;
    };

    complete_recovery_status_panel_with_sniffer(
        http,
        shared,
        provider,
        state,
        channel_id,
        status_msg_id,
        started_at_unix,
        background,
        source,
        sniff_background_agent_pending,
    )
    .await
}

#[cfg(test)]
mod recovery_dispatch_gate_tests {
    #[test]
    fn recovery_dispatch_advance_requires_successful_relay() {
        assert!(super::should_advance_recovery_dispatch_after_relay(true));
        assert!(!super::should_advance_recovery_dispatch_after_relay(false));
    }
}

#[cfg(test)]
mod recovery_completion_outcome_tests {
    use super::{RecoveryCompletionOutcome, recovery_status_panel};
    use crate::services::agent_protocol::StatusEvent;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};
    use std::sync::{Arc, Mutex};

    struct RuntimeRootGuard {
        previous: Option<std::ffi::OsString>,
        _root: tempfile::TempDir,
    }

    impl RuntimeRootGuard {
        fn new() -> Self {
            let root = tempfile::tempdir().expect("runtime root");
            let previous = std::env::var_os("AGENTDESK_ROOT_DIR");
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", root.path()) };
            Self {
                previous,
                _root: root,
            }
        }
    }

    impl Drop for RuntimeRootGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn isolate_agentdesk_runtime_root() -> (std::sync::MutexGuard<'static, ()>, RuntimeRootGuard) {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let root = RuntimeRootGuard::new();
        (lock, root)
    }

    fn state_for_recovery(user_msg_id: u64) -> super::inflight::InflightTurnState {
        serde_json::from_value(serde_json::json!({
            "version": 9,
            "provider": "claude",
            "channel_id": 4243,
            "channel_name": "adk-cc",
            "request_owner_user_id": 7,
            "user_msg_id": user_msg_id,
            "current_msg_id": user_msg_id + 1,
            "current_msg_len": 0,
            "user_text": "prompt",
            "source": "text",
            "session_id": "session",
            "tmux_session_name": "AgentDesk-claude-adk-cc",
            "output_path": "/tmp/claude-transcript.jsonl",
            "input_fifo_path": null,
            "last_offset": 0,
            "full_response": "",
            "response_sent_offset": 0,
            "started_at": "2026-01-01 00:00:00",
            "updated_at": "2026-01-01 00:00:00"
        }))
        .expect("recovery test inflight state")
    }

    #[test]
    fn emitted_lets_callers_proceed_with_dispatch_finalize() {
        assert!(RecoveryCompletionOutcome::Emitted.should_proceed());
    }

    #[test]
    fn completion_prefers_guarded_persisted_fallback_message_id() {
        let mut snapshot = state_for_recovery(9101);
        snapshot.status_message_id = Some(3003);
        let mut persisted = state_for_recovery(9101);
        persisted.status_message_id = Some(4004);

        let status_msg_id =
            recovery_status_panel::message_id_for_completion(&snapshot, Some(&persisted));

        assert_eq!(status_msg_id, Some(super::MessageId::new(4004)));
    }

    #[test]
    fn completion_ignores_persisted_id_from_different_turn() {
        let mut snapshot = state_for_recovery(9101);
        snapshot.status_message_id = Some(3003);
        let mut persisted = state_for_recovery(9201);
        persisted.status_message_id = Some(4004);

        let status_msg_id =
            recovery_status_panel::message_id_for_completion(&snapshot, Some(&persisted));

        assert_eq!(status_msg_id, Some(super::MessageId::new(3003)));
    }

    #[test]
    fn footer_mode_skips_recovery_status_panel_completion_for_stale_persisted_id() {
        let mut snapshot = state_for_recovery(9101);
        snapshot.status_message_id = Some(3003);
        let mut persisted = state_for_recovery(9101);
        persisted.status_message_id = Some(4004);

        let target = recovery_status_panel::completion_target_for_flags(
            true,
            true,
            &snapshot,
            Some(&persisted),
        );

        assert_eq!(
            target, None,
            "footer mode must not edit or SendFallback a separate recovery panel; the stale id is left for sweeper reclaim"
        );
    }

    #[test]
    fn flag_off_recovery_status_panel_completion_keeps_original_target() {
        let mut snapshot = state_for_recovery(9101);
        snapshot.status_message_id = Some(3003);
        let mut persisted = state_for_recovery(9101);
        persisted.status_message_id = Some(4004);

        let target = recovery_status_panel::completion_target_for_flags(
            false,
            true,
            &snapshot,
            Some(&persisted),
        );

        assert_eq!(target, Some(Some(super::MessageId::new(4004))));
    }

    #[test]
    fn flag_off_recovery_none_target_still_requests_send_fallback() {
        let snapshot = state_for_recovery(9101);

        let target =
            recovery_status_panel::completion_target_for_flags(false, true, &snapshot, None);

        assert_eq!(
            target,
            Some(None),
            "flag-off v2 recovery preserves the SendFallback rollback behavior when no status_message_id was persisted"
        );
    }

    #[tokio::test]
    async fn recovery_status_panel_completion_emits_background_agent_pending_payload() {
        let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root();
        let mut shared = super::super::make_shared_data_for_tests();
        Arc::get_mut(&mut shared)
            .expect("fresh test shared data should be uniquely owned")
            .ui
            .status_panel_v2_enabled = true;
        let http = poise::serenity_prelude::Http::new("Bot test-token");
        let provider = ProviderKind::Claude;
        let state = state_for_recovery(9101);
        let channel_id = ChannelId::new(state.channel_id);
        let started_at_unix = 1_700_000_000;
        shared.ui.placeholder_live_events.push_status_event(
            channel_id,
            StatusEvent::TurnCompleted {
                background: false,
                background_agent_pending: true,
            },
        );
        let mut last_status_panel_text = shared.ui.placeholder_live_events.render_status_panel(
            channel_id,
            &provider,
            started_at_unix,
        );

        let committed = super::super::turn_bridge::complete_status_panel_v2_with_http(
            &shared,
            &http,
            channel_id,
            Some(MessageId::new(4_047_301)),
            &provider,
            started_at_unix,
            &mut last_status_panel_text,
            false,
            true,
            "test_recovery_background_agent_pending_payload",
            (Some(state.user_msg_id), Some(&state)),
        )
        .await;

        assert!(committed);
        let rendered = shared
            .ui
            .placeholder_live_events
            .render_completion_footer(channel_id, &provider, "⠸");
        let block = rendered.block.expect("background-agent pending footer");

        assert!(rendered.has_unfinished_entries);
        assert!(block.contains("Background agents"));
        assert!(block.contains("Waiting for background agents ⠸"));
    }

    #[tokio::test]
    async fn recovery_status_panel_completion_producer_threads_sniffed_background_agent_pending() {
        let (_env_lock, _runtime_root) = isolate_agentdesk_runtime_root();
        for (pending, channel_raw) in [(true, 4_047_311), (false, 4_047_312)] {
            let shared = super::super::make_shared_data_for_tests();
            let provider = ProviderKind::Claude;
            let mut state = state_for_recovery(9101);
            state.channel_id = channel_raw;
            state.tmux_session_name = Some("AgentDesk-claude-recovery-background-test".to_string());
            let channel_id = ChannelId::new(state.channel_id);
            let observed_tmux_session = Arc::new(Mutex::new(Vec::new()));
            let sniffer_observed_tmux_session = observed_tmux_session.clone();
            let sink_shared = shared.clone();

            let outcome = super::complete_recovery_status_panel_with_sniffer_and_sink(
                &state,
                move |tmux_session_name| async move {
                    sniffer_observed_tmux_session
                        .lock()
                        .expect("observed tmux session lock")
                        .push(tmux_session_name);
                    pending
                },
                move |background_agent_pending| async move {
                    sink_shared.ui.placeholder_live_events.push_status_event(
                        channel_id,
                        StatusEvent::TurnCompleted {
                            background: false,
                            background_agent_pending,
                        },
                    );
                    true
                },
            )
            .await;

            assert_eq!(outcome, RecoveryCompletionOutcome::Emitted);
            assert_eq!(
                observed_tmux_session
                    .lock()
                    .expect("observed tmux session lock")
                    .as_slice(),
                &[Some(
                    "AgentDesk-claude-recovery-background-test".to_string()
                )]
            );

            let rendered = shared
                .ui
                .placeholder_live_events
                .render_completion_footer(channel_id, &provider, "⠸");
            let block_has_background_agents = rendered
                .block
                .as_deref()
                .is_some_and(|block| block.contains("Background agents"));

            assert_eq!(rendered.has_unfinished_entries, pending);
            assert_eq!(block_has_background_agents, pending);
        }
    }

    #[test]
    fn recovery_takeover_forgets_registered_completion_footer_target() {
        let channel_id = ChannelId::new(3_089_201);
        let shared = super::super::make_shared_data_for_tests();
        super::super::footer_view_reconciler::completion_footer_forget_registered_target(
            channel_id,
        );
        let _ = super::super::footer_view_reconciler::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_301),
            &ProviderKind::Claude,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );

        assert!(
            super::super::footer_view_reconciler::note_footer_suppressed_for_message_takeover(
                channel_id,
                MessageId::new(3_089_301),
            )
        );

        assert_eq!(
            super::super::footer_view_reconciler::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel_id,
                "⠸",
                1_800_000_005,
            ),
            None
        );
    }

    #[test]
    fn recovery_takeover_keeps_different_completion_footer_target() {
        let channel_id = ChannelId::new(3_089_211);
        let shared = super::super::make_shared_data_for_tests();
        super::super::footer_view_reconciler::completion_footer_forget_registered_target(
            channel_id,
        );
        let _ = super::super::footer_view_reconciler::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_311),
            &ProviderKind::Claude,
            1_800_000_000,
            "Final answer",
            None,
            true,
        );

        assert!(
            !super::super::footer_view_reconciler::note_footer_suppressed_for_message_takeover(
                channel_id,
                MessageId::new(3_089_312),
            )
        );

        assert!(
            super::super::footer_view_reconciler::completion_footer_edit_for_registered_target_at(
                shared.as_ref(),
                channel_id,
                "⠸",
                1_800_000_005,
            )
            .is_some()
        );
        super::super::footer_view_reconciler::completion_footer_forget_registered_target(
            channel_id,
        );
    }
}

#[cfg(test)]
mod no_anchored_placeholder_recovery_tests {
    //! Regression coverage for the msgid-0 panic class: a TUI-direct / recovery
    //! inflight legitimately carries `current_msg_id == 0` (never anchored a
    //! Discord placeholder, `status_message_id = None`) and may carry
    //! `user_msg_id == 0` (no Discord user message). The startup recovery /
    //! reattach loop derives placeholder / user message ids from these fields;
    //! before the fix it called `serenity::MessageId::new(0)`, which panics
    //! ("Attempted to call MessageId::new with invalid (0) value"). Because that
    //! loop awaits inline, one such inflight aborted recovery before
    //! `reconcile_done`, leaving the provider permanently degraded. These tests
    //! assert the conversion helpers and the watcher-restore seed never panic
    //! and yield a sane "no placeholder, recover watcher/session" result.
    use super::inflight::{InflightTurnState, optional_message_id};
    use super::optional_message_id as engine_optional_message_id;
    use crate::services::provider::ProviderKind;

    fn tui_direct_no_anchor_state() -> InflightTurnState {
        // TUI-direct turn: never anchored a Discord placeholder
        // (current_msg_id == 0, status_message_id == None) and has no Discord
        // user message (user_msg_id == 0), but DOES own a live tmux session.
        InflightTurnState::new(
            ProviderKind::Claude,
            4243,
            Some("adk-cc".to_string()),
            7,
            0, // user_msg_id == 0
            0, // current_msg_id == 0
            "tui direct prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/claude-transcript.jsonl".to_string()),
            None,
            0,
        )
    }

    // #3099: a TUI-injected task-notification turn recovered with
    // `user_msg_id == 0` skips the `⏳ → ✅` reaction step, so it must route to
    // the anchor-lifecycle cleanup that removes the hourglass from the injected
    // notify-bot message.
    #[test]
    fn recovery_external_input_zero_user_msg_needs_anchor_cleanup() {
        let mut state = tui_direct_no_anchor_state();
        state.turn_source = super::inflight::TurnSource::ExternalInput;
        assert!(super::recovery_inflight_needs_anchor_lifecycle_cleanup(
            &state
        ));

        let mut adopted = tui_direct_no_anchor_state();
        adopted.turn_source = super::inflight::TurnSource::ExternalAdopted;
        assert!(super::recovery_inflight_needs_anchor_lifecycle_cleanup(
            &adopted
        ));
    }

    // A managed turn, or an external turn with a real anchored user message id,
    // must NOT use the injected-anchor cleanup path.
    #[test]
    fn recovery_managed_or_anchored_turn_skips_anchor_cleanup() {
        // Default turn_source (Managed) with user_msg_id == 0 is not external.
        let managed = tui_direct_no_anchor_state();
        assert!(!super::recovery_inflight_needs_anchor_lifecycle_cleanup(
            &managed
        ));

        // External-input turn WITH a real anchored user message uses the normal
        // ⏳ → ✅ block, not the anchor cleanup.
        let mut anchored = tui_direct_no_anchor_state();
        anchored.turn_source = super::inflight::TurnSource::ExternalInput;
        anchored.user_msg_id = 777;
        assert!(!super::recovery_inflight_needs_anchor_lifecycle_cleanup(
            &anchored
        ));
    }

    #[test]
    fn optional_message_id_maps_zero_to_none_without_panicking() {
        assert_eq!(optional_message_id(0), None);
        assert_eq!(
            optional_message_id(123),
            Some(super::MessageId::new(123)),
            "non-zero ids must still build a real MessageId"
        );
        // Reachable via the engine re-export used across the recovery loop.
        assert_eq!(engine_optional_message_id(0), None);
    }

    #[test]
    fn recovery_loop_bindings_from_zero_state_are_none_not_panic() {
        // This mirrors the eager bindings at the top of the recovery loop body
        // (`restore_inflight_turns`) that previously panicked on a 0-valued
        // inflight before `reconcile_done` could be reached.
        let state = tui_direct_no_anchor_state();
        assert_eq!(state.current_msg_id, 0);
        assert_eq!(state.user_msg_id, 0);
        assert_eq!(state.status_message_id, None);

        let current_msg_id = optional_message_id(state.current_msg_id);
        let user_msg_id = optional_message_id(state.user_msg_id);
        assert_eq!(
            current_msg_id, None,
            "no anchored placeholder → relay step is skipped, not panicked"
        );
        assert_eq!(
            user_msg_id, None,
            "no Discord user message → reaction/analytics steps are skipped"
        );
    }

    // `super::super::tmux` is `#[cfg(unix)]`-only (the tmux relay is Unix-only),
    // so gate this test to Unix to keep the Windows cross-OS build green.
    #[cfg(unix)]
    #[test]
    fn watcher_restore_seed_skips_placeholder_but_preserves_session() {
        // The pane-alive reattach branch seeds the watcher from the inflight.
        // For a no-anchor turn there is no placeholder to restore, but the tmux
        // session/watcher must still be recovered — `restored_watcher_turn_from_inflight`
        // returns None (no placeholder) WITHOUT panicking, and the watcher is
        // spawned regardless by the caller.
        let state = tui_direct_no_anchor_state();
        let restored = super::super::tmux::restored_watcher_turn_from_inflight(
            &state,
            "AgentDesk-claude-adk-cc",
            true,
        );
        assert!(
            restored.is_none(),
            "current_msg_id == 0 yields no restored placeholder (no MessageId::new(0) panic)"
        );
    }

    #[test]
    fn recovered_transcript_turn_id_avoids_bogus_zero_key() {
        // user_msg_id == 0 must NOT key on discord:<channel>:0 (would collide /
        // overwrite across every no-user-message turn in the channel), and must
        // include a per-turn discriminator so repeated message-less turns in the
        // same session do not upsert-overwrite each other (Codex P2 r3).
        assert_eq!(
            super::recovered_transcript_turn_id(4243, 0, Some("session-abc"), Some(4096), "x"),
            "discord:4243:session:session-abc:off4096"
        );
        // No offset → fall back to the started_at timestamp discriminator.
        assert_eq!(
            super::recovered_transcript_turn_id(4243, 0, None, None, "2026-06-01 12:00:00"),
            "discord:4243:recovery:at2026-06-01-12-00-00"
        );
        // Two message-less turns in the same session must NOT collide.
        let a = super::recovered_transcript_turn_id(4243, 0, Some("s"), Some(100), "t");
        let b = super::recovered_transcript_turn_id(4243, 0, Some("s"), Some(200), "t");
        assert_ne!(a, b);
        // A real user message keeps the legacy message-keyed transcript id.
        assert_eq!(
            super::recovered_transcript_turn_id(4243, 99, Some("session-abc"), Some(4096), "x"),
            "discord:4243:99"
        );
    }
}
