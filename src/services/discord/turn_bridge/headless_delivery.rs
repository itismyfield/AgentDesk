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

pub(super) async fn enqueue_headless_delivery(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    // `None` for a recovery turn with no anchored user message (user_msg_id == 0).
    owning_user_msg_id: Option<MessageId>,
    session_key: Option<&str>,
    delivery_bot: Option<&str>,
    content: &str,
    cancel_token: Option<&CancelToken>,
) -> Result<(), String> {
    let target = format!("channel:{}", channel_id.get());
    let bot = delivery_bot
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(super::super::bot_role::UtilityBotRole::Notify.alias());

    let outbox_message = crate::services::message_outbox::OutboxMessage {
        target: &target,
        content,
        bot,
        source: "headless_turn",
        // Explicit reason_code keeps dedupe consistent across PG/SQLite.
        reason_code: Some("headless.delivery"),
        session_key,
    };
    if let Some(pool) = shared.pg_pool.as_ref() {
        let delivery_cancel_token = cancel_token.filter(|token| !token.is_completion_cleanup());
        // Terminal headless responses are per-turn facts. Identical content in
        // back-to-back E2E or operator turns must still be delivered.
        match crate::services::message_outbox::enqueue_outbox_pg_returning_id_with_ttl_and_cancel(
            pool,
            outbox_message,
            0,
            delivery_cancel_token,
        )
        .await
        {
            Ok(Some(outbox_id)) => {
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
                                    return wait_for_headless_delivery_outbox_visible(
                                        pool,
                                        outbox_id,
                                        HEADLESS_DELIVERY_OUTBOX_VISIBLE_TIMEOUT,
                                    )
                                    .await;
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
                return wait_for_headless_delivery_outbox_visible(
                    pool,
                    outbox_id,
                    HEADLESS_DELIVERY_OUTBOX_VISIBLE_TIMEOUT,
                )
                .await;
            }
            Ok(None) => {
                tracing::info!(
                    channel_id = channel_id.get(),
                    session_key,
                    "skipped headless direct fallback after outbox enqueue returned no row"
                );
                return Ok(());
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

    if should_suppress_headless_delivery_for_cancel(cancel_token) {
        tracing::info!(
            channel_id = channel_id.get(),
            session_key,
            "skipped headless direct fallback after turn cancellation"
        );
        return Ok(());
    }

    let notify_http = if let Some(registry) = shared.health_registry() {
        match super::health::resolve_utility_bot_http(
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
        }
    } else {
        None
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
}

const HEADLESS_DELIVERY_OUTBOX_VISIBLE_TIMEOUT: std::time::Duration =
    std::time::Duration::from_secs(30);
const HEADLESS_DELIVERY_OUTBOX_VISIBLE_POLL: std::time::Duration =
    std::time::Duration::from_millis(100);

async fn wait_for_headless_delivery_outbox_visible(
    pool: &sqlx::PgPool,
    outbox_id: i64,
    timeout: std::time::Duration,
) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let row = sqlx::query("SELECT status, error FROM message_outbox WHERE id = $1")
            .bind(outbox_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| {
                format!("poll headless delivery outbox row {outbox_id} failed: {error}")
            })?;
        let Some(row) = row else {
            return Err(format!(
                "headless delivery outbox row {outbox_id} disappeared before visible delivery"
            ));
        };
        let status: String = row
            .try_get("status")
            .map_err(|error| format!("read headless outbox row {outbox_id} status: {error}"))?;
        match status.as_str() {
            "sent" => return Ok(()),
            "failed" => {
                let error: Option<String> = row.try_get("error").ok().flatten();
                return Err(format!(
                    "headless delivery outbox row {outbox_id} failed before visible delivery: {}",
                    error.unwrap_or_else(|| "unknown error".to_string())
                ));
            }
            _ => {}
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(format!(
                "headless delivery outbox row {outbox_id} remained {status} for {}s before visible delivery",
                timeout.as_secs()
            ));
        }
        tokio::time::sleep(HEADLESS_DELIVERY_OUTBOX_VISIBLE_POLL.min(deadline - now)).await;
    }
}

#[cfg(test)]
mod headless_delivery_tests {
    use super::*;

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
