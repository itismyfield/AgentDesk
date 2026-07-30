//! Post-commit terminal delivery epilogue for terminal outcome delivery.

use std::sync::{Arc, atomic::Ordering};

use super::super::streaming_edit_text::TuiErrorClassification;
use super::*;

pub(super) enum DeliveryEpilogueMessage {
    PostCommit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DeliveryEpilogueOutcome {
    Continue,
}

pub(super) struct DeliveryEpilogueContext<'a> {
    pub(super) shared_owned: &'a Arc<SharedData>,
    pub(super) gateway: &'a Arc<dyn TurnGateway>,
    pub(super) provider: &'a ProviderKind,
    pub(super) channel_id: ChannelId,
    pub(super) user_msg_id: Option<MessageId>,
    pub(super) current_msg_id: MessageId,
    pub(super) adk_session_key: &'a Option<String>,
    pub(super) adk_cwd: &'a Option<String>,
    pub(super) dispatch_id: &'a Option<String>,
    pub(super) turn_id: &'a String,
    pub(super) user_text_owned: &'a String,
    pub(super) full_response: &'a String,
    pub(super) delivery_response: &'a String,
    pub(super) spoken_delivery_response: &'a String,
    pub(super) cancelled: bool,
    pub(super) is_prompt_too_long: bool,
    pub(super) transport_error: bool,
    pub(super) recovery_retry: bool,
    pub(super) resume_failure_detected: bool,
    pub(super) claude_tui_followup_pre_submit_requeue_candidate: bool,
    pub(super) claude_tui_busy_requeue_pending: bool,
    pub(super) tui_error_classification: TuiErrorClassification,
    #[cfg(unix)]
    pub(super) bridge_tui_gate_outcome_early:
        Option<super::super::super::tmux::TuiCompletionGateOutcome>,
    pub(super) terminal_delivery_committed: bool,
    pub(super) terminal_body_visible: bool,
    pub(super) preserve_inflight_for_cleanup_retry: bool,
    pub(super) should_complete_work_dispatch_after_delivery: bool,
    pub(super) should_fail_dispatch_after_delivery: bool,
    pub(super) bridge_relay_delegated_to_watcher: bool,
    pub(super) watcher_owner_channel_id: ChannelId,
    pub(super) can_chain_locally: bool,
    pub(super) inflight_generation: u64,
}

pub(super) struct DeliveryEpilogueState<'a> {
    pub(super) response_sent_offset: &'a mut usize,
    pub(super) inflight_state: &'a mut InflightTurnState,
    pub(super) terminal_full_replay_cleanup_msg_ids: &'a mut Vec<MessageId>,
    pub(super) bridge_should_emit_completion: &'a mut bool,
    pub(super) status_panel_terminal_committed: &'a mut bool,
    pub(super) busy_requeue_outcome: &'a mut Option<followup_requeue::FollowupRequeueOutcome>,
}

#[rustfmt::skip]
pub(super) async fn handle_delivery_epilogue(
    message: DeliveryEpilogueMessage,
    ctx: DeliveryEpilogueContext<'_>,
    state: DeliveryEpilogueState<'_>,
) -> DeliveryEpilogueOutcome {
    let (shared_owned, gateway) = (Arc::clone(ctx.shared_owned), Arc::clone(ctx.gateway));
    let (provider, channel_id) = (ctx.provider.clone(), ctx.channel_id);
    let (user_msg_id, current_msg_id) = (ctx.user_msg_id, ctx.current_msg_id);
    let (adk_session_key, adk_cwd) = (ctx.adk_session_key, ctx.adk_cwd);
    let (dispatch_id, turn_id) = (ctx.dispatch_id, ctx.turn_id);
    let user_text_owned = ctx.user_text_owned;
    let full_response = ctx.full_response.clone();
    let delivery_response = ctx.delivery_response.clone();
    let spoken_delivery_response = ctx.spoken_delivery_response.clone();
    let (cancelled, is_prompt_too_long) = (ctx.cancelled, ctx.is_prompt_too_long);
    let transport_error = ctx.transport_error;
    let recovery_retry = ctx.recovery_retry;
    let resume_failure_detected = ctx.resume_failure_detected;
    let claude_tui_followup_pre_submit_requeue_candidate =
        ctx.claude_tui_followup_pre_submit_requeue_candidate;
    let claude_tui_busy_requeue_pending = ctx.claude_tui_busy_requeue_pending;
    let tui_error_classification = ctx.tui_error_classification;
    #[cfg(unix)]
    let bridge_tui_gate_outcome_early = ctx.bridge_tui_gate_outcome_early;
    let terminal_delivery_committed = ctx.terminal_delivery_committed;
    let terminal_body_visible = ctx.terminal_body_visible;
    let preserve_inflight_for_cleanup_retry = ctx.preserve_inflight_for_cleanup_retry;
    let should_complete_work_dispatch_after_delivery =
        ctx.should_complete_work_dispatch_after_delivery;
    let should_fail_dispatch_after_delivery = ctx.should_fail_dispatch_after_delivery;
    let bridge_relay_delegated_to_watcher = ctx.bridge_relay_delegated_to_watcher;
    let watcher_owner_channel_id = ctx.watcher_owner_channel_id;
    let can_chain_locally = ctx.can_chain_locally;
    let inflight_generation = ctx.inflight_generation;

    let mut response_sent_offset = *state.response_sent_offset;
    let inflight_state = &mut *state.inflight_state;
    let terminal_full_replay_cleanup_msg_ids =
        &mut *state.terminal_full_replay_cleanup_msg_ids;
    let mut bridge_should_emit_completion = *state.bridge_should_emit_completion;
    let mut status_panel_terminal_committed = *state.status_panel_terminal_committed;
    let busy_requeue_outcome = &mut *state.busy_requeue_outcome;

    match message {
        DeliveryEpilogueMessage::PostCommit => {
        if terminal_delivery_committed {
            response_sent_offset = full_response.len();
            inflight_state.response_sent_offset = response_sent_offset;
            inflight_state.terminal_delivery_committed = true;
            inflight_state.full_response = full_response.clone();
            let mirror_outcome =
                crate::services::discord::inflight::save_inflight_state_if_identity_unchanged(
                    &mut *inflight_state,
                    "turn_bridge::terminal_delivery_committed_mirror@5536",
                );
            crate::services::discord::inflight::terminal_delivery_evidence_loss::warn_for_bridge_terminal_mirror_outcome(
                mirror_outcome,
                crate::services::discord::inflight::terminal_delivery_evidence_loss::BridgeEvidenceLossContext {
                    provider: &provider,
                    channel_id,
                    current_msg_id,
                    response_sent_offset,
                },
            );
            for frozen_msg_id in terminal_full_replay_cleanup_msg_ids.drain(..) {
                // #5413/#3607: current_msg_id is the terminal answer and is
                // already excluded here, so no terminal-anchor guard is
                // needed — every drained id is a non-terminal streamed prefix.
                if frozen_msg_id == current_msg_id {
                    continue;
                }
                let (replay_outcome, replay_detail) = match gateway
                    .delete_message(channel_id, frozen_msg_id)
                    .await
                {
                    Ok(()) => {
                        tracing::info!(
                            target: "agentdesk::codex_rollout_handoff",
                            provider = %provider.as_str(),
                            channel_id = channel_id.get(),
                            message_id = frozen_msg_id.get(),
                            "turn_bridge removed streamed rollover prefix after full terminal replay"
                        );
                        ("committed", None)
                    }
                    Err(error) => {
                        tracing::warn!(
                            target: "agentdesk::codex_rollout_handoff",
                            provider = %provider.as_str(),
                            channel_id = channel_id.get(),
                            message_id = frozen_msg_id.get(),
                            error = %error,
                            "turn_bridge failed to remove streamed rollover prefix after full terminal replay"
                        );
                        ("failed", Some(error))
                    }
                };
                crate::services::observability::emit_relay_delete(
                    provider.as_str(),
                    channel_id.get(),
                    frozen_msg_id.get(),
                    adk_session_key.as_deref(),
                    Some(turn_id.as_str()),
                    "full_terminal_replay_prefix",
                    "delete_nonterminal",
                    replay_outcome,
                    replay_detail.as_deref(),
                );
            }
            // #2236: look up the typed handoff marker stamped at dispatch
            // time so multi-agent setups with overlapping background
            // channels can be disambiguated by the stored agent_id. The
            // marker is consumed inside voice_background_completion_target
            // below; passing the stored agent_id (cloned, not taken) here
            // keeps both lookups consistent for this single dispatch.
            //
            // #2274: if the in-memory marker is absent (dcserver
            // restarted mid-turn, or rehydration has not yet completed)
            // fall back to the durable PG row for the agent_id lookup.
            // A recovery turn with no anchored user message (user_msg_id ==
            // 0) is never a voice turn — voice turns carry a synthetic,
            // non-zero voice message id — so the voice-handoff completion
            // routing (all keyed on the user message id) does not apply.
            if let Some(user_msg_id) = user_msg_id {
                let pg_pool_for_handoff = shared_owned.pg_pool.as_ref();
                let in_memory_handoff_agent_id = crate::voice::announce_meta::global_store()
                    .get_handoff(user_msg_id)
                    .and_then(|meta| meta.agent_id);
                let stored_handoff_agent_id = match in_memory_handoff_agent_id {
                    Some(agent_id) => Some(agent_id),
                    None => match pg_pool_for_handoff {
                        Some(pool) => {
                            crate::voice::announce_meta::load_handoff_durable(pool, user_msg_id)
                                .await
                                .ok()
                                .flatten()
                                .and_then(|meta| meta.agent_id)
                        }
                        None => None,
                    },
                };
                let mapped_voice_channel_id = shared_owned
                    .voice_barge_in
                    .voice_channel_for_background(channel_id, stored_handoff_agent_id.as_deref())
                    .await;
                if let Some(voice_channel_id) = voice_background_completion_target(
                    mapped_voice_channel_id,
                    dispatch_id.as_deref(),
                    user_msg_id,
                    Some(&turn_id),
                    &user_text_owned,
                    channel_id,
                    pg_pool_for_handoff,
                )
                .await
                {
                    if !inflight_state.silent_turn {
                        let voice_barge_in = shared_owned.voice_barge_in.clone();
                        let shared_for_voice = shared_owned.clone();
                        let summary_source = spoken_delivery_response.clone();
                        let background_channel_id = channel_id;
                        let failed = cancelled
                            || is_prompt_too_long
                            || transport_error
                            || recovery_retry
                            || resume_failure_detected;
                        super::super::super::task_supervisor::spawn_observed(
                            "voice_background_completion_summary",
                            async move {
                                voice_barge_in
                                    .speak_voice_background_completion_summary(
                                        &shared_for_voice,
                                        voice_channel_id,
                                        background_channel_id,
                                        &summary_source,
                                        failed,
                                    )
                                    .await;
                                voice_barge_in.publish_progress_for_playback(
                                    background_channel_id,
                                    Some(voice_channel_id),
                                    "agent:done",
                                );
                            },
                        );
                    } else {
                        shared_owned.voice_barge_in.publish_progress_for_playback(
                            channel_id,
                            Some(voice_channel_id),
                            "agent:done",
                        );
                    }
                } else if inflight_state.source == crate::dispatch::Source::Voice {
                    if !inflight_state.silent_turn {
                        shared_owned
                            .voice_barge_in
                            .spawn_spoken_result_playback(
                                &shared_owned,
                                channel_id,
                                &spoken_delivery_response,
                            )
                            .await;
                    }
                    shared_owned
                        .voice_barge_in
                        .publish_progress(channel_id, "agent:done");
                }
            }
        }

        // #2161 (Codex round-2 H1): run the TUI completion gate BEFORE
        // dispatch completion / queue drain so a still-busy ClaudeTui
        // pane cannot advance lifecycle state ahead of pane quiescence.
        // The same outcome is reused by the status-panel emit below.
        // The gate lives in the `tmux` module (`#[cfg(unix)]`); on
        // non-unix targets we skip it and emit completion as normal.
        //
        // #2293 — when the early gate (~line 4254, hoisted ABOVE the
        // mailbox release) already ran for this turn, reuse its outcome
        // here instead of polling tmux a second time. The early
        // result is the authoritative one because it was sampled
        // closer to the moment the mailbox would have been released;
        // re-polling here would create a window where the pane could
        // settle into idle AFTER we already deferred mailbox cleanup,
        // producing inconsistent dispatch / status semantics across
        // the two gate sites.
        #[cfg(unix)]
        {
            let tui_transport_error_skip_gate = claude_tui_followup_pre_submit_requeue_candidate
                || (transport_error
                    && bridge_tui_transport_error_should_skip_quiescence(
                        &provider,
                        inflight_state.runtime_kind,
                        &full_response,
                        tui_error_classification,
                    ));
            let bridge_gate_outcome = if terminal_delivery_committed
                && !preserve_inflight_for_cleanup_retry
                && !tui_transport_error_skip_gate
            {
                if let Some(outcome) = bridge_tui_gate_outcome_early {
                    outcome
                } else if let Some(tmux_session_name) = inflight_state.tmux_session_name.as_deref()
                {
                    super::super::super::tmux::run_tui_completion_gate(
                        &provider,
                        channel_id,
                        tmux_session_name,
                        inflight_state.task_notification_kind,
                    )
                    .await
                } else {
                    super::super::super::tmux::TuiCompletionGateOutcome::NotGated
                }
            } else {
                if terminal_delivery_committed && tui_transport_error_skip_gate {
                    tracing::info!(
                        provider = %provider.as_str(),
                        channel_id = channel_id.get(),
                        runtime_kind = ?inflight_state.runtime_kind,
                        "TUI transport error was already delivered; skipping quiescence gate so inflight cleanup can complete"
                    );
                }
                followup_requeue::requeue_if_needed(
                    busy_requeue_outcome,
                    claude_tui_followup_pre_submit_requeue_candidate,
                    claude_tui_busy_requeue_pending,
                    &shared_owned,
                    &provider,
                    channel_id,
                    &inflight_state,
                    dispatch_id.as_deref(),
                    adk_session_key.as_deref(),
                    turn_id.as_str(),
                )
                .await;
                super::super::super::tmux::TuiCompletionGateOutcome::NotGated
            };

            bridge_should_emit_completion = bridge_gate_outcome.should_emit_completion();
        }

        if should_complete_work_dispatch_after_terminal_delivery(
            should_complete_work_dispatch_after_delivery,
            terminal_delivery_committed,
            preserve_inflight_for_cleanup_retry,
            resume_failure_detected,
            recovery_retry,
            &full_response,
        ) {
            complete_work_dispatch_on_turn_end(
                &shared_owned,
                dispatch_id.as_deref(),
                adk_cwd.as_deref(),
                Some(&full_response),
            )
            .await;
        } else if should_fail_dispatch_after_terminal_delivery(
            should_fail_dispatch_after_delivery,
            terminal_delivery_committed,
            preserve_inflight_for_cleanup_retry,
        ) {
            // Transport error — fail the dispatch only after the terminal
            // error response is deliverable, so auto-queue does not advance
            // ahead of visible turn completion.
            fail_dispatch_with_retry(
                shared_owned.api_port,
                dispatch_id.as_deref(),
                &full_response,
            )
            .await;
        }

        // Signal the watcher that this turn's response was already delivered.
        // Prevents the watcher from relaying the same response when it resumes.
        // #3041 P1-2 (codex P1-c): a B2 Skip set
        // `preserve_inflight_for_cleanup_retry = true`, so this gate (encoded in
        // `bridge_epilogue_marks_watcher_delivered`) does NOT mark the watcher
        // delivered — the bridge never delivered the range; the holder owns it.
        if bridge_epilogue_marks_watcher_delivered(
            preserve_inflight_for_cleanup_retry,
            bridge_relay_delegated_to_watcher,
        ) && let Some(watcher) = shared_owned.tmux_watchers.get(&watcher_owner_channel_id)
        {
            watcher.turn_delivered.store(true, Ordering::Release);
        }

        if can_chain_locally
            && !preserve_inflight_for_cleanup_retry
            && !delivery_response.trim().is_empty()
            && let Some(user_msg_id) = user_msg_id
        {
            tv_done(
                &shared_owned,
                channel_id,
                user_msg_id,
                inflight_generation,
                "done",
            )
            .await;
        }

        td_warn(
            terminal_delivery_committed,
            preserve_inflight_for_cleanup_retry,
            channel_id,
        );
        status_panel_terminal_committed = status_panel_completion_ready_after_terminal_body(
            terminal_delivery_committed,
            terminal_body_visible,
            preserve_inflight_for_cleanup_retry,
        );
        if status_panel_terminal_committed {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ▶ Response sent");
            if let Ok(mut last) = shared_owned.last_turn_at.lock() {
                *last = Some(chrono::Local::now().to_rfc3339());
            }
        }
        }
    }

    *state.response_sent_offset = response_sent_offset;
    *state.bridge_should_emit_completion = bridge_should_emit_completion;
    *state.status_panel_terminal_committed = status_panel_terminal_committed;

    DeliveryEpilogueOutcome::Continue
}

#[cfg(test)]
mod tests {
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };

    use super::*;
    use crate::services::discord::{formatting::ReplaceLongMessageOutcome, gateway::GatewayFuture};
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct CapturingWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .expect("capturing writer lock")
                .extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;

        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    struct NoopGateway;

    impl TurnGateway for NoopGateway {
        fn send_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            panic!("delivery epilogue test must not send a message")
        }

        fn edit_message<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            panic!("delivery epilogue test must not edit a message")
        }

        fn replace_message_with_outcome<'a>(
            &'a self,
            _channel_id: ChannelId,
            _message_id: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            panic!("delivery epilogue test must not replace a message")
        }

        fn schedule_retry_with_history<'a>(
            &'a self,
            _channel_id: ChannelId,
            _user_message_id: MessageId,
            _user_text: &'a str,
        ) -> GatewayFuture<'a, ()> {
            panic!("delivery epilogue test must not schedule a retry")
        }

        fn dispatch_queued_turn<'a>(
            &'a self,
            _channel_id: ChannelId,
            _intervention: &'a Intervention,
            _request_owner_name: &'a str,
            _has_more_queued_turns: bool,
            _dispatch_lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
        ) -> GatewayFuture<'a, Result<(), String>> {
            panic!("delivery epilogue test must not dispatch a queued turn")
        }

        fn validate_live_routing<'a>(
            &'a self,
            _channel_id: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            Box::pin(async { Ok(()) })
        }

        fn requester_mention(&self) -> Option<String> {
            None
        }

        fn can_chain_locally(&self) -> bool {
            false
        }

        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            Some(ProviderKind::Claude)
        }
    }

    #[tokio::test]
    async fn terminal_delivery_epilogue_routes_identity_mismatch_to_warn() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let temp = tempfile::TempDir::new().expect("runtime root");
        let _env_reset = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            temp.path(),
        );
        let channel_id = ChannelId::new(5_025);
        let current_msg_id = MessageId::new(5_026);
        let provider = ProviderKind::Claude;
        let mut stale = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            Some("terminal-delivery-epilogue".to_string()),
            1,
            100,
            current_msg_id.get(),
            "stale turn".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        let newer = InflightTurnState::new(
            provider.clone(),
            channel_id.get(),
            Some("terminal-delivery-epilogue".to_string()),
            1,
            200,
            current_msg_id.get(),
            "newer turn".to_string(),
            None,
            None,
            None,
            None,
            0,
        );
        crate::services::discord::inflight::save_inflight_state(&newer).expect("seed newer owner");

        let shared = crate::services::discord::make_shared_data_for_tests();
        let gateway: Arc<dyn TurnGateway> = Arc::new(NoopGateway);
        let full_response = "delivered body".to_string();
        let delivery_response = full_response.clone();
        let spoken_delivery_response = full_response.clone();
        let adk_session_key = None;
        let adk_cwd = None;
        let dispatch_id = None;
        let turn_id = "terminal-delivery-epilogue-test".to_string();
        let user_text = "user prompt".to_string();
        let mut response_sent_offset = 0;
        let mut terminal_full_replay_cleanup_msg_ids = Vec::new();
        let mut bridge_should_emit_completion = false;
        let mut status_panel_terminal_committed = false;
        let mut busy_requeue_outcome = None;

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
            .with_ansi(false)
            .without_time()
            .with_writer(CapturingWriter(buffer.clone()))
            .finish();
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);

        handle_delivery_epilogue(
            DeliveryEpilogueMessage::PostCommit,
            DeliveryEpilogueContext {
                shared_owned: &shared,
                gateway: &gateway,
                provider: &provider,
                channel_id,
                user_msg_id: None,
                current_msg_id,
                adk_session_key: &adk_session_key,
                adk_cwd: &adk_cwd,
                dispatch_id: &dispatch_id,
                turn_id: &turn_id,
                user_text_owned: &user_text,
                full_response: &full_response,
                delivery_response: &delivery_response,
                spoken_delivery_response: &spoken_delivery_response,
                cancelled: false,
                is_prompt_too_long: false,
                transport_error: false,
                recovery_retry: false,
                resume_failure_detected: false,
                claude_tui_followup_pre_submit_requeue_candidate: false,
                claude_tui_busy_requeue_pending: false,
                tui_error_classification: TuiErrorClassification::default(),
                #[cfg(unix)]
                bridge_tui_gate_outcome_early: Some(
                    crate::services::discord::tmux::TuiCompletionGateOutcome::NotGated,
                ),
                terminal_delivery_committed: true,
                terminal_body_visible: true,
                preserve_inflight_for_cleanup_retry: false,
                should_complete_work_dispatch_after_delivery: false,
                should_fail_dispatch_after_delivery: false,
                bridge_relay_delegated_to_watcher: false,
                watcher_owner_channel_id: channel_id,
                can_chain_locally: false,
                inflight_generation: 0,
            },
            DeliveryEpilogueState {
                response_sent_offset: &mut response_sent_offset,
                inflight_state: &mut stale,
                terminal_full_replay_cleanup_msg_ids: &mut terminal_full_replay_cleanup_msg_ids,
                bridge_should_emit_completion: &mut bridge_should_emit_completion,
                status_panel_terminal_committed: &mut status_panel_terminal_committed,
                busy_requeue_outcome: &mut busy_requeue_outcome,
            },
        )
        .await;

        let logs = String::from_utf8(buffer.lock().expect("captured logs lock").clone())
            .expect("captured logs must be UTF-8");
        assert_eq!(response_sent_offset, full_response.len());
        assert!(
            logs.contains(
                "turn bridge delivered the terminal answer but could not mirror terminal_delivery_committed"
            ),
            "the production epilogue must route an identity-mismatch outcome to WARN; logs={logs}"
        );
    }
}
