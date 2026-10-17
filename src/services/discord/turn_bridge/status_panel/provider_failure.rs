//! Terminal projection of a turn that ended on the bridge's provider failure notice.

use super::super::*;
use super::{StatusPanelSurface, purge, singleton, status_panel_message_missing_error};

/// First line of the bridge's provider failure notice; the notice itself carries the detail.
pub(in crate::services::discord::turn_bridge) const PROVIDER_FAILURE_SUMMARY: &str =
    "provider가 응답을 완료하지 못했어요.";

/// Whether the turn ends on the bridge's own provider failure notice, not on an answer or on a
/// stale-resume retry, requeue or silent turn that keep their own handling.
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

/// The failed turn's panel. It has no placeholder shape, so a queued terminal reconcile consumes
/// it without editing.
pub(in crate::services::discord::turn_bridge) fn failed_panel_text(started_at_unix: i64) -> String {
    format!("⚠️ 실패 — 응답을 완료하지 못했어요\n턴 시작 : <t:{started_at_unix}:R>")
}

/// Ends the separate panel on the failure line instead of a completion, with the same terminal
/// cleanup; the detail stays in the notice only.
pub(super) async fn end_status_panel<G: TurnGateway + ?Sized>(
    shared: &SharedData,
    gateway: &G,
    channel_id: ChannelId,
    status_panel_msg_id: Option<MessageId>,
    provider: &ProviderKind,
    started_at_unix: i64,
    last_status_panel_text: &mut String,
) -> bool {
    let Some(panel_id) = normalize_status_panel_message_id(status_panel_msg_id) else {
        return true;
    };
    if !shared.ui.status_panel_v2_enabled {
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
