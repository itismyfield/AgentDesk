use super::*;

/// #4229 W7b S-A: terminal token update and disabled auto-compact tail of
/// `tmux_output_watcher_with_restore`, moved verbatim from `tmux_watcher.rs`.
/// `is_prompt_too_long` and `tmux_session_name` exist only because they are
/// referenced by the `if false` disabled auto-compact branch (formerly
/// `tmux_watcher.rs:3501`, #227). Do not remove these fields before deleting that
/// branch; doing so causes `E0425`.
pub(super) struct WatcherTerminalTokenUpdateContext<'a> {
    pub(super) result_usage: Option<crate::db::turns::TurnTokenUsage>,
    pub(super) shared: &'a Arc<SharedData>,
    pub(super) channel_id: ChannelId,
    pub(super) watcher_direct_terminal_idle_committed: bool,
    pub(super) tmux_session_name: &'a String,
    pub(super) is_prompt_too_long: bool,
}

pub(super) async fn run_watcher_terminal_token_update(ctx: WatcherTerminalTokenUpdateContext<'_>) {
    let WatcherTerminalTokenUpdateContext {
        result_usage,
        shared,
        channel_id,
        watcher_direct_terminal_idle_committed,
        tmux_session_name,
        is_prompt_too_long,
    } = ctx;

    // Update session tokens from result event and auto-compact if threshold exceeded
    if let Some(tokens) = result_usage.map(|usage| usage.context_occupancy_input_tokens()) {
        let provider = shared.settings.read().await.provider.clone();
        let session_key = crate::services::discord::adk_session::build_adk_session_key(
            &shared, channel_id, &provider, None,
        )
        .await;
        let channel_name = {
            let data = shared.core.lock().await;
            data.sessions
                .get(&channel_id)
                .and_then(|s| s.channel_name.clone())
        };
        let thread_channel_id = channel_name
            .as_deref()
            .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name);
        let agent_id = resolve_role_binding(channel_id, channel_name.as_deref())
            .map(|binding| binding.role_id);
        crate::services::discord::adk_session::post_adk_session_status(
            session_key.as_deref(),
            channel_name.as_deref(),
            None,
            watcher_terminal_token_update_status(watcher_direct_terminal_idle_committed),
            &provider,
            None,
            Some(tokens),
            None,
            None,
            thread_channel_id,
            Some(channel_id),
            agent_id.as_deref(),
            shared.api_port,
        )
        .await;

        let ctx_cfg =
            crate::services::discord::adk_session::fetch_context_thresholds(shared.api_port).await;
        let pct = (tokens * 100) / ctx_cfg.context_window.max(1);
        // #227: Re-enabled with 5-min cooldown (matches turn_bridge path).
        // Without cooldown, the compact turn's own result could re-trigger compact.
        let cooldown_key = format!("auto_compact_cooldown:{}", channel_id.get());
        let cooldown_value =
            match crate::services::discord::internal_api::get_kv_value(&cooldown_key) {
                Ok(value) => value,
                Err(_) => {
                    if let Some(pg_pool) = shared.pg_pool.as_ref() {
                        sqlx::query_scalar::<_, Option<String>>(
                            "SELECT value
                         FROM kv_meta
                         WHERE key = $1
                           AND (expires_at IS NULL OR expires_at > NOW())
                         LIMIT 1",
                        )
                        .bind(&cooldown_key)
                        .fetch_optional(pg_pool)
                        .await
                        .ok()
                        .flatten()
                        .flatten()
                    } else {
                        None
                    }
                }
            };
        let compact_cooldown_ok =
            cooldown_value
                .and_then(|v| v.parse::<i64>().ok())
                .map_or(true, |ts| {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs() as i64;
                    now - ts > 300 // 5 min cooldown
                });
        // DISABLED — token counting still unreliable
        if false && pct >= ctx_cfg.compact_pct && !is_prompt_too_long && compact_cooldown_ok {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚡ [watcher] Auto-compact: {} at {pct}% ({tokens} tokens)",
                tmux_session_name
            );
            let name = tmux_session_name.clone();
            let _ = tokio::task::spawn_blocking(move || {
                crate::services::platform::tmux::send_keys(&name, &["/compact", "Enter"])
            })
            .await;
            // Set cooldown timestamp
            let cooldown_key = format!("auto_compact_cooldown:{}", channel_id.get());
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let now_text = now.to_string();
            if crate::services::discord::internal_api::set_kv_value(&cooldown_key, &now_text)
                .is_err()
            {
                if let Some(pg_pool) = shared.pg_pool.as_ref() {
                    let _ = sqlx::query(
                        "INSERT INTO kv_meta (key, value, expires_at)
                         VALUES ($1, $2, NULL)
                         ON CONFLICT (key) DO UPDATE
                         SET value = EXCLUDED.value,
                             expires_at = EXCLUDED.expires_at",
                    )
                    .bind(&cooldown_key)
                    .bind(&now_text)
                    .execute(pg_pool)
                    .await;
                }
            }
            // Notify: auto-compact triggered
            let target = format!("channel:{}", channel_id.get());
            let content = format!("🗜️ 자동 컨텍스트 압축 (사용률: {pct}%)");
            let _ = enqueue_outbox_best_effort(
                shared.pg_pool.as_ref(),
                OutboxMessage {
                    target: target.as_str(),
                    content: content.as_str(),
                    bot: "notify",
                    source: "system",
                    reason_code: None,
                    session_key: None,
                    attachment: None,
                },
            )
            .await;
        }
    }
}
