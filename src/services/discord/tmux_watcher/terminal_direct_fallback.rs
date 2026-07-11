//! #4229 S8: watcher direct-terminal fallback send arm, extracted verbatim from
//! the `watcher_direct_fallback_after_session_bound_ack` branch of the emission
//! match in `tmux_watcher.rs` (behavior-preserving decompose). Covers the
//! long-chunks controller/legacy, short-replace controller/legacy edit+fallback+
//! partial, placeholderless fresh-send, empty-cleanup, and the relay-success
//! watermark/idle-commit epilogue. The `relay_ok` value is returned to the parent
//! arm unchanged.

use std::sync::Arc;

use super::*;

use crate::services::agent_protocol::TaskNotificationKind;
use crate::services::discord::inflight::{InflightTurnIdentity, InflightTurnState};
use crate::services::discord::task_notification_delivery as task_delivery;
use crate::services::discord::turn_finalizer::TurnKey;
use crate::services::discord::{DeliveryLeaseCell, SharedData};
use crate::services::provider::ProviderKind;

pub(in crate::services::discord) struct WatcherDirectFallbackLocals<'a> {
    pub(in crate::services::discord) tui_direct_anchor_terminal_body_visible: &'a mut bool,
    pub(in crate::services::discord) placeholder_msg_id: &'a mut Option<MessageId>,
    pub(in crate::services::discord) placeholder_from_restored_inflight: &'a mut bool,
    pub(in crate::services::discord) last_edit_text: &'a mut String,
    pub(in crate::services::discord) watcher_streaming_rollover_frozen_msg_ids:
        &'a mut Vec<MessageId>,
    pub(in crate::services::discord) watcher_long_chunk_anchor_msg_id: &'a mut Option<MessageId>,
    pub(in crate::services::discord) watcher_long_chunk_delivered_body: &'a mut Option<String>,
    pub(in crate::services::discord) completion_footer_terminal_target:
        &'a mut Option<WatcherCompletionFooterTerminalTarget>,
    pub(in crate::services::discord) retry_terminal_delivery_from_offset: &'a mut bool,
    pub(in crate::services::discord) tui_direct_anchor_or_lease_present_for_lifecycle: &'a mut bool,
    pub(in crate::services::discord) watcher_direct_terminal_idle_committed: &'a mut bool,
    pub(in crate::services::discord) last_relayed_offset: &'a mut Option<u64>,
    pub(in crate::services::discord) last_observed_generation_mtime_ns: &'a mut Option<i64>,
    pub(in crate::services::discord) task_response_claim:
        &'a mut Option<task_delivery::ResponseDeliveryClaim>,
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn apply_watcher_direct_fallback_send(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    watcher_provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    direct_terminal_response: &str,
    should_tag_monitor_origin: bool,
    data_start_offset: u64,
    current_offset: u64,
    turn_data_start_offset: u64,
    response_sent_offset: usize,
    task_notification_kind: Option<TaskNotificationKind>,
    task_notification_context: Option<&task_delivery::TaskNotificationContext>,
    terminal_kind: Option<WatcherTerminalKind>,
    has_direct_terminal_response: bool,
    session_bound_fallback_uses_full_body: bool,
    cutover_short_replace: bool,
    single_message_panel_footer_mode: bool,
    watcher_lease_cell: &Arc<DeliveryLeaseCell>,
    watcher_lease_turn: TurnKey,
    watcher_lease_key: &crate::services::discord::DeliveryLeaseKey,
    watcher_instance_id: u64,
    watcher_lease_start: u64,
    watcher_lease_end: u64,
    inflight_before_relay: &Option<InflightTurnState>,
    inflight_identity_before_relay: &Option<InflightTurnIdentity>,
    external_input_lease_before_relay: bool,
    external_input_lease_generation_before_relay: Option<u64>,
    prompt_anchor_present_before_relay: bool,
    ssh_direct_pending: bool,
    locals: WatcherDirectFallbackLocals<'_>,
) -> bool {
    let WatcherDirectFallbackLocals {
        tui_direct_anchor_terminal_body_visible,
        placeholder_msg_id,
        placeholder_from_restored_inflight,
        last_edit_text,
        watcher_streaming_rollover_frozen_msg_ids,
        watcher_long_chunk_anchor_msg_id,
        watcher_long_chunk_delivered_body,
        completion_footer_terminal_target,
        retry_terminal_delivery_from_offset,
        tui_direct_anchor_or_lease_present_for_lifecycle,
        watcher_direct_terminal_idle_committed,
        last_relayed_offset,
        last_observed_generation_mtime_ns,
        task_response_claim,
    } = locals;
    let (user_msg_id, started_at, turn_start_offset) = inflight_identity_before_relay
        .as_ref()
        .map_or((0, "", None), |identity| {
            (
                identity.user_msg_id,
                identity.started_at.as_str(),
                identity.turn_start_offset,
            )
        });
    let response_turn_key = task_delivery::durable_response_turn_key(
        channel_id.get(),
        watcher_provider.as_str(),
        tmux_session_name,
        user_msg_id,
        started_at,
        turn_start_offset,
        watcher_lease_end,
        direct_terminal_response,
    );
    let formatted = if shared.ui.status_panel_v2_enabled {
        crate::services::discord::formatting::format_for_discord_with_status_panel(
            direct_terminal_response,
            &watcher_provider,
        )
    } else {
        crate::services::discord::formatting::format_for_discord_with_provider(
            direct_terminal_response,
            &watcher_provider,
        )
    };
    let relay_text = if should_tag_monitor_origin {
        crate::services::discord::prepend_monitor_auto_turn_origin(&formatted)
    } else {
        formatted
    };
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 👁 Relaying terminal response to Discord ({} chars, offset {}, task_notification_kind={})",
        relay_text.len(),
        data_start_offset,
        task_notification_kind
            .map(TaskNotificationKind::as_str)
            .unwrap_or("none")
    );
    let mut relay_ok = true;
    let mut direct_send_delivered = false;
    let mut external_input_lease_consumed_by_relay = false;
    let task_response_path = has_direct_terminal_response && task_notification_kind.is_some();
    match (task_response_path, *placeholder_msg_id) {
        (true, _) => {
            let kind = task_notification_kind.expect("task response path requires a kind");
            let outcome = super::task_response_authority::apply_watcher_task_response(
                http,
                shared,
                watcher_provider,
                channel_id,
                tmux_session_name,
                kind,
                task_notification_context,
                &response_turn_key,
                (!started_at.is_empty()).then_some(started_at),
                turn_start_offset,
                watcher_lease_end,
                &relay_text,
                external_input_lease_before_relay,
                super::task_response_authority::WatcherTaskResponseLocals {
                    placeholder_msg_id: &mut *placeholder_msg_id,
                    placeholder_from_restored_inflight: &mut *placeholder_from_restored_inflight,
                    last_edit_text: &mut *last_edit_text,
                    retry_terminal_delivery_from_offset: &mut *retry_terminal_delivery_from_offset,
                    tui_direct_anchor_terminal_body_visible:
                        &mut *tui_direct_anchor_terminal_body_visible,
                    tui_direct_anchor_or_lease_present_for_lifecycle:
                        &mut *tui_direct_anchor_or_lease_present_for_lifecycle,
                    task_response_claim: &mut *task_response_claim,
                },
            )
            .await;
            relay_ok = outcome.relay_ok;
            direct_send_delivered = outcome.direct_send_delivered;
            external_input_lease_consumed_by_relay = outcome.external_input_lease_consumed_by_relay;
        }
        (false, Some(msg_id)) => {
            if has_direct_terminal_response {
                if watcher_should_send_ordered_new_chunks_for_terminal_fallback(
                    session_bound_fallback_uses_full_body,
                    &relay_text,
                ) {
                    if cutover_short_replace {
                        terminal_long_chunks::apply_watcher_long_chunks_controller(
                            &http,
                            &shared,
                            &watcher_provider,
                            channel_id,
                            &tmux_session_name,
                            msg_id,
                            &relay_text,
                            direct_terminal_response,
                            &watcher_lease_cell,
                            watcher_lease_turn,
                            Some(watcher_lease_key.clone()),
                            watcher_instance_id,
                            (watcher_lease_start, watcher_lease_end),
                            session_bound_fallback_uses_full_body,
                            &mut *watcher_streaming_rollover_frozen_msg_ids,
                            inflight_before_relay.as_ref(),
                            terminal_long_chunks::WatcherLongChunksLocals {
                                relay_ok: &mut relay_ok,
                                direct_send_delivered: &mut direct_send_delivered,
                                tui_direct_anchor_terminal_body_visible:
                                    &mut *tui_direct_anchor_terminal_body_visible,
                                external_input_lease_consumed_by_relay:
                                    &mut external_input_lease_consumed_by_relay,
                                placeholder_msg_id: &mut *placeholder_msg_id,
                                placeholder_from_restored_inflight:
                                    &mut *placeholder_from_restored_inflight,
                                last_edit_text: &mut *last_edit_text,
                            },
                        )
                        .await;
                    } else {
                        let delivered_long_chunk_body = direct_terminal_response.to_string();
                        terminal_long_chunks::apply_watcher_long_chunks_legacy(
                            &http,
                            &shared,
                            &watcher_provider,
                            channel_id,
                            &tmux_session_name,
                            msg_id,
                            &relay_text,
                            session_bound_fallback_uses_full_body,
                            &mut *watcher_streaming_rollover_frozen_msg_ids,
                            inflight_before_relay.as_ref(),
                            &mut *watcher_long_chunk_anchor_msg_id,
                            terminal_long_chunks::WatcherLongChunksLocals {
                                relay_ok: &mut relay_ok,
                                direct_send_delivered: &mut direct_send_delivered,
                                tui_direct_anchor_terminal_body_visible:
                                    &mut *tui_direct_anchor_terminal_body_visible,
                                external_input_lease_consumed_by_relay:
                                    &mut external_input_lease_consumed_by_relay,
                                placeholder_msg_id: &mut *placeholder_msg_id,
                                placeholder_from_restored_inflight:
                                    &mut *placeholder_from_restored_inflight,
                                last_edit_text: &mut *last_edit_text,
                            },
                        )
                        .await;
                        if watcher_long_chunk_anchor_msg_id.is_some() {
                            *watcher_long_chunk_delivered_body = Some(delivered_long_chunk_body);
                        }
                    }
                } else if cutover_short_replace {
                    // #3089 A4: route short-replace through the unified controller
                    // (flag ON) — see `apply_watcher_short_replace_controller`. The
                    // CONTROLLER owns the SINGLE `LeaseHolder::Watcher` lease (the
                    // watcher's own acquire/heartbeat/commit/advance/release were
                    // skipped at the acquire site). #2757 PreserveAlways is honoured;
                    // the rare `SentFallbackAfterEditFailure` sub-case mirrors the
                    // legacy fallback arm (NO footer target, `Failed(edit_error)`
                    // cleanup, original preserved) via the controller-surfaced
                    // `ReplaceDeliveryKind` (#3089 A4 r2, codex r1 [High]).
                    terminal_send::apply_watcher_short_replace_controller(
                        &http,
                        &shared,
                        &watcher_provider,
                        channel_id,
                        &tmux_session_name,
                        msg_id,
                        &relay_text,
                        direct_terminal_response,
                        &watcher_lease_cell,
                        watcher_lease_turn,
                        Some(watcher_lease_key.clone()),
                        watcher_instance_id,
                        (watcher_lease_start, watcher_lease_end),
                        single_message_panel_footer_mode,
                        inflight_before_relay.as_ref(),
                        terminal_send::WatcherShortReplaceLocals {
                            relay_ok: &mut relay_ok,
                            direct_send_delivered: &mut direct_send_delivered,
                            tui_direct_anchor_terminal_body_visible:
                                &mut *tui_direct_anchor_terminal_body_visible,
                            external_input_lease_consumed_by_relay:
                                &mut external_input_lease_consumed_by_relay,
                            placeholder_msg_id: &mut *placeholder_msg_id,
                            placeholder_from_restored_inflight:
                                &mut *placeholder_from_restored_inflight,
                            last_edit_text: &mut *last_edit_text,
                            completion_footer_terminal_target:
                                &mut *completion_footer_terminal_target,
                            retry_terminal_delivery_from_offset:
                                &mut *retry_terminal_delivery_from_offset,
                        },
                    )
                    .await;
                } else {
                    // #3805 P1: capture the tail continuation chunk (id +
                    // its own text) so the completion footer re-anchors onto
                    // it instead of stranding on the edited chunk 0.
                    let mut last_chunk_anchor = None;
                    match replace_long_message_raw_with_outcome(
                        &http,
                        channel_id,
                        msg_id,
                        &relay_text,
                        &shared,
                        &mut last_chunk_anchor,
                    )
                    .await
                    {
                        Ok(ReplaceLongMessageOutcome::EditedOriginal) => {
                            direct_send_delivered = true;
                            *tui_direct_anchor_terminal_body_visible = true;
                            external_input_lease_consumed_by_relay =
                                watcher_inflight_represents_external_input(
                                    inflight_before_relay.as_ref(),
                                );
                            // #3805 P1: re-anchor the completion footer to the
                            // LAST continuation chunk with the tail chunk's OWN
                            // text (single-chunk ⇒ chunk 0 + full body).
                            let (footer_target_msg_id, footer_target_text) =
                                        crate::services::discord::formatting::watcher_completion_footer_anchor(
                                            last_chunk_anchor.as_ref(),
                                            msg_id,
                                            &relay_text,
                                        );
                            remember_watcher_completion_footer_terminal_target(
                                single_message_panel_footer_mode,
                                &mut *completion_footer_terminal_target,
                                footer_target_msg_id,
                                footer_target_text,
                            );
                            *placeholder_msg_id = None;
                            *placeholder_from_restored_inflight = false;
                            last_edit_text.clear();
                            // #3351 r21 mirror: edited into the final response —
                            // a stale record must not let a drain delete it.
                            drop_placeholder_orphan_record(
                                &watcher_provider,
                                &shared,
                                channel_id,
                                msg_id,
                            );
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!(
                                "  [{ts}] 👁 ✓ relayed terminal response (edit) channel {} msg {} ({} chars)",
                                channel_id.get(),
                                msg_id.get(),
                                relay_text.len()
                            );
                            record_placeholder_cleanup(
                                &shared,
                                &watcher_provider,
                                channel_id,
                                msg_id,
                                &tmux_session_name,
                                PlaceholderCleanupOperation::EditTerminal,
                                PlaceholderCleanupOutcome::Succeeded,
                                "watcher_terminal_relay",
                            );
                        }
                        Ok(ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                            edit_error,
                            ..
                        }) => {
                            direct_send_delivered = true;
                            *tui_direct_anchor_terminal_body_visible = true;
                            external_input_lease_consumed_by_relay =
                                watcher_inflight_represents_external_input(
                                    inflight_before_relay.as_ref(),
                                );
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!(
                                "  [{ts}] 👁 ✓ relayed terminal response (fallback send after edit failure) channel {} msg {} ({} chars, edit_error={edit_error})",
                                channel_id.get(),
                                msg_id.get(),
                                relay_text.len()
                            );
                            record_placeholder_cleanup(
                                &shared,
                                &watcher_provider,
                                channel_id,
                                msg_id,
                                &tmux_session_name,
                                PlaceholderCleanupOperation::EditTerminal,
                                PlaceholderCleanupOutcome::failed(edit_error),
                                "watcher_terminal_relay",
                            );
                            if watcher_fallback_edit_failure_can_delete_original_placeholder(
                                response_sent_offset,
                                &*last_edit_text,
                            ) {
                                let cleanup = delete_terminal_placeholder(
                                    &http,
                                    channel_id,
                                    &shared,
                                    &watcher_provider,
                                    &tmux_session_name,
                                    msg_id,
                                    "watcher_terminal_relay_fallback_cleanup",
                                )
                                .await;
                                match fallback_placeholder_cleanup_decision(&cleanup) {
                                            FallbackPlaceholderCleanupDecision::RelayCommitted => {
                                                *placeholder_msg_id = None;
                                                *placeholder_from_restored_inflight = false;
                                                last_edit_text.clear();
                                                // #3351 r21 mirror: delete committed.
                                                drop_placeholder_orphan_record(
                                                    &watcher_provider,
                                                    &shared,
                                                    channel_id,
                                                    msg_id,
                                                );
                                            }
                                            FallbackPlaceholderCleanupDecision::PreserveInflightForCleanupRetry => {
                                                relay_ok = false;
                                                *tui_direct_anchor_terminal_body_visible = false;
                                                let ts = chrono::Local::now().format("%H:%M:%S");
                                                tracing::warn!(
                                                    "  [{ts}] ⚠ watcher: terminal response was delivered via fallback send, but stale placeholder cleanup did not commit for channel {} msg {}",
                                                    channel_id.get(),
                                                    msg_id.get()
                                                );
                                            }
                                        }
                            } else {
                                *placeholder_msg_id = None;
                                *placeholder_from_restored_inflight = false;
                                last_edit_text.clear();
                                // #3351 (codex r2 #2): message intentionally preserved
                                // (#2757) — a stale record must not let a drain delete it.
                                drop_placeholder_orphan_record(
                                    &watcher_provider,
                                    &shared,
                                    channel_id,
                                    msg_id,
                                );
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::warn!(
                                    "  [{ts}] ⚠ watcher: terminal response delivered via fallback send; preserving original msg {} in channel {} because it may contain streamed response content (#2757)",
                                    msg_id.get(),
                                    channel_id.get()
                                );
                            }
                        }
                        Ok(ReplaceLongMessageOutcome::PartialContinuationFailure {
                            sent_chunks,
                            total_chunks,
                            failed_chunk_index,
                            sent_continuation_message_ids,
                            cleanup_errors,
                            error,
                        }) => {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            let display_error = stripped_send_error(&error);
                            tracing::warn!(
                                "  [{ts}] ⚠ watcher: terminal response partially delivered in channel {} msg {} (sent_chunks={}, total_chunks={}, failed_chunk_index={}, cleaned_continuations={}, cleanup_errors={}, error={}); preserving inflight for retry",
                                channel_id.get(),
                                msg_id.get(),
                                sent_chunks,
                                total_chunks,
                                failed_chunk_index,
                                sent_continuation_message_ids.len(),
                                cleanup_errors.len(),
                                display_error
                            );
                            record_placeholder_cleanup(
                                &shared,
                                &watcher_provider,
                                channel_id,
                                msg_id,
                                &tmux_session_name,
                                PlaceholderCleanupOperation::EditTerminal,
                                PlaceholderCleanupOutcome::failed(format!(
                                    "{display_error}; cleaned_continuations={}; cleanup_errors={}",
                                    sent_continuation_message_ids.len(),
                                    cleanup_errors.len()
                                )),
                                "watcher_terminal_relay_partial_continuation_failure",
                            );
                            let failure_class = watcher_partial_continuation_failure_class(
                                &error,
                                cleanup_errors.is_empty(),
                            );
                            let plan = watcher_send_failure_plan_warned(
                                failure_class,
                                WatcherNoRewindWarnSite::Partial,
                                &watcher_provider,
                                channel_id,
                                &tmux_session_name,
                                &error,
                            );
                            relay_ok = plan.relay_ok;
                            *retry_terminal_delivery_from_offset = plan.retry_offset;
                        }
                        Err(e) => {
                            info_watcher_failed_relay(e.as_ref());
                            let plan = watcher_send_failure_plan_warned(
                                classify_watcher_send_failure(e.as_ref()),
                                WatcherNoRewindWarnSite::EditFull,
                                &watcher_provider,
                                channel_id,
                                &tmux_session_name,
                                e.as_ref(),
                            );
                            relay_ok = plan.relay_ok;
                            *retry_terminal_delivery_from_offset = plan.retry_offset;
                        }
                    }
                }
            } else {
                let outcome = delete_terminal_placeholder(
                    &http,
                    channel_id,
                    &shared,
                    &watcher_provider,
                    &tmux_session_name,
                    msg_id,
                    "watcher_empty_terminal_cleanup",
                )
                .await;
                if !outcome.is_committed() {
                    relay_ok = false;
                } else {
                    *placeholder_msg_id = None;
                    *placeholder_from_restored_inflight = false;
                    last_edit_text.clear();
                }
            }
        }
        (false, None) => {
            if has_direct_terminal_response {
                let prompt_anchor = crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
                    watcher_provider.as_str(),
                    &tmux_session_name,
                    channel_id.get(),
                );
                let prompt_anchor_reference = prompt_anchor.map(|anchor| {
                    (
                        ChannelId::new(anchor.channel_id),
                        MessageId::new(anchor.message_id),
                    )
                });
                let rollback_anchor_msg_id = watcher_rollback_anchor_msg_id(
                    prompt_anchor_reference.as_ref(),
                    watcher_lease_turn.user_msg_id,
                    watcher_lease_start,
                );
                // The rollback sender makes chunk failure all-or-nothing
                // before a rewind. A timeout after Discord accepts a POST
                // is still inherently ambiguous, so classification and the
                // attempt cap below remain the backstop.
                match crate::services::discord::formatting::send_long_message_raw_with_reference_rollback(
                            &http,
                            channel_id,
                            rollback_anchor_msg_id,
                            &relay_text,
                            &shared,
                            prompt_anchor_reference,
                        )
                        .await
                        {
                            Ok(_) => {
                                *tui_direct_anchor_or_lease_present_for_lifecycle |=
                                    prompt_anchor.is_some();
                                external_input_lease_consumed_by_relay =
                                    external_input_lease_before_relay || prompt_anchor.is_some();
                                direct_send_delivered = true;
                                *tui_direct_anchor_terminal_body_visible = true;
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] 👁 ✓ relayed terminal response (new message) channel {} ({} chars, prompt_anchor_message_id={:?})",
                                    channel_id.get(),
                                    relay_text.len(),
                                    prompt_anchor_reference.map(|(_, message_id)| message_id.get())
                                );
                            }
                            Err(e) => {
                                info_watcher_failed_relay(e.as_ref());
                                let plan = watcher_send_failure_plan_warned(
                                    classify_watcher_send_failure(e.as_ref()),
                                    WatcherNoRewindWarnSite::PlaceholderlessFull,
                                    &watcher_provider,
                                    channel_id,
                                    &tmux_session_name,
                                    e.as_ref(),
                                );
                                relay_ok = plan.relay_ok;
                                *retry_terminal_delivery_from_offset = plan.retry_offset;
                            }
                        }
            }
        }
    }
    if relay_ok {
        if direct_send_delivered || !has_direct_terminal_response {
            if direct_send_delivered {
                // #3041 P1-4 codex: clear BY the generation snapshotted before
                // this awaited delivery, NOT by key. The old unconditional by-key
                // clear had a stale-snapshot clobber: turn-1 snapshots the lease
                // present, starts its send; turn-2 records a NEWER same-key lease;
                // turn-1's send succeeds and the by-key clear removed turn-2's
                // lease (re-introducing the exact no-clobber race the generation
                // nonce was added to kill). Generation-scoped clear only removes
                // the lease this relay actually consumed; sentinel/None (no lease
                // was present) clears nothing — guarded by the consumed gate too.
                if let Some(generation) = external_input_lease_generation_before_relay
                    && external_input_lease_consumed_by_relay
                {
                    crate::services::tui_prompt_dedupe::clear_external_input_relay_lease_if_generation_matches(
                                watcher_provider.as_str(),
                                &tmux_session_name,
                                channel_id.get(),
                                generation,
                            );
                }
                if watcher_direct_terminal_should_commit_session_idle(
                    direct_send_delivered,
                    inflight_before_relay.is_some(),
                    external_input_lease_consumed_by_relay,
                    prompt_anchor_present_before_relay,
                    external_input_lease_before_relay,
                    ssh_direct_pending,
                ) {
                    *watcher_direct_terminal_idle_committed =
                        commit_watcher_direct_terminal_session_idle(
                            &shared,
                            &watcher_provider,
                            channel_id,
                            &tmux_session_name,
                            terminal_kind,
                            data_start_offset,
                            current_offset,
                        )
                        .await;
                }
            }
            *last_relayed_offset = Some(turn_data_start_offset);
            // #1270 codex P2: snapshot the current `.generation` mtime on
            // every successful relay so the local regression check has a
            // real baseline. Without this, normal relay paths (which never
            // enter the reset helper) leave the baseline at None, and a
            // later regression misclassifies same-wrapper rotation as
            // fresh-respawn — clearing the offset and re-relaying bytes.
            *last_observed_generation_mtime_ns =
                Some(read_generation_file_mtime_ns(&tmux_session_name));
            // #1134: first successful relay for this attach. The
            // watcher_latency module is idempotent — only the first
            // call after `record_attach` actually observes a sample,
            // so the unconditional call here is safe and cheap.
            crate::services::observability::watcher_latency::record_first_relay(channel_id.get());
            // #3558 (codex review follow-up): same backward-write TOCTOU
            // as the session-bound-delegation arm — the old unlocked
            // `load_inflight_state` → mutate → `save_inflight_state` re-wrote
            // the whole stale row (including `last_offset`/
            // `response_sent_offset`). Route the relay-success watermark
            // through the single-flock RMW helper, which patches ONLY
            // `last_watcher_relayed_*` and preserves the disk watermark.
            // #1270: persist the matching `.generation` mtime alongside the
            // offset so a replacement watcher (e.g. after dcserver restart)
            // can disambiguate same-wrapper rotation (mtime unchanged → pin
            // to EOF) from cancel→respawn (mtime changed → reset to 0) when
            // restoring this offset.
            if let Some(identity) = inflight_identity_before_relay.as_ref() {
                let _ = crate::services::discord::inflight::persist_watcher_relay_watermark_locked(
                    &watcher_provider,
                    channel_id.get(),
                    identity,
                    &tmux_session_name,
                    crate::services::discord::inflight::WatcherRelayWatermarkPatch {
                        last_watcher_relayed_offset: Some(turn_data_start_offset),
                        last_watcher_relayed_generation_mtime_ns:
                            *last_observed_generation_mtime_ns,
                    },
                );
            }
        }
        clear_provider_overload_retry_state(channel_id);
    }
    relay_ok
}

#[cfg(test)]
mod tests {
    include!("terminal_direct_fallback_tests.rs");
}
