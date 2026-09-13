use std::collections::VecDeque;
use std::sync::Arc;

use super::super::bridge_latency_spans::BridgeLatencySpans;
use super::super::stream_tick::{
    LongRunningPlaceholderActive, PendingLongRunningOpenAfterStateSave,
    PendingLongRunningRetargetAfterStateSave,
};
use super::super::{streaming_edit_text::TuiErrorClassification, *};
use super::exit_reconcile::StreamLoopOutcome;
use crate::services::discord::inflight::CodexRange;

#[cfg(test)]
pub(crate) mod terminal_prepare_test {
    use super::*;
    #[derive(Default)]
    pub(crate) struct TerminalPrepareTestHook {
        pub(crate) channel_id: u64,
        pub(crate) entered: tokio::sync::Notify,
        pub(crate) release: tokio::sync::Notify,
        pub(crate) after_revalidation: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }
    pub(crate) static TERMINAL_PREPARE_TEST_HOOK: std::sync::Mutex<
        Option<Arc<TerminalPrepareTestHook>>,
    > = std::sync::Mutex::new(None);

    pub(crate) fn for_channel(channel: ChannelId) -> Option<Arc<TerminalPrepareTestHook>> {
        TERMINAL_PREPARE_TEST_HOOK
            .lock()
            .unwrap()
            .as_ref()
            .filter(|hook| hook.channel_id == channel.get())
            .cloned()
    }
}

#[cfg(unix)]
pub(in super::super) struct PinnedTerminalTransport<'a> {
    pub(in super::super) source: (&'a SharedData, &'a dyn TurnGateway, &'a ProviderKind),
    pub(in super::super) target: (ChannelId, ChannelId, MessageId),
    pub(in super::super) payload: (Option<&'a str>, &'a str, (u64, u64)),
    pub(in super::super) trace: (Option<&'a str>, Option<&'a str>, Option<&'a str>),
}

#[cfg(unix)]
impl PinnedTerminalTransport<'_> {
    pub(in super::super) async fn deliver(
        &self,
        pinned: terminal_delivery::PinnedBridgeDeliveryLease,
        long: bool,
    ) -> (bool, bool, Option<MessageId>) {
        use crate::services::discord::formatting::ReplaceLongMessageOutcome as Replace;
        use LeaseOutcome::{NotDelivered, Unknown};
        use terminal_controller_cutover as cutover;
        use terminal_delivery::PinnedBridgeCommit::*;
        let (shared, gateway, provider) = self.source;
        let (owner, channel, msg) = self.target;
        let (tmux, body, range) = self.payload;
        let settle =
            cutover::begin_pinned_terminal(shared, provider, long, (owner, channel), range);
        let (message_id, failed, fallback) = if long {
            match send_ordered_long_terminal_response(
                shared,
                gateway,
                provider,
                channel,
                msg,
                tmux,
                body,
                self.trace.0,
                self.trace.1,
                self.trace.2,
            )
            .await
            {
                Ok((_, tail)) => (tail, false, false),
                Err(_) => (None, true, false),
            }
        } else {
            let result = gateway
                .replace_message_with_outcome(channel, msg, body)
                .await;
            match result {
                Ok(Replace::EditedOriginal) => (Some(msg), false, false),
                Ok(Replace::SentFallbackAfterEditFailure {
                    replacement_anchor, ..
                }) => (replacement_anchor, false, true),
                result => (None, result.is_err(), false),
            }
        };
        let committed = if let Some(message_id) = message_id {
            let result = match pinned.commit_after_send(shared, message_id.get()) {
                Pending(lease) => lease.commit_after_send(shared, message_id.get()),
                result => result,
            };
            match result {
                Current | Historical => true,
                Pending(lease) => {
                    lease.release_without_receipt(Unknown);
                    false
                }
                Rejected => false,
            }
        } else {
            pinned.release_without_receipt(failed.then_some(NotDelivered).unwrap_or(Unknown));
            false
        };
        let receipt = committed.then_some(message_id).flatten();
        settle(receipt, committed);
        (committed, fallback, receipt)
    }
}

#[cfg(unix)]
pub(in crate::services::discord) async fn publish_retained_terminal_recovery(
    shared: &SharedData,
    gateway: &dyn TurnGateway,
    row: &InflightTurnState,
    text: &str,
) -> bool {
    use crate::services::discord::{inflight, outbound::delivery_record as dr};
    let Some(admitted) = inflight::CodexRange::from_retained_tui_terminal(row) else {
        return false;
    };
    let Some(provider) = row.provider_kind() else {
        return false;
    };
    let owner = ChannelId::new(admitted.source.offset_authority_channel_id);
    let channel = ChannelId::new(admitted.source.delivery_channel_id);
    let Some(message) = inflight::opt_message_id(row.current_msg_id) else {
        return false;
    };
    let acquired = terminal_delivery::bridge_delivery_lease_for_inflight(
        shared,
        owner,
        shared.restart.current_generation,
        row,
        Some(admitted.complete_record_end()),
    );
    let PreparedBridgeLease::Pinned(pinned) =
        prepare_bridge_lease(acquired, Some(&admitted), row, shared, &provider, channel)
    else {
        return false;
    };
    // The source proof covers the full admitted body; recovery has already
    // formatted only the suffix not represented by frozen Discord prefixes.
    let long = terminal_delivery::terminal_delivery_should_send_new_chunks(true, text);
    let (committed, _, receipt) = PinnedTerminalTransport {
        source: (shared, gateway, &provider),
        target: (owner, channel, message),
        payload: (
            row.tmux_session_name.as_deref(),
            text,
            admitted.source.range,
        ),
        trace: (row.dispatch_id.as_deref(), row.session_key.as_deref(), None),
    }
    .deliver(pinned, long)
    .await;
    committed
        && receipt.is_some_and(|anchor| {
            dr::confirmed_delivery_receipt_exists(
                &provider,
                channel,
                anchor.get(),
                &admitted.source,
            )
        })
}

#[cfg(unix)]
#[rustfmt::skip]
pub(in super::super) fn persist_pinned_terminal_anchor(
    state: &mut InflightTurnState,
    anchor: Option<MessageId>,
) -> bool {
    let Some((anchor, provider)) = anchor.zip(state.provider_kind()) else {
        return false;
    };
    let expected = crate::services::discord::inflight::InflightTurnIdentity::from_state(state);
    let authority = crate::services::discord::inflight::StreamRelayAuthority::from_state(state);
    let (channel, current, len) = (state.channel_id, state.current_msg_id, state.current_msg_len);
    crate::services::discord::inflight::bind_recovery_anchor_if_matches_identity(
        &provider, channel, &expected, state.turn_start_offset, current, Some(len), anchor.get(), len,
        Some(authority), Some(state),
    ) == crate::services::discord::inflight::GuardedSaveOutcome::Saved
}

pub(in crate::services::discord::turn_bridge) enum PreparedBridgeLease {
    Legacy(BridgeLeaseAcquire),
    Unresolved,
    #[cfg(unix)]
    Pinned(super::super::terminal_delivery::PinnedBridgeDeliveryLease),
}

pub(in crate::services::discord::turn_bridge) fn prepare_bridge_lease(
    acquire: BridgeLeaseAcquire,
    admitted: Option<&CodexRange>,
    inflight: &InflightTurnState,
    shared: &SharedData,
    provider: &ProviderKind,
    delivery_channel: ChannelId,
) -> PreparedBridgeLease {
    use BridgeLeaseAcquire::{Held, NoRange, Skip};
    let Some(admitted) = admitted else {
        return PreparedBridgeLease::Legacy(acquire);
    };
    if matches!(acquire, Skip) {
        return PreparedBridgeLease::Legacy(acquire);
    }
    // A positively admitted Claude terminal may publish only with its exact
    // source. Losing that proof does not restore legacy publication authority.
    #[cfg(unix)]
    let unavailable = || {
        if matches!(provider, ProviderKind::Claude) {
            PreparedBridgeLease::Unresolved
        } else {
            PreparedBridgeLease::Legacy(NoRange)
        }
    };
    #[cfg(not(unix))]
    return PreparedBridgeLease::Legacy(acquire);
    #[cfg(unix)]
    match admitted.revalidated_source(inflight) {
        // #5264: Codex I/O failure keeps the real acquire result; inventing a
        // Skip here would claim another holder owns a lease we just released.
        Err(()) if !matches!(provider, ProviderKind::Claude) => {
            PreparedBridgeLease::Legacy(acquire)
        }
        Err(()) | Ok(None) => unavailable(),
        Ok(Some(source)) => match acquire {
            // Pin consumes the lease, including on failure. Only Codex retains
            // legacy retry permission after that release; Claude needs its proof.
            Held(lease) => {
                #[cfg(test)]
                if let Some(hook) = terminal_prepare_test::for_channel(delivery_channel) {
                    if let Some(mutate) = hook.after_revalidation.lock().unwrap().take() {
                        mutate();
                    }
                }
                lease
                    .pin_exact_source(shared, provider, delivery_channel, source)
                    .map_or_else(unavailable, PreparedBridgeLease::Pinned)
            }
            NoRange => unavailable(),
            Skip => unreachable!(),
        },
    }
}

macro_rules! dispatch_pinned_terminal {
    ($shared:ident $gateway:ident $provider:ident $owner:ident $inflight:ident $end:ident $admitted:ident $channel:ident $message:ident $body:ident $start:ident $dispatch:ident $session:ident $turn:ident $long:ident $full:ident $footer_mode:ident $committed:ident $visible:ident $sent:ident $footer:ident $preserve:ident $skip_owner:ident $handled:ident $outcome:ident) => {{
        if $admitted.is_some() {
            #[cfg(test)]
            if let Some(hook) = $crate::services::discord::turn_bridge::stream_loop::types::terminal_prepare_test::for_channel($channel) {
                hook.entered.notify_one();
                hook.release.notified().await;
            }
            let prepared = $crate::services::discord::turn_bridge::stream_loop::types::prepare_bridge_lease(
                bridge_delivery_lease_for_inflight(
                    $shared.as_ref(),
                    $owner,
                    $shared.restart.current_generation,
                    &$inflight,
                    $end,
                ),
                $admitted,
                &$inflight,
                $shared.as_ref(),
                &$provider,
                $channel,
            );
            match prepared {
                $crate::services::discord::turn_bridge::stream_loop::types::PreparedBridgeLease::Unresolved => {
                    $preserve = true;
                    $outcome = $crate::services::discord::turn_bridge::terminal_outcome_delivery::TerminalOutcomeDeliveryOutcome::Unresolved {
                        error: "admitted Claude terminal lost its verified source or delivery lease".into(),
                    };
                    $handled = true;
                }
                $crate::services::discord::turn_bridge::stream_loop::types::PreparedBridgeLease::Pinned(pinned) => {
                    let transport = PinnedTerminalTransport {
                        source: ($shared.as_ref(), $gateway.as_ref(), &$provider),
                        target: ($owner, $channel, $message),
                        payload: (
                            $inflight.tmux_session_name.as_deref(),
                            &$body,
                            ($start, $end.unwrap_or(0)),
                        ),
                        trace: (
                            $dispatch.as_deref(),
                            $session.as_deref(),
                            Some($turn.as_str()),
                        ),
                    };
                    let (mut did_commit, fallback, anchor) = transport.deliver(pinned, $long).await;
                    if $long && did_commit {
                        did_commit = $crate::services::discord::turn_bridge::stream_loop::types::persist_pinned_terminal_anchor(&mut $inflight, anchor);
                    }
                    if $long {
                        if did_commit {
                            ($committed, $visible) = (true, true);
                            $sent = $full.len();
                            $inflight.response_sent_offset = $full.len();
                            if $footer_mode {
                                $footer = Some($body.clone());
                            }
                        } else {
                            $preserve = true;
                        }
                    } else {
                        let outcome = if did_commit {
                            $crate::services::discord::outbound::turn_output_controller::DeliveryOutcome::Delivered {
                                committed_to: $end.unwrap_or(0),
                                replace_kind: fallback.then(|| $crate::services::discord::outbound::turn_output_controller::ReplaceDeliveryKind::FreshFallbackAfterEditFailure {
                                    edit_error: "fallback after edit failure".into(), replacement_anchor: anchor,
                                }), new_chunks: None,
                            }
                        } else {
                            $crate::services::discord::outbound::turn_output_controller::DeliveryOutcome::Unknown { fell_back: fallback }
                        };
                        terminal_controller_cutover::apply_bridge_short_replace_outcome(
                            outcome,
                            $shared.as_ref(),
                            &$provider,
                            $channel,
                            $message,
                            $inflight.tmux_session_name.as_deref(),
                            &$body,
                            $full.len(),
                            $footer_mode,
                            $dispatch.as_deref(),
                            $session.as_deref(),
                            Some($turn.as_str()),
                            terminal_controller_cutover::BridgeShortReplaceLocals {
                                terminal_delivery_committed: &mut $committed,
                                terminal_body_visible: &mut $visible,
                                completion_footer_terminal_text: &mut $footer,
                                preserve_inflight_for_cleanup_retry: &mut $preserve,
                                bridge_skip_holder_owns_inflight: &mut $skip_owner,
                                inflight_response_sent_offset: &mut $inflight
                                    .response_sent_offset,
                            },
                        );
                    }
                    $handled = true;
                }
                $crate::services::discord::turn_bridge::stream_loop::types::PreparedBridgeLease::Legacy(BridgeLeaseAcquire::Skip) => {
                    $preserve = true;
                    $skip_owner = true;
                    $handled = true;
                }
                // The pinned end is not consumed after this match, and the legacy fallback
                // deliberately reads the separate exclusion-lease end, so there is nothing
                // to clear here. Nulling `$end` was what collapsed a non-admitted CodexTui
                // turn's lease range to `NoRange`.
                $crate::services::discord::turn_bridge::stream_loop::types::PreparedBridgeLease::Legacy(_) => {}
            }
        }
    }};
}
pub(in crate::services::discord::turn_bridge) use dispatch_pinned_terminal;

pub(in crate::services::discord::turn_bridge) struct StreamLoopContext {
    pub(in crate::services::discord::turn_bridge) shared_owned: Arc<SharedData>,
    pub(in crate::services::discord::turn_bridge) gateway: Arc<dyn TurnGateway>,
    pub(in crate::services::discord::turn_bridge) channel_id: ChannelId,
    pub(in crate::services::discord::turn_bridge) provider: ProviderKind,
    pub(in crate::services::discord::turn_bridge) cancel_token:
        Arc<crate::services::provider::CancelToken>,
    pub(in crate::services::discord::turn_bridge) user_text_owned: String,
    pub(in crate::services::discord::turn_bridge) request_owner_name: String,
    pub(in crate::services::discord::turn_bridge) adk_session_key: Option<String>,
    pub(in crate::services::discord::turn_bridge) adk_session_name: Option<String>,
    pub(in crate::services::discord::turn_bridge) adk_session_info: Option<String>,
    pub(in crate::services::discord::turn_bridge) adk_cwd: Option<String>,
    pub(in crate::services::discord::turn_bridge) dispatch_id: Option<String>,
    pub(in crate::services::discord::turn_bridge) role_binding: Option<RoleBinding>,
    pub(in crate::services::discord::turn_bridge) turn_id: String,
    pub(in crate::services::discord::turn_bridge) voice_progress_playback_channel_id:
        Option<ChannelId>,
    pub(in crate::services::discord::turn_bridge) single_message_panel_footer_mode: bool,
    pub(in crate::services::discord::turn_bridge) footer_owner:
        crate::services::discord::footer_view_reconciler::CompletionFooterOwner,
    pub(in crate::services::discord::turn_bridge) status_panel_started_at: i64,
    pub(in crate::services::discord::turn_bridge) status_interval: std::time::Duration,
    pub(in crate::services::discord::turn_bridge) context_window_tokens: u64,
    pub(in crate::services::discord::turn_bridge) context_compact_percent: u64,
}

pub(in crate::services::discord::turn_bridge) struct StreamLoopState<'a> {
    pub(in crate::services::discord::turn_bridge) rx: &'a mut StreamMessageReceiverAdapter,
    pub(in crate::services::discord::turn_bridge) full_response: &'a mut String,
    pub(in crate::services::discord::turn_bridge) last_edit_text: &'a mut String,
    pub(in crate::services::discord::turn_bridge) done: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) cancelled: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) rx_disconnected: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) current_tool_line: &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge) prev_tool_status: &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge) last_tool_name: &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge) last_tool_summary: &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge) accumulated_input_tokens: &'a mut u64,
    pub(in crate::services::discord::turn_bridge) accumulated_cache_create_tokens: &'a mut u64,
    pub(in crate::services::discord::turn_bridge) accumulated_cache_read_tokens: &'a mut u64,
    pub(in crate::services::discord::turn_bridge) accumulated_output_tokens: &'a mut u64,
    pub(in crate::services::discord::turn_bridge) spin_idx: &'a mut usize,
    pub(in crate::services::discord::turn_bridge) restart_followup_pending:
        &'a mut Option<RestartAttempt>,
    pub(in crate::services::discord::turn_bridge) any_tool_used: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) has_post_tool_text: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) tmux_handed_off: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) watcher_owns_assistant_relay: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) watcher_relay_available_for_turn: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) watcher_delivery_pin:
        &'a mut Option<WatcherClaimIncarnation>,
    pub(in crate::services::discord::turn_bridge) watcher_handoff_claim_outcome:
        &'a mut WatcherHandoffClaimOutcome,
    pub(in crate::services::discord::turn_bridge) standby_relay_owns_output: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) last_assistant_text_line: &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge) long_running_placeholder_active:
        &'a mut LongRunningPlaceholderActive,
    pub(in crate::services::discord::turn_bridge) active_background_child_session_ids:
        &'a mut Vec<i64>,
    pub(in crate::services::discord::turn_bridge) transport_error: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) transcript_events:
        &'a mut Vec<SessionTranscriptEvent>,
    pub(in crate::services::discord::turn_bridge) resume_failure_detected: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) session_handshake_seen: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) terminal_session_reset_required: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) recovery_retry: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) last_adk_heartbeat: &'a mut std::time::Instant,
    pub(in crate::services::discord::turn_bridge) pending_stream_messages:
        &'a mut VecDeque<StreamMessage>,
    pub(in crate::services::discord::turn_bridge) pending_status_tool_results:
        &'a mut VecDeque<String>,
    pub(in crate::services::discord::turn_bridge) pending_status_tool_results_by_id:
        &'a mut std::collections::HashMap<String, String>,
    pub(in crate::services::discord::turn_bridge) last_inflight_long_run_heartbeat:
        &'a mut std::time::Instant,
    pub(in crate::services::discord::turn_bridge) last_activity_heartbeat_at:
        &'a mut Option<std::time::Instant>,
    pub(in crate::services::discord::turn_bridge) terminal_control_ready_observed: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) terminal_control_drain_until:
        &'a mut Option<std::time::Instant>,
    pub(in crate::services::discord::turn_bridge) current_msg_id: &'a mut MessageId,
    pub(in crate::services::discord::turn_bridge) expected_current_message: &'a mut (u64, usize),
    pub(in crate::services::discord::turn_bridge) bridge_created_response_placeholder_msg_id:
        &'a mut Option<MessageId>,
    pub(in crate::services::discord::turn_bridge) response_sent_offset: &'a mut usize,
    pub(in crate::services::discord::turn_bridge) bridge_confirmed_response_sent_offset:
        &'a mut usize,
    pub(in crate::services::discord::turn_bridge) streamed_assistant_text_this_turn: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) streaming_rollover_frozen_msg_ids:
        &'a mut Vec<MessageId>,
    pub(in crate::services::discord::turn_bridge) terminal_full_replay_cleanup_msg_ids:
        &'a mut Vec<MessageId>,
    pub(in crate::services::discord::turn_bridge) tmux_last_offset: &'a mut Option<u64>,
    pub(in crate::services::discord::turn_bridge) watcher_owner_channel_id: &'a mut ChannelId,
    pub(in crate::services::discord::turn_bridge) new_session_id: &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge) new_raw_provider_session_id:
        &'a mut Option<String>,
    pub(in crate::services::discord::turn_bridge) inflight_state: &'a mut InflightTurnState,
    pub(in crate::services::discord::turn_bridge) last_status_edit: &'a mut tokio::time::Instant,
    pub(in crate::services::discord::turn_bridge) first_answer_relayed: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) last_session_panel_lifecycle_refresh:
        &'a mut tokio::time::Instant,
    pub(in crate::services::discord::turn_bridge) status_panel_msg_id: &'a mut Option<MessageId>,
    pub(in crate::services::discord::turn_bridge) last_status_panel_text: &'a mut String,
    pub(in crate::services::discord::turn_bridge) status_panel_dirty: &'a mut bool,
    pub(in crate::services::discord::turn_bridge) last_status_panel_edit:
        &'a mut tokio::time::Instant,
    pub(in crate::services::discord::turn_bridge) bridge_spans: &'a mut BridgeLatencySpans,
    pub(in crate::services::discord::turn_bridge) status_panel_generation: &'a mut u64,
    pub(in crate::services::discord::turn_bridge) entry_watcher_epoch_current: &'a mut bool,
}

pub(in crate::services::discord::turn_bridge) struct StreamLoopOutput {
    pub(in crate::services::discord::turn_bridge) outcome: StreamLoopOutcome,
    pub(in crate::services::discord::turn_bridge) tui_error_classification: TuiErrorClassification,
    pub(in crate::services::discord::turn_bridge) codex_tui_terminal_range: Option<CodexRange>,
    pub(in crate::services::discord::turn_bridge) pending_long_running_open_after_state_save:
        PendingLongRunningOpenAfterStateSave,
    pub(in crate::services::discord::turn_bridge) pending_long_running_retarget_after_state_save:
        PendingLongRunningRetargetAfterStateSave,
}
