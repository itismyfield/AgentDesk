use std::sync::Arc;

use super::*;

use crate::services::discord::SharedData;
use crate::services::discord::placeholder_cleanup::PlaceholderCleanupOutcome;
use crate::services::provider::ProviderKind;

pub(in crate::services::discord) fn reconcile_committed_placeholder_cleanup(
    outcome: Option<&PlaceholderCleanupOutcome>,
    placeholder_msg_id: &mut Option<MessageId>,
    placeholder_from_restored_inflight: &mut bool,
    last_edit_text: &mut String,
    on_committed: impl FnOnce(),
) -> bool {
    if !outcome.is_some_and(PlaceholderCleanupOutcome::is_committed) {
        return false;
    }
    *placeholder_msg_id = None;
    *placeholder_from_restored_inflight = false;
    last_edit_text.clear();
    on_committed();
    true
}

/// Borrowed inputs for [`reconcile_already_committed_after_edit_failure`]:
/// the shared transport/session identity of the edit-failed range plus the
/// watcher send-arm locals the guarded reconciliation may clear. Grouped so the
/// helper stays under the structural argument-count ratchet without an allow.
pub(in crate::services::discord) struct CommittedEditFailureReconcileCtx<'a> {
    pub(in crate::services::discord) http: &'a Arc<serenity::Http>,
    pub(in crate::services::discord) shared: &'a Arc<SharedData>,
    pub(in crate::services::discord) provider: &'a ProviderKind,
    pub(in crate::services::discord) channel_id: ChannelId,
    pub(in crate::services::discord) tmux_session_name: &'a str,
    pub(in crate::services::discord) msg_id: MessageId,
    pub(in crate::services::discord) inflight_before_relay:
        Option<&'a crate::services::discord::InflightTurnState>,
    pub(in crate::services::discord) range: (u64, u64),
    pub(in crate::services::discord) response_sent_offset: usize,
    pub(in crate::services::discord) edit_error: String,
    pub(in crate::services::discord) direct_send_delivered: &'a mut bool,
    pub(in crate::services::discord) tui_direct_anchor_terminal_body_visible: &'a mut bool,
    pub(in crate::services::discord) placeholder_msg_id: &'a mut Option<MessageId>,
    pub(in crate::services::discord) placeholder_from_restored_inflight: &'a mut bool,
    pub(in crate::services::discord) last_edit_text: &'a mut String,
    pub(in crate::services::discord) cleanup_source: &'static str,
    pub(in crate::services::discord) record_source: &'static str,
}

pub(in crate::services::discord) async fn reconcile_already_committed_after_edit_failure(
    ctx: CommittedEditFailureReconcileCtx<'_>,
) {
    let CommittedEditFailureReconcileCtx {
        http,
        shared,
        provider,
        channel_id,
        tmux_session_name,
        msg_id,
        inflight_before_relay,
        range,
        response_sent_offset,
        edit_error,
        direct_send_delivered,
        tui_direct_anchor_terminal_body_visible,
        placeholder_msg_id,
        placeholder_from_restored_inflight,
        last_edit_text,
        cleanup_source,
        record_source,
    } = ctx;
    *direct_send_delivered = true;
    *tui_direct_anchor_terminal_body_visible = false;
    let cleanup = super::super::delete_terminal_placeholder_unless_delivered(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        msg_id,
        inflight_before_relay,
        Some(range),
        response_sent_offset,
        last_edit_text,
        true,
        cleanup_source,
    )
    .await;
    reconcile_committed_placeholder_cleanup(
        cleanup.as_ref(),
        placeholder_msg_id,
        placeholder_from_restored_inflight,
        last_edit_text,
        || drop_placeholder_orphan_record(provider, shared, channel_id, msg_id),
    );
    super::super::record_placeholder_cleanup(
        shared,
        provider,
        channel_id,
        msg_id,
        tmux_session_name,
        crate::services::discord::placeholder_cleanup::PlaceholderCleanupOperation::EditTerminal,
        PlaceholderCleanupOutcome::failed(edit_error),
        record_source,
    );
}

/// Reconcile streamed copies only after the existing terminal plan confirms delivery.
pub(in crate::services::discord) struct ConfirmedPreviewCleanup<'a> {
    pub http: &'a Arc<serenity::Http>,
    pub shared: &'a Arc<SharedData>,
    pub provider: &'a ProviderKind,
    pub channel: ChannelId,
    pub session: &'a str,
    pub expected_turn: Option<&'a InflightTurnState>,
    pub range: (u64, u64),
    pub sent_offset: usize,
    pub placeholder: &'a mut Option<MessageId>,
    pub restored: &'a mut bool,
    pub edit: &'a mut String,
    pub frozen: &'a mut Vec<MessageId>,
}

pub(in crate::services::discord) async fn reconcile_confirmed_preview(
    ctx: ConfirmedPreviewCleanup<'_>,
) {
    use crate::services::discord::inflight::InflightTurnIdentity;
    let mut ids = ctx.frozen.clone();
    ids.extend(*ctx.placeholder);
    ids.sort_unstable();
    ids.dedup();
    for id in ids {
        // ACK awaited network: re-read now and never use an old row to authorize
        // deletion of a successor's anchor.
        let current = crate::services::discord::inflight::load_inflight_state(
            ctx.provider,
            ctx.channel.get(),
        );
        if current.as_ref().is_some_and(|row| {
            !ctx.expected_turn.is_some_and(|expected| {
                InflightTurnIdentity::from_state(expected).matches_state(row)
                    && expected.turn_nonce == row.turn_nonce
                    && expected.provider == row.provider
                    && expected.request_owner_user_id == row.request_owner_user_id
                    && expected.finalizer_turn_id == row.finalizer_turn_id
                    && expected.output_path == row.output_path
                    && expected.runtime_kind == row.runtime_kind
            })
        }) {
            return;
        }
        let outcome = delete_terminal_placeholder_unless_delivered(
            ctx.http,
            ctx.channel,
            ctx.shared,
            ctx.provider,
            ctx.session,
            id,
            current.as_ref(),
            Some(ctx.range),
            ctx.sent_offset,
            ctx.edit,
            true,
            "watcher_confirmed_sink_preview_cleanup",
        )
        .await;
        if outcome
            .as_ref()
            .is_some_and(PlaceholderCleanupOutcome::is_committed)
        {
            ctx.frozen.retain(|frozen| *frozen != id);
            if *ctx.placeholder == Some(id) {
                reconcile_committed_placeholder_cleanup(
                    outcome.as_ref(),
                    ctx.placeholder,
                    ctx.restored,
                    ctx.edit,
                    || {},
                );
            }
            drop_placeholder_orphan_record(ctx.provider, ctx.shared, ctx.channel, id);
        } else if outcome
            .as_ref()
            .is_some_and(|outcome| !outcome.is_permanent_failure())
        {
            // Some means positive delivered-elsewhere proof authorized DELETE;
            // an HTTP failure leaves a durable retry, never an unproven deletion.
            crate::services::discord::status_panel_orphan_store::enqueue(
                ctx.provider,
                &ctx.shared.token_hash,
                ctx.channel.get(),
                id.get(),
            );
        }
    }
}
