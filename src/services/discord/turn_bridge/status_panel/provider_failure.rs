//! Terminal projection of a turn that ended on the bridge's provider failure notice.

use super::super::*;
use super::{StatusPanelSurface, purge, singleton, status_panel_message_missing_error};

/// First line of the bridge's provider failure notice; the notice itself carries the detail.
pub(in crate::services::discord::turn_bridge) const PROVIDER_FAILURE_SUMMARY: &str =
    "provider가 응답을 완료하지 못했어요.";

/// Whether the turn ends on the bridge's own provider failure notice, not on an answer or on a
/// stale-resume retry, busy or pre-submit requeue, or silent turn that keep their own handling.
pub(in crate::services::discord::turn_bridge) fn is_provider_failure_turn(
    transport_error: bool,
    resume_failure_detected: bool,
    requeue: bool,
    silent_turn: bool,
    full_response: &str,
) -> bool {
    transport_error
        && !resume_failure_detected
        && !requeue
        && !silent_turn
        && full_response
            .strip_prefix("⚠️ ")
            .is_some_and(|rest| rest.starts_with(PROVIDER_FAILURE_SUMMARY))
}

/// The failed turn's panel line. It is no placeholder shape, so a queued terminal reconcile
/// consumes it without editing.
pub(in crate::services::discord::turn_bridge) fn failed_panel_text(started_at_unix: i64) -> String {
    format!("⚠️ 실패 — 응답을 완료하지 못했어요\n턴 시작 : <t:{started_at_unix}:R>")
}

/// The turn's terminal projection: the usual completion for an answer. A provider failure gets
/// no success footer, and its separate panel ends on the failure line with the same ownership
/// checks and terminal cleanup as a completion.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord::turn_bridge) async fn complete_terminal_projection<
    G: TurnGateway + ?Sized,
>(
    provider_failure: bool,
    shared: &SharedData,
    gateway: &G,
    channel_id: ChannelId,
    current_msg_id: MessageId,
    user_msg_id: Option<MessageId>,
    status_panel_msg_id: Option<MessageId>,
    provider: &ProviderKind,
    started_at_unix: i64,
    last_status_panel_text: &mut String,
    single_message_panel_footer_mode: bool,
    is_external_input_tui_direct: bool,
    terminal_text: Option<&str>,
    indicator: &str,
    this_turn_status_panel_generation: u64,
    tmux_session_name: Option<&str>,
    permits_channel_effects: bool,
) -> bool {
    if !provider_failure {
        return super::super::single_message_footer::complete_bridge_terminal_footer_or_status_panel(
            shared,
            gateway,
            channel_id,
            current_msg_id,
            user_msg_id,
            status_panel_msg_id,
            provider,
            started_at_unix,
            last_status_panel_text,
            single_message_panel_footer_mode,
            is_external_input_tui_direct,
            terminal_text,
            indicator,
            this_turn_status_panel_generation,
            tmux_session_name,
            permits_channel_effects,
        )
        .await;
    }
    // In footer mode the notice already replaced the footer message; it stays as the last word.
    if single_message_panel_footer_mode
        || !permits_channel_effects
        || !shared.ui.status_panel_v2_enabled
    {
        return true;
    }
    let Some(panel_id) = normalize_status_panel_message_id(status_panel_msg_id) else {
        return true;
    };
    let this_turn = user_msg_id.map_or(0, MessageId::get);
    let generation = this_turn_status_panel_generation;
    if super::completion_panel_superseded(
        provider,
        channel_id,
        this_turn,
        Some(panel_id),
        generation,
    ) {
        return true;
    }
    let text = failed_panel_text(started_at_unix);
    if *last_status_panel_text != text {
        let surface = StatusPanelSurface::Gateway(shared, gateway);
        match surface.edit(channel_id, panel_id, &text).await {
            Ok(()) => {}
            Err(error) if status_panel_message_missing_error(&error) => return true,
            Err(error) => {
                tracing::warn!(%channel_id, %error, "failed to end status panel on provider failure");
                return false;
            }
        }
    }
    singleton::commit_completed_binding(shared, provider, channel_id, Some(panel_id));
    *last_status_panel_text = text;
    purge::purge_pending_bind_for_completed_status_panel(
        shared,
        provider,
        channel_id,
        Some(panel_id),
    );
    purge::purge_terminal_reconcile_for_completed_status_panel(
        shared,
        provider,
        channel_id,
        Some(panel_id),
    );
    true
}
