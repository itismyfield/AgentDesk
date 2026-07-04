//! #3038 S1 status-panel completion helpers moved out of turn_bridge/mod.rs.

use super::*;

pub(super) fn status_panel_completion_ready_after_terminal_body(
    terminal_delivery_committed: bool,
    terminal_body_visible: bool,
    preserve_inflight_for_cleanup_retry: bool,
) -> bool {
    terminal_delivery_committed && terminal_body_visible && !preserve_inflight_for_cleanup_retry
}

pub(super) async fn complete_status_panel_v2<G: TurnGateway + ?Sized>(
    shared: &SharedData,
    gateway: &G,
    channel_id: ChannelId,
    status_panel_msg_id: Option<MessageId>,
    provider: &ProviderKind,
    started_at_unix: i64,
    last_status_panel_text: &mut String,
    background: bool,
    background_agent_pending: bool,
    source: &'static str,
    expected_user_msg_id: u64,
) -> bool {
    if !shared.ui.status_panel_v2_enabled {
        return true;
    }
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::TurnCompleted {
            background,
            background_agent_pending,
        },
    );
    let panel_text = shared.ui.placeholder_live_events.render_status_panel(
        channel_id,
        provider,
        started_at_unix,
    );

    match status_panel_completion_action(status_panel_msg_id, last_status_panel_text, &panel_text) {
        StatusPanelCompletionAction::AlreadyCommitted => {
            purge_pending_bind_for_completed_status_panel(
                shared,
                provider,
                channel_id,
                status_panel_msg_id,
            );
            true
        }
        StatusPanelCompletionAction::SendFallback => {
            let inflight =
                crate::services::discord::turn_end_wip_warning::load_matching_inflight_state(
                    provider,
                    channel_id,
                    Some(expected_user_msg_id),
                );
            let _ = warn_turn_end_wip_before_status_panel_commit(
                shared,
                gateway,
                channel_id,
                inflight.as_ref(),
                source,
            )
            .await;
            complete_status_panel_v2_fallback_with_gateway(
                shared,
                gateway,
                channel_id,
                provider,
                expected_user_msg_id,
                last_status_panel_text,
                panel_text,
                source,
            )
            .await
        }
        StatusPanelCompletionAction::Edit(status_msg_id) => {
            let inflight =
                crate::services::discord::turn_end_wip_warning::load_matching_inflight_state(
                    provider,
                    channel_id,
                    Some(expected_user_msg_id),
                );
            let _ = warn_turn_end_wip_before_status_panel_commit(
                shared,
                gateway,
                channel_id,
                inflight.as_ref(),
                source,
            )
            .await;
            let edit_result = if gateway.can_chain_locally() {
                TurnGateway::edit_message(gateway, channel_id, status_msg_id, &panel_text).await
            } else if let Some(http) = shared.serenity_http_or_token_fallback() {
                super::http::edit_channel_message(&http, channel_id, status_msg_id, &panel_text)
                    .await
                    .map(|_| ())
                    .map_err(|error| error.to_string())
            } else {
                Err("no Discord HTTP available for status-panel-v2 completion edit".to_string())
            };
            match edit_result {
                Ok(()) => {
                    *last_status_panel_text = panel_text;
                    purge_pending_bind_for_completed_status_panel(
                        shared,
                        provider,
                        channel_id,
                        status_panel_msg_id,
                    );
                    true
                }
                Err(error) => {
                    if status_panel_message_missing_error(&error) {
                        return complete_status_panel_v2_fallback_with_gateway(
                            shared,
                            gateway,
                            channel_id,
                            provider,
                            expected_user_msg_id,
                            last_status_panel_text,
                            panel_text,
                            source,
                        )
                        .await;
                    }
                    tracing::warn!(
                        "[turn_bridge] failed to finalize status-panel-v2 message {} in channel {} from {}: {}",
                        status_msg_id,
                        channel_id,
                        source,
                        error
                    );
                    false
                }
            }
        }
    }
}

async fn warn_turn_end_wip_before_status_panel_commit<G: TurnGateway + ?Sized>(
    shared: &SharedData,
    gateway: &G,
    channel_id: ChannelId,
    inflight: Option<&super::super::InflightTurnState>,
    source: &'static str,
) -> crate::services::discord::turn_end_wip_warning::TurnEndWipWarningOutcome {
    if gateway.can_chain_locally() {
        return crate::services::discord::turn_end_wip_warning::warn_turn_end_wip_with_gateway(
            gateway, channel_id, inflight, source,
        )
        .await;
    }
    crate::services::discord::turn_end_wip_warning::warn_turn_end_wip_with_shared_http(
        shared, channel_id, inflight, source,
    )
    .await
}

enum StatusPanelWipInflight<'a> {
    Preloaded(&'a super::super::InflightTurnState),
    Loaded(super::super::InflightTurnState),
}

impl StatusPanelWipInflight<'_> {
    fn as_inflight(&self) -> &super::super::InflightTurnState {
        match self {
            StatusPanelWipInflight::Preloaded(state) => state,
            StatusPanelWipInflight::Loaded(state) => state,
        }
    }
}

fn preloaded_status_panel_wip_inflight<'a>(
    preloaded: Option<&'a super::super::InflightTurnState>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_msg_id: Option<u64>,
) -> Option<&'a super::super::InflightTurnState> {
    let expected_user_msg_id = expected_user_msg_id?;
    if expected_user_msg_id == 0 {
        return None;
    }
    let state = preloaded?;
    if state.user_msg_id != expected_user_msg_id {
        return None;
    }
    if state.channel_id != channel_id.get() {
        return None;
    }
    if state.provider != provider.as_str() {
        return None;
    }
    Some(state)
}

fn status_panel_wip_inflight_for_completion<'a>(
    preloaded: Option<&'a super::super::InflightTurnState>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_msg_id: Option<u64>,
) -> Option<StatusPanelWipInflight<'a>> {
    if let Some(state) =
        preloaded_status_panel_wip_inflight(preloaded, provider, channel_id, expected_user_msg_id)
    {
        return Some(StatusPanelWipInflight::Preloaded(state));
    }
    crate::services::discord::turn_end_wip_warning::load_matching_inflight_state(
        provider,
        channel_id,
        expected_user_msg_id,
    )
    .map(StatusPanelWipInflight::Loaded)
}

async fn complete_status_panel_v2_fallback_with_gateway<G: TurnGateway + ?Sized>(
    shared: &SharedData,
    gateway: &G,
    channel_id: ChannelId,
    provider: &ProviderKind,
    expected_user_msg_id: u64,
    last_status_panel_text: &mut String,
    panel_text: String,
    source: &'static str,
) -> bool {
    match send_status_panel_v2_completion_fallback(shared, gateway, channel_id, &panel_text).await {
        Ok(message_id) => {
            persist_status_panel_completion_fallback_message_id(
                provider,
                channel_id,
                Some(expected_user_msg_id),
                message_id,
                source,
            );
            *last_status_panel_text = panel_text;
            true
        }
        Err(error) => {
            tracing::warn!(
                "[turn_bridge] failed to send fallback status-panel-v2 completion in channel {} from {}: {}",
                channel_id,
                source,
                error
            );
            false
        }
    }
}

pub(in crate::services::discord) async fn complete_status_panel_v2_with_http(
    shared: &std::sync::Arc<SharedData>,
    http: &serenity::Http,
    channel_id: ChannelId,
    status_panel_msg_id: Option<MessageId>,
    provider: &ProviderKind,
    started_at_unix: i64,
    last_status_panel_text: &mut String,
    background: bool,
    background_agent_pending: bool,
    source: &'static str,
    expected_inflight: (Option<u64>, Option<&super::super::InflightTurnState>),
) -> bool {
    let (expected_user_msg_id, inflight_snapshot) = expected_inflight;
    if !shared.ui.status_panel_v2_enabled {
        return true;
    }
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::TurnCompleted {
            background,
            background_agent_pending,
        },
    );
    let panel_text = shared.ui.placeholder_live_events.render_status_panel(
        channel_id,
        provider,
        started_at_unix,
    );

    match status_panel_completion_action(status_panel_msg_id, last_status_panel_text, &panel_text) {
        StatusPanelCompletionAction::AlreadyCommitted => {
            purge_pending_bind_for_completed_status_panel(
                shared.as_ref(),
                provider,
                channel_id,
                status_panel_msg_id,
            );
            true
        }
        StatusPanelCompletionAction::SendFallback => {
            let inflight = status_panel_wip_inflight_for_completion(
                inflight_snapshot,
                provider,
                channel_id,
                expected_user_msg_id,
            );
            let _ = crate::services::discord::turn_end_wip_warning::warn_turn_end_wip_with_http(
                http,
                channel_id,
                inflight.as_ref().map(StatusPanelWipInflight::as_inflight),
                source,
            )
            .await;
            rate_limit_wait(shared, channel_id).await;
            complete_status_panel_v2_fallback_with_http(
                http,
                channel_id,
                provider,
                expected_user_msg_id,
                last_status_panel_text,
                panel_text,
                source,
            )
            .await
        }
        StatusPanelCompletionAction::Edit(status_msg_id) => {
            let inflight = status_panel_wip_inflight_for_completion(
                inflight_snapshot,
                provider,
                channel_id,
                expected_user_msg_id,
            );
            let _ = crate::services::discord::turn_end_wip_warning::warn_turn_end_wip_with_http(
                http,
                channel_id,
                inflight.as_ref().map(StatusPanelWipInflight::as_inflight),
                source,
            )
            .await;
            rate_limit_wait(shared, channel_id).await;
            match super::http::edit_channel_message(http, channel_id, status_msg_id, &panel_text)
                .await
            {
                Ok(_) => {
                    *last_status_panel_text = panel_text;
                    purge_pending_bind_for_completed_status_panel(
                        shared.as_ref(),
                        provider,
                        channel_id,
                        status_panel_msg_id,
                    );
                    true
                }
                Err(error) => {
                    let error = error.to_string();
                    if status_panel_message_missing_error(&error) {
                        return complete_status_panel_v2_fallback_with_http(
                            http,
                            channel_id,
                            provider,
                            expected_user_msg_id,
                            last_status_panel_text,
                            panel_text,
                            source,
                        )
                        .await;
                    }
                    tracing::warn!(
                        "[turn_bridge] failed to finalize status-panel-v2 message {} in channel {} from {}: {}",
                        status_msg_id,
                        channel_id,
                        source,
                        error
                    );
                    false
                }
            }
        }
    }
}

async fn complete_status_panel_v2_fallback_with_http(
    http: &serenity::Http,
    channel_id: ChannelId,
    provider: &ProviderKind,
    expected_user_msg_id: Option<u64>,
    last_status_panel_text: &mut String,
    panel_text: String,
    source: &'static str,
) -> bool {
    match send_status_panel_v2_completion_fallback_http(http, channel_id, &panel_text).await {
        Ok(message_id) => {
            persist_status_panel_completion_fallback_message_id(
                provider,
                channel_id,
                expected_user_msg_id,
                message_id,
                source,
            );
            *last_status_panel_text = panel_text;
            true
        }
        Err(error) => {
            tracing::warn!(
                "[turn_bridge] failed to send fallback status-panel-v2 completion in channel {} from {}: {}",
                channel_id,
                source,
                error
            );
            false
        }
    }
}

fn purge_pending_bind_for_completed_status_panel(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    status_panel_msg_id: Option<MessageId>,
) {
    let Some(message_id) = normalize_status_panel_message_id(status_panel_msg_id) else {
        return;
    };
    crate::services::discord::status_panel_orphan_store::remove_pending_bind(
        provider,
        &shared.token_hash,
        channel_id.get(),
        message_id.get(),
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusPanelCompletionAction {
    AlreadyCommitted,
    Edit(MessageId),
    SendFallback,
}

fn status_panel_completion_action(
    status_panel_msg_id: Option<MessageId>,
    last_status_panel_text: &str,
    panel_text: &str,
) -> StatusPanelCompletionAction {
    if panel_text == last_status_panel_text {
        return StatusPanelCompletionAction::AlreadyCommitted;
    }
    match normalize_status_panel_message_id(status_panel_msg_id) {
        Some(message_id) => StatusPanelCompletionAction::Edit(message_id),
        None => StatusPanelCompletionAction::SendFallback,
    }
}

pub(in crate::services::discord) fn normalize_status_panel_message_id(
    status_panel_msg_id: Option<MessageId>,
) -> Option<MessageId> {
    status_panel_msg_id.filter(|id| !is_synthetic_headless_message_id(*id))
}

/// #3161 (follow-up to #3142): bridge-path sibling of the watcher
/// committed-output status-panel staleness gate. The bridge captures
/// `status_panel_msg_id` from THIS turn's pinned inflight snapshot at turn start
/// and EDITs it at completion (the `complete_status_panel_v2` Edit arm). Between
/// those two points a NEWER follow-up turn on the SAME channel can re-bind the
/// on-disk `status_message_id` onto the SAME panel message (status-panel reuse),
/// so by completion time that Discord message is the newer turn's LIVE panel. If
/// the older bridge turn still EDITs it with its own `응답 완료` text it aliases
/// the newer turn's panel (the newer turn then re-overwrites it — cosmetic-
/// transient, matching the issue's severity).
///
/// The bridge is turn-pinned by IDENTITY (it owns `this_turn_user_msg_id`), not
/// by a committed offset range like the watcher, so the gate is identity-based:
/// return TRUE (skip the panel EDIT) iff the CURRENT on-disk row is concrete
/// evidence of a DIFFERENT, real turn that now OWNS this turn's panel —
/// `this_turn_user_msg_id != 0` AND `on_disk_user_msg_id != 0` AND
/// `on_disk_user_msg_id != this_turn_user_msg_id` AND the on-disk row's
/// `status_message_id` equals THIS turn's `status_panel_msg_id`.
///
/// Over-suppression guard (the issue's explicit requirement): an in-range
/// id==0 bridge/watcher-direct turn (`this_turn_user_msg_id == 0`, e.g.
/// TUI-direct / external-input) is NEVER flagged — the leading
/// `this_turn_user_msg_id != 0` short-circuit keeps it completing its panel
/// even when a different real on-disk owner is present, because a 0-id turn
/// cannot be proven stale by identity. We additionally require a real on-disk
/// owner AND that the on-disk row OWNS our exact panel id, so a turn whose
/// panel was never re-adopted still completes normally. Absent inflight row, no
/// panel id, a same-turn row, or a row pointing at a different panel all return
/// FALSE → the EDIT fires exactly as today.
pub(super) fn status_panel_completion_edit_aliases_newer_turn(
    this_turn_user_msg_id: u64,
    status_panel_msg_id: Option<MessageId>,
    on_disk_user_msg_id: u64,
    on_disk_status_message_id: Option<u64>,
) -> bool {
    let Some(panel_id) = normalize_status_panel_message_id(status_panel_msg_id) else {
        return false;
    };
    // `this_turn_user_msg_id != 0`: an in-range id==0 watcher-direct / external-
    // input bridge turn cannot be proven stale by identity, so it MUST still
    // complete its panel (the issue's over-suppression guard). Only a real
    // (non-zero) this-turn identity that a DIFFERENT real on-disk owner has
    // superseded on the SAME panel is treated as aliasing.
    this_turn_user_msg_id != 0
        && on_disk_user_msg_id != 0
        && on_disk_user_msg_id != this_turn_user_msg_id
        && on_disk_status_message_id == Some(panel_id.get())
}

/// #3161 (codex P1): pure seam deciding whether THIS turn's epilogue must
/// identity-guard the inflight-row removal instead of clearing it
/// unconditionally. A real (non-zero) this-turn identity MUST be guarded so an
/// OLD turn whose status-panel completion edit was alias-skipped (because a
/// NEWER turn re-adopted its panel — see
/// [`status_panel_completion_edit_aliases_newer_turn`]) does NOT also delete the
/// NEWER owner's on-disk inflight row. The guarded clear only removes the row
/// when the on-disk `user_msg_id` still matches THIS turn, so a newer owner is
/// preserved and can still complete its own panel.
///
/// An id==0 this-turn (TUI-direct / external-input bridge turn) cannot be
/// proven against the on-disk identity, so it keeps the unconditional clear —
/// the same over-suppression carve-out the alias predicate uses for the EDIT.
pub(super) fn bridge_epilogue_identity_guards_inflight_clear(this_turn_user_msg_id: u64) -> bool {
    this_turn_user_msg_id != 0
}

fn persist_status_panel_completion_fallback_message_id(
    provider: &ProviderKind,
    channel_id: ChannelId,
    expected_user_msg_id: Option<u64>,
    message_id: MessageId,
    source: &'static str,
) {
    if is_synthetic_headless_message_id(message_id) {
        return;
    }
    let Some(expected_user_msg_id) = expected_user_msg_id else {
        return;
    };
    // #3077: route the load-modify-save through the typed bind op so the
    // user_msg_id guard and the field set are serialized under the inflight
    // flock (no TOCTOU with a concurrent turn rebinding the row). Behavior is
    // preserved: bind only when the on-disk row still belongs to this turn.
    let guard = super::inflight::StatusPanelBindGuard {
        require_user_msg_id: Some(expected_user_msg_id),
        ..Default::default()
    };
    match super::inflight::bind_status_panel(provider, channel_id.get(), message_id.get(), &guard) {
        super::inflight::StatusPanelBindOutcome::Bound { .. }
        | super::inflight::StatusPanelBindOutcome::AlreadyBound
        | super::inflight::StatusPanelBindOutcome::SkippedPanelAlreadySet(_) => {}
        super::inflight::StatusPanelBindOutcome::Missing => {}
        super::inflight::StatusPanelBindOutcome::GuardMismatch => {
            tracing::debug!(
                "[turn_bridge] skipped persisting status-panel-v2 fallback id {} in channel {} from {}: inflight user_msg_id != expected {}",
                message_id,
                channel_id,
                source,
                expected_user_msg_id
            );
        }
        super::inflight::StatusPanelBindOutcome::IoError => {
            tracing::warn!(
                "[turn_bridge] failed to persist fallback status-panel-v2 message {} in channel {} from {}",
                message_id,
                channel_id,
                source
            );
        }
    }
}

async fn send_status_panel_v2_completion_fallback_http(
    http: &serenity::Http,
    channel_id: ChannelId,
    panel_text: &str,
) -> Result<MessageId, String> {
    super::http::send_channel_message(http, channel_id, panel_text)
        .await
        .map(|message| message.id)
        .map_err(|error| error.to_string())
}

async fn send_status_panel_v2_completion_fallback<G: TurnGateway + ?Sized>(
    shared: &SharedData,
    gateway: &G,
    channel_id: ChannelId,
    panel_text: &str,
) -> Result<MessageId, String> {
    if gateway.can_chain_locally() {
        return gateway.send_message(channel_id, panel_text).await;
    }
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return Err(
            "no Discord HTTP available for status-panel-v2 completion fallback".to_string(),
        );
    };
    super::http::send_channel_message(&http, channel_id, panel_text)
        .await
        .map(|message| message.id)
        .map_err(|error| error.to_string())
}

fn status_panel_message_missing_error(error: &str) -> bool {
    let normalized = error.to_ascii_lowercase();
    normalized.contains("unknown message") || normalized.contains("10008")
}

pub(super) fn should_open_long_running_placeholder_controller(
    status_panel_v2_enabled: bool,
) -> bool {
    !status_panel_v2_enabled
}

pub(super) fn status_panel_message_id_for_turn(
    inflight_state: &mut InflightTurnState,
    reuse_status_panel_message: bool,
) -> Option<MessageId> {
    if !reuse_status_panel_message {
        inflight_state.status_message_id = None;
    }
    let status_msg_id = inflight_state.status_message_id.map(MessageId::new)?;
    if is_synthetic_headless_message_id(status_msg_id) {
        inflight_state.status_message_id = None;
        return None;
    }
    Some(status_msg_id)
}

/// #3560 codex review: default-OFF → default-ON deployment migration guard.
///
/// When `single_message_panel` was default-OFF a bridge turn may have already
/// created a *separate* status panel (a real Discord message tracked by
/// `inflight_state.status_message_id`). After #3560 the same turn can be
/// resumed under footer mode, where the separate panel is no longer used.
/// Previously the resume path simply cleared `status_message_id` to `None`,
/// orphaning that Discord message (a stuck panel that never completes).
///
/// This helper closes that gap: before clearing the handle, it edits the old
/// panel into a short migration notice (mirroring the
/// `long_running_placeholder_active` reconciliation path) so the message is
/// visibly finalized rather than left dangling. Synthetic-headless ids are not
/// real Discord messages, so they are dropped without an edit.
///
/// Returns `true` when an old separate panel existed and was reconciled
/// (regardless of edit success), `false` when there was nothing to migrate.
/// This is worker-local: it operates only on the resuming turn's own handle.
pub(super) async fn migrate_separate_status_panel_to_footer<G: TurnGateway + ?Sized>(
    gateway: &G,
    channel_id: ChannelId,
    inflight_state: &mut InflightTurnState,
) -> bool {
    let Some(raw_id) = inflight_state.status_message_id.take() else {
        return false;
    };
    let old_id = MessageId::new(raw_id);
    if is_synthetic_headless_message_id(old_id) {
        // Not a real Discord message — nothing to finalize.
        return true;
    }
    let migration_notice = "🔁 상태 패널이 통합 패널로 이전되었습니다.";
    if let Err(error) = gateway
        .edit_message(channel_id, old_id, migration_notice)
        .await
    {
        tracing::warn!(
            "[turn_bridge] failed to finalize migrated separate status panel {} in channel {}: {}",
            old_id,
            channel_id,
            error
        );
    }
    true
}

#[cfg(test)]
#[path = "status_panel_tests.rs"]
mod status_panel_v2_rework_tests;

pub(super) fn record_status_panel_events(
    shared: &SharedData,
    channel_id: ChannelId,
    events: Vec<StatusEvent>,
) -> bool {
    if shared.ui.status_panel_v2_enabled && !events.is_empty() {
        shared
            .ui
            .placeholder_live_events
            .push_status_events(channel_id, events);
        true
    } else {
        false
    }
}
