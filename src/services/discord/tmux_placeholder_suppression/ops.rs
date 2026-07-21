use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use crate::services::provider::ProviderKind;

use super::super::super::placeholder_cleanup::{
    PlaceholderCleanupOperation, PlaceholderCleanupOutcome, PlaceholderCleanupRecord,
    classify_delete_error, committed_terminal_anchor_protects_delete,
};
use super::super::super::{SharedData, rate_limit_wait};
use super::evidence::{
    apply_terminal_committed_delete_proof_gate, cleanup_current_output_eof,
    guarded_cleanup_delivered_elsewhere_signal, guarded_nonterminal_delete_decision,
};
use super::{
    GuardedDeliveredElsewhereSignal, GuardedNonterminalDeleteDecision, PlaceholderSuppressDecision,
    PlaceholderSuppressOrigin, guarded_cleanup_target_author,
};
use crate::services::discord;

pub(super) fn strip_placeholder_indicators_for_preserve(
    text: &str,
    provider: &ProviderKind,
) -> String {
    discord::single_message_panel::strip_placeholder_terminal_status(text, provider)
        .trim_end()
        .to_string()
}
#[allow(clippy::too_many_arguments)]
pub(crate) async fn apply_placeholder_suppression(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    placeholder_msg_id: Option<serenity::MessageId>,
    origin: PlaceholderSuppressOrigin,
    decision: PlaceholderSuppressDecision,
    detail: Option<&str>,
) {
    let detail_suffix = detail.map(|d| format!(" — {d}")).unwrap_or_default();
    match decision {
        PlaceholderSuppressDecision::None => {}
        PlaceholderSuppressDecision::Preserve {
            reason,
            cleaned_body,
        } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 {} preserved placeholder ({reason}){detail_suffix}",
                origin.log_scope()
            );
            if let Some(msg_id) = placeholder_msg_id {
                if cleaned_body.is_empty() {
                    delete_nonterminal_placeholder(
                        http,
                        channel_id,
                        shared,
                        provider,
                        tmux_session_name,
                        msg_id,
                        origin.log_scope(),
                    )
                    .await;
                } else {
                    edit_preserve_placeholder(
                        http,
                        channel_id,
                        shared,
                        provider,
                        tmux_session_name,
                        msg_id,
                        &cleaned_body,
                        origin.log_scope(),
                    )
                    .await;
                }
            }
        }
        PlaceholderSuppressDecision::Delete => {
            if let Some(msg_id) = placeholder_msg_id {
                let outcome = delete_terminal_placeholder(
                    http,
                    channel_id,
                    shared,
                    provider,
                    tmux_session_name,
                    msg_id,
                    origin.log_scope(),
                )
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    message_id = msg_id.get(),
                    outcome = ?outcome,
                    "  [{ts}] 👁 {} delete placeholder result{detail_suffix}",
                    origin.log_scope()
                );
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 {} delete placeholder skipped (no placeholder_msg_id){detail_suffix}",
                    origin.log_scope()
                );
            }
        }
        PlaceholderSuppressDecision::Edit(content) => {
            if let Some(msg_id) = placeholder_msg_id {
                let outcome = edit_terminal_placeholder(
                    http,
                    channel_id,
                    shared,
                    provider,
                    tmux_session_name,
                    msg_id,
                    &content,
                    origin.log_scope(),
                )
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    message_id = msg_id.get(),
                    outcome = ?outcome,
                    "  [{ts}] 👁 {} edit placeholder result{detail_suffix}",
                    origin.log_scope()
                );
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 {} edit placeholder skipped (no placeholder_msg_id){detail_suffix}",
                    origin.log_scope()
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn record_placeholder_cleanup(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    message_id: MessageId,
    tmux_session_name: &str,
    operation: PlaceholderCleanupOperation,
    outcome: PlaceholderCleanupOutcome,
    source: &'static str,
) {
    if let PlaceholderCleanupOutcome::Failed { class, detail } = &outcome {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ placeholder cleanup {} failed ({}) for channel {} msg {}: {}",
            operation.as_str(),
            class.as_str(),
            channel_id.get(),
            message_id.get(),
            detail
        );
    }
    let record = PlaceholderCleanupRecord {
        provider: provider.clone(),
        channel_id,
        message_id,
        tmux_session_name: Some(tmux_session_name.to_string()),
        operation,
        outcome,
        source,
    };
    shared.ui.placeholder_cleanup.record(record);
}

pub(crate) async fn delete_terminal_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    delete_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        PlaceholderCleanupOperation::DeleteTerminal,
        source,
    )
    .await
}

pub(crate) async fn delete_nonterminal_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    delete_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        PlaceholderCleanupOperation::DeleteNonterminal,
        source,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn delete_nonterminal_placeholder_unless_delivered(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    live_inflight: Option<&discord::inflight::InflightTurnState>,
    delivered_range: Option<(u64, u64)>,
    response_sent_offset: usize,
    last_edit_text: &str,
    source: &'static str,
) -> Option<PlaceholderCleanupOutcome> {
    delete_placeholder_unless_delivered(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        live_inflight,
        delivered_range,
        response_sent_offset,
        last_edit_text,
        // Nonterminal cleanup never runs behind a proven terminal commit, so it
        // keeps the base decision table (no positive-proof requirement).
        false,
        PlaceholderCleanupOperation::DeleteNonterminal,
        source,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn delete_terminal_placeholder_unless_delivered(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    live_inflight: Option<&discord::inflight::InflightTurnState>,
    delivered_range: Option<(u64, u64)>,
    response_sent_offset: usize,
    last_edit_text: &str,
    require_delivered_elsewhere_proof: bool,
    source: &'static str,
) -> Option<PlaceholderCleanupOutcome> {
    delete_placeholder_unless_delivered(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        live_inflight,
        delivered_range,
        response_sent_offset,
        last_edit_text,
        require_delivered_elsewhere_proof,
        PlaceholderCleanupOperation::DeleteTerminal,
        source,
    )
    .await
}
#[allow(clippy::too_many_arguments)]
async fn delete_placeholder_unless_delivered(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    live_inflight: Option<&discord::inflight::InflightTurnState>,
    delivered_range: Option<(u64, u64)>,
    response_sent_offset: usize,
    last_edit_text: &str,
    require_delivered_elsewhere_proof: bool,
    operation: PlaceholderCleanupOperation,
    source: &'static str,
) -> Option<PlaceholderCleanupOutcome> {
    let committed_terminal_anchor = committed_terminal_anchor_protects_delete(
        &shared.ui.placeholder_cleanup,
        provider,
        channel_id,
        message_id,
        live_inflight,
    );
    let current_output_eof = cleanup_current_output_eof(shared, live_inflight, tmux_session_name);
    let live_committed_end =
        crate::services::discord::tmux::committed_frontier_for_current_generation(
            shared,
            channel_id,
            tmux_session_name,
        );
    let delivered_elsewhere = guarded_cleanup_delivered_elsewhere_signal(
        provider,
        channel_id,
        tmux_session_name,
        message_id,
        delivered_range,
        current_output_eof,
        live_committed_end,
    );
    let target_author = guarded_cleanup_target_author(live_inflight, message_id);
    // #4158 hardening: capture the positive-proof signal BEFORE the decision
    // consumes `delivered_elsewhere`, so the terminal-committed proof gate can
    // reason about it without a clone.
    let has_positive_delivered_elsewhere = matches!(
        delivered_elsewhere,
        GuardedDeliveredElsewhereSignal::Found { .. }
    );
    let decision = apply_terminal_committed_delete_proof_gate(
        guarded_nonterminal_delete_decision(
            provider,
            response_sent_offset,
            last_edit_text,
            committed_terminal_anchor,
            delivered_elsewhere,
            target_author,
        ),
        has_positive_delivered_elsewhere,
        require_delivered_elsewhere_proof,
    );
    match decision {
        GuardedNonterminalDeleteDecision::PreserveNoEdit { evidence } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                message_id = message_id.get(),
                evidence = %evidence,
                source = source,
                response_sent_offset = response_sent_offset,
                "  [{ts}] 👁 preserved delivered placeholder without edit during guarded cleanup for {tmux_session_name}"
            );
            None
        }
        GuardedNonterminalDeleteDecision::PreserveWithStrip {
            evidence,
            cleaned_body,
        } => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                message_id = message_id.get(),
                evidence = %evidence,
                source = source,
                response_sent_offset = response_sent_offset,
                "  [{ts}] 👁 preserved exposed placeholder with stripped streaming chrome during guarded cleanup for {tmux_session_name}"
            );
            let outcome = edit_preserve_placeholder(
                http,
                channel_id,
                shared,
                provider,
                tmux_session_name,
                message_id,
                &cleaned_body,
                source,
            )
            .await;
            if !outcome.is_committed() {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    message_id = message_id.get(),
                    source = source,
                    "  [{ts}] ⚠ guarded cleanup strip-preserve edit failed; keeping placeholder for {tmux_session_name}"
                );
            }
            None
        }
        GuardedNonterminalDeleteDecision::Delete { evidence } => {
            tracing::debug!(
                message_id = message_id.get(),
                evidence = %evidence,
                source = source,
                response_sent_offset = response_sent_offset,
                "guarded cleanup deleting placeholder for {tmux_session_name}"
            );
            Some(
                delete_placeholder_with_operation(
                    http,
                    channel_id,
                    shared,
                    provider,
                    tmux_session_name,
                    message_id,
                    operation,
                    source,
                )
                .await,
            )
        }
    }
}
/// #3871: which streamed rollover-prefix message ids the watcher MUST delete
/// after a terminal delivery so the frozen prefixes don't duplicate the body.
///
/// When a `>DISCORD_MSG_LIMIT` answer rolls over mid-stream, the prefix
/// placeholder is FROZEN as a standalone permanent message and a fresh
/// placeholder is opened for the remainder. The terminal full-body fallback
/// (`session_bound_fallback_uses_full_body`) re-posts the WHOLE body as ordered
/// chunks, so every frozen prefix is now a duplicate copy of bytes already in
/// the replay → delete them all (watcher parity with the sink's
/// `terminal_full_replay_cleanup_msg_ids`). On the remainder-only path
/// (`false`) the frozen prefixes carry the legit, already-delivered
/// `[0..response_sent_offset]` prose and MUST be preserved — return nothing.
pub(crate) fn watcher_rollover_prefixes_to_delete_on_terminal(
    session_bound_fallback_uses_full_body: bool,
    frozen_rollover_msg_ids: &[MessageId],
) -> Vec<MessageId> {
    if session_bound_fallback_uses_full_body {
        frozen_rollover_msg_ids.to_vec()
    } else {
        Vec::new()
    }
}

/// #3871: delete the streamed rollover-prefix messages the watcher froze during
/// streaming, after a terminal full-body replay re-posted their bytes. Mirrors
/// the sink's drain-and-delete of `terminal_full_replay_cleanup_msg_ids`; each
/// id is a non-terminal streamed prefix so `DeleteNonterminal` is used. No-op on
/// the remainder-only path (see [`watcher_rollover_prefixes_to_delete_on_terminal`]).
pub(crate) async fn delete_watcher_rollover_frozen_prefixes(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    session_bound_fallback_uses_full_body: bool,
    frozen_rollover_msg_ids: Vec<MessageId>,
) {
    for frozen_prefix in watcher_rollover_prefixes_to_delete_on_terminal(
        session_bound_fallback_uses_full_body,
        &frozen_rollover_msg_ids,
    ) {
        rate_limit_wait(shared, channel_id).await;
        let _ = delete_nonterminal_placeholder(
            http,
            channel_id,
            shared,
            provider,
            tmux_session_name,
            frozen_prefix,
            "watcher_terminal_rollover_prefix_dedup_3871",
        )
        .await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn delete_placeholder_with_operation(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    operation: PlaceholderCleanupOperation,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    let outcome = match channel_id.delete_message(http, message_id).await {
        Ok(_) => PlaceholderCleanupOutcome::Succeeded,
        Err(error) => classify_delete_error(&error.to_string()),
    };
    record_placeholder_cleanup(
        shared,
        provider,
        channel_id,
        message_id,
        tmux_session_name,
        operation,
        outcome.clone(),
        source,
    );
    outcome
}

#[allow(clippy::too_many_arguments)]
async fn edit_terminal_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    content: &str,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    edit_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        content,
        PlaceholderCleanupOperation::EditTerminal,
        source,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn edit_preserve_placeholder(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    content: &str,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    edit_placeholder_with_operation(
        http,
        channel_id,
        shared,
        provider,
        tmux_session_name,
        message_id,
        content,
        PlaceholderCleanupOperation::EditPreserve,
        source,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn edit_placeholder_with_operation(
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    tmux_session_name: &str,
    message_id: MessageId,
    content: &str,
    operation: PlaceholderCleanupOperation,
    source: &'static str,
) -> PlaceholderCleanupOutcome {
    rate_limit_wait(shared, channel_id).await;
    let outcome =
        match discord::http::edit_channel_message(http, channel_id, message_id, content).await {
            Ok(_) => PlaceholderCleanupOutcome::Succeeded,
            Err(error) => PlaceholderCleanupOutcome::failed(error.to_string()),
        };
    record_placeholder_cleanup(
        shared,
        provider,
        channel_id,
        message_id,
        tmux_session_name,
        operation,
        outcome.clone(),
        source,
    );
    outcome
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FallbackPlaceholderCleanupDecision {
    RelayCommitted,
    PreserveInflightForCleanupRetry,
}

pub(crate) fn fallback_placeholder_cleanup_decision(
    cleanup: &PlaceholderCleanupOutcome,
) -> FallbackPlaceholderCleanupDecision {
    if cleanup.is_committed() {
        FallbackPlaceholderCleanupDecision::RelayCommitted
    } else {
        FallbackPlaceholderCleanupDecision::PreserveInflightForCleanupRetry
    }
}
