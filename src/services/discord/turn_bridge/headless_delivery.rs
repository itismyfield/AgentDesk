//! #3038 S1 headless placeholder cleanup and delivery helpers moved out of turn_bridge/mod.rs.

use super::*;

#[derive(Debug, Clone, PartialEq, Eq)]
enum HeadlessPlaceholderCleanupAction {
    Delete,
    Edit(String),
    Skip,
}

pub(super) fn is_synthetic_headless_message_id(message_id: MessageId) -> bool {
    super::is_synthetic_headless_message_id_raw(message_id.get())
}

/// Synthetic placeholder id used when a recovery turn has no anchored Discord
/// placeholder (current_msg_id == 0) and creating a fresh one failed. It sits in
/// the synthetic-headless range so `is_synthetic_headless_message_id` already
/// treats it as "no real Discord message to edit", driving placeholder
/// (re)creation on first streamed output instead of a doomed edit of a
/// nonexistent message. (A real Discord message id never reaches this range.)
pub(super) const SYNTHETIC_HEADLESS_RECOVERY_PLACEHOLDER_ID: u64 = 8_000_000_000_000_000_001;

fn is_codex_tool_log_marker_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    let Some(rest) = trimmed.strip_prefix('[') else {
        return false;
    };
    let Some((name, _)) = rest.split_once(']') else {
        return false;
    };
    const CODEX_TOOL_MARKER_NAMES: &[&str] = &[
        "bash",
        "read",
        "edit",
        "multiedit",
        "grep",
        "glob",
        "ls",
        "task",
        "todowrite",
        "webfetch",
        "websearch",
        "applypatch",
        "notebookread",
        "notebookedit",
    ];
    let normalized = name.trim().to_ascii_lowercase();
    normalized.starts_with("mcp__")
        || CODEX_TOOL_MARKER_NAMES
            .iter()
            .any(|candidate| *candidate == normalized)
}

fn strip_headless_placeholder_artifacts(text: &str, provider: &ProviderKind) -> String {
    let strip_codex_tool_logs = matches!(provider, ProviderKind::Codex);
    let mut kept = Vec::new();
    let mut pending_fence: Option<Vec<&str>> = None;

    for line in text.lines() {
        if let Some(fence) = pending_fence.as_mut() {
            fence.push(line);
            if line.trim_start().starts_with("```") {
                let inner = &fence[1..fence.len().saturating_sub(1)];
                let tool_log_only = strip_codex_tool_logs
                    && inner
                        .iter()
                        .filter(|inner_line| !inner_line.trim().is_empty())
                        .all(|inner_line| is_codex_tool_log_marker_line(inner_line));
                if !tool_log_only {
                    kept.extend(fence.iter().copied());
                }
                pending_fence = None;
            }
            continue;
        }

        let trimmed = line.trim();
        if super::formatting::is_streaming_placeholder_status_line(trimmed) {
            continue;
        }
        if strip_codex_tool_logs && is_codex_tool_log_marker_line(line) {
            continue;
        }
        if line.trim_start().starts_with("```") {
            pending_fence = Some(vec![line]);
            continue;
        }
        kept.push(line);
    }

    if let Some(fence) = pending_fence {
        let inner = &fence[1..];
        let tool_log_only = strip_codex_tool_logs
            && inner
                .iter()
                .filter(|inner_line| !inner_line.trim().is_empty())
                .all(|inner_line| is_codex_tool_log_marker_line(inner_line));
        if !tool_log_only {
            kept.extend(fence);
        }
    }

    kept.join("\n").trim().to_string()
}

fn headless_streaming_placeholder_cleanup_action(
    last_edit_text: &str,
    provider: &ProviderKind,
    status_panel_v2_enabled: bool,
    single_message_panel_footer_mode: bool,
) -> HeadlessPlaceholderCleanupAction {
    if single_message_panel_footer_mode {
        if let Some(cleaned) =
            super::finalize_bridge_streaming_footer(true, last_edit_text, provider)
        {
            return if cleaned == last_edit_text {
                HeadlessPlaceholderCleanupAction::Skip
            } else {
                HeadlessPlaceholderCleanupAction::Edit(cleaned)
            };
        }
        if let Some(cleaned) =
            crate::services::discord::single_message_panel::strip_streaming_footer(
                last_edit_text,
                provider,
            )
        {
            return if cleaned.trim().is_empty() {
                HeadlessPlaceholderCleanupAction::Delete
            } else if cleaned == last_edit_text {
                HeadlessPlaceholderCleanupAction::Skip
            } else {
                HeadlessPlaceholderCleanupAction::Edit(cleaned)
            };
        }
    }
    let mut cleaned = if status_panel_v2_enabled {
        super::formatting::format_for_discord_with_status_panel(last_edit_text, provider)
    } else {
        super::formatting::format_for_discord_with_provider(last_edit_text, provider)
    };
    if status_panel_v2_enabled {
        cleaned = strip_headless_placeholder_artifacts(&cleaned, provider);
    }
    if cleaned.trim().is_empty() {
        HeadlessPlaceholderCleanupAction::Delete
    } else if cleaned == last_edit_text {
        HeadlessPlaceholderCleanupAction::Skip
    } else {
        HeadlessPlaceholderCleanupAction::Edit(cleaned)
    }
}

pub(super) async fn cleanup_headless_streaming_placeholder_after_delivery(
    shared: &SharedData,
    channel_id: ChannelId,
    current_msg_id: MessageId,
    status_panel_msg_id: Option<MessageId>,
    last_edit_text: &str,
    provider: &ProviderKind,
) {
    if current_msg_id.get() == 0
        || status_panel_msg_id == Some(current_msg_id)
        || is_synthetic_headless_message_id(current_msg_id)
    {
        return;
    }
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return;
    };
    match headless_streaming_placeholder_cleanup_action(
        last_edit_text,
        provider,
        shared.ui.status_panel_v2_enabled,
        super::bridge_single_message_panel_footer_enabled(shared.ui.status_panel_v2_enabled),
    ) {
        HeadlessPlaceholderCleanupAction::Delete => {
            if let Err(error) =
                super::http::delete_channel_message(&http, channel_id, current_msg_id).await
            {
                tracing::warn!(
                    "[turn_bridge] failed to delete stale headless streaming placeholder {} in channel {}: {}",
                    current_msg_id,
                    channel_id,
                    error
                );
            }
        }
        HeadlessPlaceholderCleanupAction::Edit(cleaned) => {
            if let Err(error) =
                super::http::edit_channel_message(&http, channel_id, current_msg_id, &cleaned).await
            {
                tracing::warn!(
                    "[turn_bridge] failed to clean stale headless streaming placeholder {} in channel {}: {}",
                    current_msg_id,
                    channel_id,
                    error
                );
            }
        }
        HeadlessPlaceholderCleanupAction::Skip => {}
    }
}

/// The caller-supplied delivery identity, once trimmed and emptiness-filtered.
///
/// In production this is `InflightTurnState::delivery_bot`, written only by
/// `router::message_handler::headless_turn` from turn metadata that the routine
/// runtime (`routines::agent_executor`) puts there. Ordinary user turns and
/// restart-recovery turns carry no such metadata, so this is `None` for them.
fn caller_supplied_delivery_bot(delivery_bot: Option<&str>) -> Option<&str> {
    delivery_bot
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// #5159: which bot identity a headless terminal delivery posts under.
///
/// `enqueue_headless_delivery` receives the turn's answer body from the
/// `can_chain_locally == false` arm of `terminal_outcome_delivery`. A false
/// `can_chain_locally` only says "this turn has no live Discord context to edit
/// its placeholder through"; it says nothing about the content being an
/// operational notice. Delivery-path selection and identity selection are
/// therefore decided separately here.
///
/// Precedence:
/// 1. a caller-supplied identity — the routine runtime targets an explicit bot;
/// 2. otherwise the turn's own provider alias, which is exactly the identity the
///    `can_chain_locally == true` arm posts under (`DiscordGateway` edits the
///    placeholder with that provider's `serenity::Http`, registered under
///    `ProviderKind::as_str` by `runtime_bootstrap::framework_setup`), and the
///    same alias `message_outbox::delivery_bot_for_target_session` already
///    rewrites DM-session rows to;
/// 3. the notify utility bot, only for a provider outside the provider registry,
///    which has no provider-bot alias to post under.
fn headless_delivery_bot_alias<'a>(
    delivery_bot: Option<&'a str>,
    provider: &'a ProviderKind,
) -> &'a str {
    if let Some(explicit) = caller_supplied_delivery_bot(delivery_bot) {
        return explicit;
    }
    if provider.is_supported() {
        return provider.as_str();
    }
    super::super::bot_role::UtilityBotRole::Notify.alias()
}

/// Whether the direct (non-outbox) fallback at the end of
/// `enqueue_headless_delivery` keeps its legacy notify-utility http preference.
///
/// It does so only for a caller-supplied identity, leaving the routine runtime's
/// fallback behaviour exactly as it was. When the identity came from the turn's
/// own provider (#5159's user-turn default) that preference is dropped, because
/// the `shared` http this function falls through to already *is* that provider's
/// bot — the identity the placeholder-edit arm would have posted under.
fn headless_direct_fallback_prefers_notify_http(delivery_bot: Option<&str>) -> bool {
    caller_supplied_delivery_bot(delivery_bot).is_some()
}

mod durable_outbox;
mod intake_outbox_argument;
use intake_outbox_argument::HeadlessDeliveryRuntimeArguments;
pub(super) use intake_outbox_argument::{
    HeadlessDeliveryArguments, HeadlessDeliveryInputs, assemble_headless_delivery_arguments,
};

mod outcome;
pub(crate) use outcome::{
    HeadlessDeliveryDisposition, HeadlessDeliveryOutcome, headless_delivery_disposition,
    preserve_ambiguous_headless_delivery_for_retry,
};
use outcome::{classify_visible_outbox_result, run_headless_direct_fallback};

pub(super) async fn enqueue_headless_delivery(
    arguments: HeadlessDeliveryArguments<'_>,
) -> HeadlessDeliveryOutcome {
    let HeadlessDeliveryRuntimeArguments {
        shared,
        channel_id,
        owning_user_msg_id,
        session_key,
        delivery_bot,
        provider,
        born_generation,
        content,
        cancel_token,
    } = arguments.into_runtime_arguments();
    let target = format!("channel:{}", channel_id.get());
    let bot = headless_delivery_bot_alias(delivery_bot, provider);

    let outbox_message = crate::services::message_outbox::OutboxMessage {
        target: &target,
        content,
        bot,
        source: "headless_turn",
        // Explicit reason_code keeps dedupe consistent across PG/SQLite.
        reason_code: Some("headless.delivery"),
        session_key,
        attachment: None,
    };
    if let Some(pool) = shared.pg_pool.as_ref() {
        let delivery_cancel_token = cancel_token.filter(|token| !token.is_completion_cleanup());
        // Terminal headless responses are per-turn facts. Identical content in
        // back-to-back E2E or operator turns must still be delivered.
        match durable_outbox::enqueue_headless_outbox(
            pool,
            outbox_message,
            owning_user_msg_id,
            provider,
            born_generation,
            delivery_cancel_token,
        )
        .await
        {
            Ok(crate::services::message_outbox::OutboxEnqueueOutcome::Enqueued {
                id: outbox_id,
            }) => {
                if let Some(session_key) =
                    session_key.map(str::trim).filter(|value| !value.is_empty())
                {
                    let thread_channel_id = channel_id.get().to_string();
                    // #2838/#2950: once enqueue returned Ok(Some(outbox_id))
                    // the outbox row exists, but visible completion must still
                    // wait for the notify-bot worker to mark that row sent.
                    // The delivery marker below is best-effort dedup bookkeeping;
                    // propagating a marker failure as a delivery Err makes the
                    // caller preserve inflight, which then re-delivers via
                    // recovery (duplicate) AND stalls the queue. So every
                    // post-enqueue marker failure logs and still falls through
                    // to the outbox visibility wait. Only genuine non-delivery
                    // (failed outbox visibility or failed direct fallback below)
                    // returns Err.
                    match pool.begin().await {
                        Ok(mut tx) => {
                            if let Err(error) =
                                sqlx::query("SELECT pg_advisory_xact_lock(1752, hashtext($1))")
                                    .bind(&thread_channel_id)
                                    .execute(&mut *tx)
                                    .await
                            {
                                let _ = tx.rollback().await;
                                tracing::warn!(
                                    "[outbox] terminal delivery marker lock failed for session {session_key} (outbox {outbox_id} already enqueued; waiting for visible delivery): {error}"
                                );
                            } else {
                                let active_user_message_id =
                                    super::mailbox_snapshot(shared.as_ref(), channel_id)
                                        .await
                                        .active_user_message_id;
                                if let Some(active_user_message_id) = active_user_message_id
                                    && Some(active_user_message_id) != owning_user_msg_id
                                {
                                    tracing::warn!(
                                        "[outbox] skipped terminal delivery marker {} for session {} because active turn message changed from {:?} to {}",
                                        outbox_id,
                                        session_key,
                                        owning_user_msg_id.map(|id| id.get()),
                                        active_user_message_id.get()
                                    );
                                } else if let Err(error) = sqlx::query(
                                    "UPDATE sessions
                                            SET active_turn_delivery_outbox_id = $1
                                          WHERE session_key = $2
                                            AND thread_channel_id = $3
                                            AND status IN ('turn_active', 'working')",
                                )
                                .bind(outbox_id)
                                .bind(session_key)
                                .bind(&thread_channel_id)
                                .execute(&mut *tx)
                                .await
                                {
                                    let _ = tx.rollback().await;
                                    tracing::warn!(
                                        "[outbox] terminal delivery marker write failed for session {session_key} row {outbox_id} (already enqueued; waiting for visible delivery): {error}"
                                    );
                                    return classify_visible_outbox_result(
                                        durable_outbox::wait_for_headless_delivery_outbox_visible(
                                            pool,
                                            outbox_id,
                                            durable_outbox::HEADLESS_DELIVERY_OUTBOX_VISIBLE_TIMEOUT,
                                        )
                                        .await,
                                    );
                                }
                                if let Err(error) = tx.commit().await {
                                    tracing::warn!(
                                        "[outbox] terminal delivery marker commit failed for session {session_key} (outbox {outbox_id} already enqueued; waiting for visible delivery): {error}"
                                    );
                                }
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                "[outbox] terminal delivery marker tx begin failed for session {session_key} (outbox {outbox_id} already enqueued; waiting for visible delivery): {error}"
                            );
                        }
                    }
                }
                return classify_visible_outbox_result(
                    durable_outbox::wait_for_headless_delivery_outbox_visible(
                        pool,
                        outbox_id,
                        durable_outbox::HEADLESS_DELIVERY_OUTBOX_VISIBLE_TIMEOUT,
                    )
                    .await,
                );
            }
            Ok(crate::services::message_outbox::OutboxEnqueueOutcome::Cancelled) => {
                tracing::info!(
                    channel_id = channel_id.get(),
                    session_key,
                    "skipped headless delivery after outbox enqueue observed cancellation"
                );
                return HeadlessDeliveryOutcome::Cancelled;
            }
            Ok(crate::services::message_outbox::OutboxEnqueueOutcome::NoRow) => {
                tracing::warn!(
                    channel_id = channel_id.get(),
                    session_key,
                    "headless outbox enqueue returned no row; preserving inflight for retry"
                );
                return HeadlessDeliveryOutcome::Ambiguous {
                    surfaced_error: None,
                };
            }
            Err(error) => {
                tracing::warn!(
                    "[outbox] postgres enqueue failed for terminal response on channel {}: {}",
                    channel_id,
                    error
                );
            }
        }
    }

    let suppress_for_cancel = should_suppress_headless_delivery_for_cancel(cancel_token);
    if suppress_for_cancel {
        tracing::info!(
            channel_id = channel_id.get(),
            session_key,
            "skipped headless direct fallback after turn cancellation"
        );
    }

    let direct_fallback = async {
        let notify_http = match shared
            .health_registry()
            .filter(|_| headless_direct_fallback_prefers_notify_http(delivery_bot))
        {
            Some(registry) => match super::health::resolve_utility_bot_http(
                registry.as_ref(),
                super::bot_role::UtilityBotRole::Notify,
            )
            .await
            {
                Ok(http) => Some(http),
                Err((status, body)) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ headless notify bot unavailable in channel {}: {} {} — falling back to provider bot",
                        channel_id,
                        status,
                        body
                    );
                    None
                }
            },
            None => None,
        };

        // Phase 5.2 of intake-node-routing (issue #2009): use gateway-or-token
        // fallback so the standby worker path can still deliver headless
        // messages even when `cached_serenity_ctx` is None.
        let http = notify_http
            .or_else(|| shared.serenity_http_or_token_fallback())
            .ok_or_else(|| {
                format!(
                    "headless delivery unavailable for channel {}: no outbox storage or discord http",
                    channel_id.get()
                )
            })?;
        send_long_message_raw(&http, channel_id, content, shared)
            .await
            .map_err(|error| format!("headless direct delivery failed: {error}"))?;
        Ok(())
    };
    run_headless_direct_fallback(suppress_for_cancel, || direct_fallback).await
}

#[cfg(test)]
#[path = "headless_delivery/production_seam_tests.rs"]
mod production_seam_tests;

#[cfg(test)]
mod headless_delivery_tests {
    use super::*;

    /// #5159 regression guard. A user turn (and a restart-recovery turn) reaches
    /// the headless outbox arm with `delivery_bot == None`, because only routine
    /// metadata ever writes `InflightTurnState::delivery_bot`. Before this fix
    /// the decision was `delivery_bot.unwrap_or(Notify.alias())`, so the turn's
    /// own answer body was posted by the notify utility bot. It must post as
    /// the agent bot instead — the identity the placeholder-edit arm uses.
    #[test]
    fn user_turn_answer_posts_as_the_agent_bot_not_notify() {
        for provider in [
            ProviderKind::Claude,
            ProviderKind::Codex,
            ProviderKind::Gemini,
            ProviderKind::OpenCode,
            ProviderKind::Qwen,
        ] {
            let bot = headless_delivery_bot_alias(None, &provider);
            assert_eq!(
                bot,
                provider.as_str(),
                "user turn answer must carry the agent identity for {provider:?}"
            );
            assert_ne!(
                bot,
                super::super::super::bot_role::UtilityBotRole::Notify.alias(),
                "user turn answer must not be posted by the notify utility bot"
            );
        }

        // Blank/whitespace metadata is not an identity either — it must not
        // reopen the notify default.
        for blank in ["", "   ", "\t"] {
            assert_eq!(
                headless_delivery_bot_alias(Some(blank), &ProviderKind::Claude),
                "claude"
            );
        }
    }

    /// #5159 non-regression guard: a routine-originated headless turn carries an
    /// explicit `delivery_bot` (`routines::agent_executor` → turn metadata →
    /// `router::message_handler::headless_turn`). That identity always wins,
    /// including when it is the notify bot itself.
    #[test]
    fn routine_supplied_delivery_bot_is_never_overridden() {
        for (supplied, expected) in [
            ("notify", "notify"),
            ("announce", "announce"),
            ("dm", "dm"),
            ("routine-thread-bot", "routine-thread-bot"),
            ("  notify  ", "notify"),
        ] {
            assert_eq!(
                headless_delivery_bot_alias(Some(supplied), &ProviderKind::Claude),
                expected,
                "routine identity {supplied:?} must survive the provider default"
            );
        }
        // Also for a provider the registry does not know.
        assert_eq!(
            headless_delivery_bot_alias(
                Some("notify"),
                &ProviderKind::Unsupported("weirdcli".to_string())
            ),
            "notify"
        );
    }

    /// A provider outside the provider registry has no provider-bot alias, so
    /// the notify utility bot stays the last resort there.
    #[test]
    fn unregistered_provider_still_falls_back_to_the_notify_bot() {
        let provider = ProviderKind::Unsupported("weirdcli".to_string());
        assert!(!provider.is_supported());
        assert_eq!(
            headless_delivery_bot_alias(None, &provider),
            super::super::super::bot_role::UtilityBotRole::Notify.alias()
        );
    }

    #[test]
    fn headless_cleanup_deletes_status_only_codex_placeholder() {
        let action = headless_streaming_placeholder_cleanup_action(
            "[Bash] /bin/zsh -lc 'cargo test'\n⠙ Processing...",
            &ProviderKind::Codex,
            true,
            false,
        );

        assert_eq!(action, HeadlessPlaceholderCleanupAction::Delete);
    }

    #[test]
    fn headless_cleanup_deletes_processing_with_codex_tool_log_block() {
        let action = headless_streaming_placeholder_cleanup_action(
            "⠂ Processing...\n\n```\n[Bash] /bin/zsh -lc 'grep -n foo src/lib.rs'\n[Bash] /bin/zsh -lc \"sed -n '1,40p' src/lib.rs\"\n```",
            &ProviderKind::Codex,
            true,
            false,
        );

        assert_eq!(action, HeadlessPlaceholderCleanupAction::Delete);
    }

    #[test]
    fn headless_cleanup_edits_partial_answer_without_processing_footer() {
        let action = headless_streaming_placeholder_cleanup_action(
            "partial answer\n\n⠙ Processing...",
            &ProviderKind::Codex,
            true,
            false,
        );

        assert_eq!(
            action,
            HeadlessPlaceholderCleanupAction::Edit("partial answer".to_string())
        );
    }

    #[test]
    fn headless_cleanup_edits_partial_answer_without_korean_processing_footer() {
        let action = headless_streaming_placeholder_cleanup_action(
            "partial answer\n\n⠙ 계속 처리 중",
            &ProviderKind::Codex,
            true,
            false,
        );

        assert_eq!(
            action,
            HeadlessPlaceholderCleanupAction::Edit("partial answer".to_string())
        );
    }

    #[test]
    fn headless_cleanup_strips_single_message_panel_footer() {
        let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
        let footer =
            crate::services::discord::single_message_panel::compose_footer_status_block("⠸", panel);
        let action = headless_streaming_placeholder_cleanup_action(
            &format!("partial answer\n\n{footer}"),
            &ProviderKind::Claude,
            true,
            true,
        );

        assert_eq!(
            action,
            HeadlessPlaceholderCleanupAction::Edit("partial answer".to_string())
        );
    }
}
