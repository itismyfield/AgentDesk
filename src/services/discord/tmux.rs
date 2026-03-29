use std::sync::Arc;
use std::sync::atomic::Ordering;

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use crate::services::claude;
use crate::services::provider::parse_provider_and_channel_from_tmux_name;
use crate::services::tmux_diagnostics::{
    build_tmux_death_diagnostic, read_tmux_exit_reason, record_tmux_exit_reason,
    tmux_session_exists, tmux_session_has_live_pane,
};

use super::formatting::{
    format_for_discord, format_tool_input, normalize_empty_lines, send_long_message_raw,
};
use super::settings::{channel_supports_provider, resolve_role_binding};
use super::{DISCORD_MSG_LIMIT, SharedData, TmuxWatcherHandle, rate_limit_wait};

use crate::utils::format::tail_with_ellipsis;

use crate::services::tmux_common::{current_tmux_owner_marker, tmux_exact_target, tmux_owner_path};

fn session_belongs_to_current_runtime(session_name: &str, current_owner_marker: &str) -> bool {
    std::fs::read_to_string(tmux_owner_path(session_name))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| value == current_owner_marker)
        .unwrap_or(false)
}

/// Background watcher that continuously tails a tmux output file.
/// When Claude produces output from terminal input (not Discord), relay it to Discord.
pub(super) async fn tmux_output_watcher(
    channel_id: ChannelId,
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    output_path: String,
    tmux_session_name: String,
    initial_offset: u64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    pause_epoch: Arc<std::sync::atomic::AtomicU64>,
    turn_delivered: Arc<std::sync::atomic::AtomicBool>,
) {
    use claude::StreamLineState;
    use std::io::{Read, Seek, SeekFrom};

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] 👁 tmux watcher started for #{tmux_session_name} at offset {initial_offset}");

    let mut current_offset = initial_offset;
    let mut prompt_too_long_killed = false;
    // Guard against duplicate relay: track the offset from which the last relay was sent.
    // If the outer loop circles back and current_offset hasn't advanced past this point,
    // the relay is suppressed.
    let mut last_relayed_offset: Option<u64> = None;

    loop {
        // Always consume resume_offset first — the turn bridge may have set it
        // between the previous paused check and now, so reading it here prevents
        // the watcher from using a stale current_offset after unpausing.
        if let Some(new_offset) = resume_offset.lock().ok().and_then(|mut g| g.take()) {
            current_offset = new_offset;
            // Clear turn_delivered: the watcher is now starting from a fresh offset
            // set by the turn bridge, so future data at this offset is safe to relay.
            turn_delivered.store(false, Ordering::Relaxed);
            // Reset duplicate-relay guard: new offset means new data range.
            last_relayed_offset = None;
        }

        // Check cancel or global shutdown (both exit quietly, no "session ended" message)
        if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
            break;
        }

        // If paused (Discord handler is processing its own turn), wait
        if paused.load(Ordering::Relaxed) {
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            continue;
        }

        // Snapshot pause epoch — if this changes later, a Discord turn claimed this data
        let epoch_snapshot = pause_epoch.load(Ordering::Relaxed);

        // Check if tmux session is still alive (with timeout to prevent
        // blocking thread pool exhaustion if tmux hangs)
        let alive = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking({
                let name = tmux_session_name.clone();
                move || tmux_session_has_live_pane(&name)
            }),
        )
        .await
        .unwrap_or(Ok(false))
        .unwrap_or(false);

        if !alive {
            // Re-check shutdown/cancel — SIGTERM handler may have set the flag
            // between the top-of-loop check and here
            if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
                );
                break;
            }
            // Extra grace: wait briefly and re-check, since SIGTERM handler is async
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
                );
                break;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            if let Some(diag) = build_tmux_death_diagnostic(&tmux_session_name, Some(&output_path))
            {
                println!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended, watcher stopping ({diag})"
                );
            } else {
                println!("  [{ts}] 👁 tmux session {tmux_session_name} ended, watcher stopping");
            }
            if !prompt_too_long_killed {
                // Suppress warning for normal dispatch completion — not an error
                let is_normal_completion = read_tmux_exit_reason(&tmux_session_name)
                    .map(|r| r.contains("dispatch turn completed"))
                    .unwrap_or(false);
                if !is_normal_completion {
                    let _ = channel_id
                        .say(
                            &http,
                            "⚠️ 작업 세션이 종료되었습니다. 다음 메시지를 보내면 새 세션이 시작됩니다.",
                        )
                        .await;
                }
            }
            break;
        }

        // Try to read new data from output file
        let read_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking({
                let path = output_path.clone();
                let offset = current_offset;
                move || -> Result<(Vec<u8>, u64), String> {
                    let mut file =
                        std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
                    file.seek(SeekFrom::Start(offset))
                        .map_err(|e| format!("seek: {}", e))?;
                    let mut buf = vec![0u8; 16384];
                    let n = file.read(&mut buf).map_err(|e| format!("read: {}", e))?;
                    buf.truncate(n);
                    Ok((buf, offset + n as u64))
                }
            }),
        )
        .await;

        let (data, new_offset) = match read_result {
            Ok(Ok(Ok((data, off)))) => (data, off),
            _ => {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                continue;
            }
        };

        if data.is_empty() {
            // No new data, sleep and retry
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            continue;
        }

        // We got new data while not paused — this means terminal input triggered a response
        let data_start_offset = current_offset; // offset where this read batch started
        current_offset = new_offset;

        // Collect the full turn: keep reading until we see a "result" event
        let mut all_data = String::from_utf8_lossy(&data).to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        // Create a placeholder message for real-time status display
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let mut spin_idx: usize = 0;
        let mut placeholder_msg_id: Option<serenity::MessageId> = None;
        let mut last_edit_text = String::new();

        // Process any complete lines we already have
        let (mut found_result, mut is_prompt_too_long, mut is_auth_error, mut result_tokens) =
            process_watcher_lines(
                &mut all_data,
                &mut state,
                &mut full_response,
                &mut tool_state,
            );

        // Keep reading until result or timeout
        // Check if a Discord turn claimed this data since our epoch snapshot
        let epoch_changed = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
        let mut was_paused = paused.load(Ordering::Relaxed) || epoch_changed;
        if was_paused {
            // A Discord turn took over — discard what we read
            continue;
        }
        if !found_result {
            let turn_start = tokio::time::Instant::now();
            let turn_timeout = super::turn_watchdog_timeout();
            let mut last_status_update = tokio::time::Instant::now();

            while !found_result && turn_start.elapsed() < turn_timeout {
                if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
                    break;
                }
                if paused.load(Ordering::Relaxed) {
                    was_paused = true;
                    break;
                }

                let read_more = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    tokio::task::spawn_blocking({
                        let path = output_path.clone();
                        let offset = current_offset;
                        move || -> Result<(Vec<u8>, u64), String> {
                            let mut file =
                                std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
                            file.seek(SeekFrom::Start(offset))
                                .map_err(|e| format!("seek: {}", e))?;
                            let mut buf = vec![0u8; 16384];
                            let n = file.read(&mut buf).map_err(|e| format!("read: {}", e))?;
                            buf.truncate(n);
                            Ok((buf, offset + n as u64))
                        }
                    }),
                )
                .await;

                match read_more {
                    Ok(Ok(Ok((chunk, off)))) if !chunk.is_empty() => {
                        current_offset = off;
                        all_data.push_str(&String::from_utf8_lossy(&chunk));
                        let (fr, ptl, ae, rt) = process_watcher_lines(
                            &mut all_data,
                            &mut state,
                            &mut full_response,
                            &mut tool_state,
                        );
                        found_result = fr;
                        is_prompt_too_long = is_prompt_too_long || ptl;
                        is_auth_error = is_auth_error || ae;
                        if rt.is_some() {
                            result_tokens = rt;
                        }
                    }
                    _ => {
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    }
                }

                // Check for stale session error during streaming — abort relay immediately
                if full_response.contains("No conversation found")
                    || full_response.contains("Error: No conversation")
                {
                    found_result = true; // Exit the loop
                    break;
                }

                // Update Discord placeholder at configurable interval
                if last_status_update.elapsed() >= super::status_update_interval() {
                    last_status_update = tokio::time::Instant::now();
                    let indicator = SPINNER[spin_idx % SPINNER.len()];
                    spin_idx += 1;

                    let raw_tool_status = super::formatting::resolve_raw_tool_status(
                        tool_state.current_tool_line.as_deref(),
                        &full_response,
                    );
                    let tool_status = super::formatting::humanize_tool_status(raw_tool_status);
                    let footer = format!("\n\n{} {}", indicator, tool_status);
                    let body_budget = DISCORD_MSG_LIMIT.saturating_sub(footer.len() + 10);
                    let display_text = if full_response.is_empty() {
                        format!("{} {}", indicator, tool_status)
                    } else {
                        let normalized = normalize_empty_lines(&full_response);
                        let body = tail_with_ellipsis(&normalized, body_budget.max(1));
                        format!("{}{}", body, footer)
                    };

                    if display_text != last_edit_text {
                        match placeholder_msg_id {
                            Some(msg_id) => {
                                // Edit existing placeholder
                                rate_limit_wait(&shared, channel_id).await;
                                let _ = channel_id
                                    .edit_message(
                                        &http,
                                        msg_id,
                                        serenity::EditMessage::new().content(&display_text),
                                    )
                                    .await;
                            }
                            None => {
                                // Create new placeholder
                                if let Ok(msg) = channel_id.say(&http, &display_text).await {
                                    placeholder_msg_id = Some(msg.id);
                                }
                            }
                        }
                        last_edit_text = display_text;
                    }
                }
            }
        }

        // If paused was set while we were reading (even if already unpaused), discard partial data.
        // Also check epoch: if it changed, a Discord turn claimed this data even if paused is now false.
        if was_paused
            || paused.load(Ordering::Relaxed)
            || pause_epoch.load(Ordering::Relaxed) != epoch_snapshot
        {
            // Clean up placeholder if we created one
            if let Some(msg_id) = placeholder_msg_id {
                let _ = channel_id.delete_message(&http, msg_id).await;
            }
            continue;
        }

        // Handle prompt-too-long: kill session so next message creates a fresh one
        if is_prompt_too_long {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 👁 Prompt too long detected in watcher for {tmux_session_name}, killing session"
            );
            prompt_too_long_killed = true;

            let sess = tmux_session_name.clone();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    record_tmux_exit_reason(&sess, "watcher cleanup: prompt too long");
                    crate::services::platform::tmux::kill_session(&sess);
                }),
            )
            .await;

            let notice = "⚠️ 컨텍스트 한도 초과로 세션을 초기화했습니다. 다음 메시지부터 새 세션으로 처리됩니다.";
            match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    let _ = channel_id
                        .edit_message(&http, msg_id, serenity::EditMessage::new().content(notice))
                        .await;
                }
                None => {
                    let _ = channel_id.say(&http, notice).await;
                }
            }
            // Don't break — let the watcher exit naturally when session-alive check fails
            continue;
        }

        // Handle auth error: kill session and notify user to re-authenticate
        if is_auth_error {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 👁 Auth error detected in watcher for {tmux_session_name}, killing session"
            );
            prompt_too_long_killed = true; // reuse flag to suppress duplicate "session ended" message

            let sess = tmux_session_name.clone();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    record_tmux_exit_reason(&sess, "watcher cleanup: authentication failed");
                    crate::services::platform::tmux::kill_session(&sess);
                }),
            )
            .await;

            let notice = "⚠️ 인증이 만료되었습니다. 세션을 종료합니다.\n관리자가 CLI에서 재인증(`/login`)을 완료한 후 다시 시도해주세요.";
            match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    let _ = channel_id
                        .edit_message(&http, msg_id, serenity::EditMessage::new().content(notice))
                        .await;
                }
                None => {
                    let _ = channel_id.say(&http, notice).await;
                }
            }
            continue;
        }

        // Final guard: re-check epoch and turn_delivered right before relay.
        // Closes the race window where a Discord turn starts between the epoch check
        // above (line 277) and this relay — the turn_bridge may have already delivered
        // the same response to its own placeholder.
        if paused.load(Ordering::Relaxed)
            || pause_epoch.load(Ordering::Relaxed) != epoch_snapshot
            || turn_delivered.load(Ordering::Relaxed)
        {
            if let Some(msg_id) = placeholder_msg_id {
                let _ = channel_id.delete_message(&http, msg_id).await;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] 👁 Late epoch/delivered guard: suppressed duplicate relay for {}",
                tmux_session_name
            );
            continue;
        }

        // Duplicate-relay guard: if we already relayed from this same data range, suppress.
        if let Some(prev_offset) = last_relayed_offset {
            if data_start_offset <= prev_offset {
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!(
                    "  [{ts}] 👁 Duplicate relay guard: suppressed re-relay for {} (data_start={}, last_relayed={})",
                    tmux_session_name, data_start_offset, prev_offset
                );
                if let Some(msg_id) = placeholder_msg_id {
                    let _ = channel_id.delete_message(&http, msg_id).await;
                }
                continue;
            }
        }

        // Detect stale session resume failure in watcher output
        let is_stale_resume = full_response.contains("No conversation found")
            || full_response.contains("Error: No conversation");
        if is_stale_resume {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] ⚠ Watcher detected stale session resume failure (channel {}), clearing session_id",
                channel_id
            );
            let stale_sid = {
                let mut data = shared.core.lock().await;
                let old = data
                    .sessions
                    .get(&channel_id)
                    .and_then(|s| s.session_id.clone());
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.session_id = None;
                }
                old
            };
            // Clear DB session_id
            {
                let hostname = crate::services::platform::hostname_short();
                let session_key = format!("{}:{}", hostname, tmux_session_name);
                super::adk_session::clear_claude_session_id(&session_key, shared.api_port).await;
            }
            if let Some(ref sid) = stale_sid {
                let _ = reqwest::Client::new()
                    .post(crate::config::local_api_url(
                        shared.api_port,
                        "/api/dispatched-sessions/clear-stale-session-id",
                    ))
                    .json(&serde_json::json!({"claude_session_id": sid}))
                    .send()
                    .await;
            }
            record_tmux_exit_reason(
                &tmux_session_name,
                "stale session resume detected — forcing fresh session before auto-retry",
            );
            crate::services::platform::tmux::kill_session(&tmux_session_name);
            // Replace placeholder with recovery notice (don't delete — avoids visual gap)
            if let Some(msg_id) = placeholder_msg_id {
                let _ = channel_id
                    .edit_message(
                        &http,
                        msg_id,
                        serenity::EditMessage::new()
                            .content("↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다."),
                    )
                    .await;
            }
            // Auto-retry: fetch Discord history, store in kv_meta for LLM injection,
            // then re-send only the original user message via announce bot.
            if let Ok(msgs) = channel_id
                .messages(&http, serenity::builder::GetMessages::new().limit(10))
                .await
            {
                let mut history_lines = Vec::new();
                let mut last_user_msg = String::new();
                for msg in msgs.iter().rev() {
                    if !msg.content.trim().is_empty() {
                        let content: String = msg.content.chars().take(300).collect();
                        history_lines.push(format!("{}: {}", msg.author.name, content));
                        if !msg.author.bot {
                            last_user_msg = msg.content.clone();
                        }
                    }
                }
                if !last_user_msg.is_empty() {
                    // Store history in kv_meta for router to inject into LLM prompt
                    if !history_lines.is_empty() {
                        let _ = reqwest::Client::new()
                            .post(crate::config::local_api_url(shared.api_port, "/api/kv"))
                            .json(&serde_json::json!({
                                "key": format!("session_retry_context:{}", channel_id),
                                "value": history_lines.join("\n"),
                            }))
                            .send()
                            .await;
                    }
                    // Discord: short notice + original message only
                    let retry_content = format!(
                        "[이전 대화 복원 — 세션이 만료되어 최근 대화를 컨텍스트로 제공합니다]\n\n{}",
                        last_user_msg
                    );
                    let _ = reqwest::Client::new()
                        .post(crate::config::local_api_url(shared.api_port, "/api/send"))
                        .json(&serde_json::json!({
                            "target": format!("channel:{}", channel_id),
                            "content": retry_content,
                            "source": "pipeline",
                            "bot": "announce",
                        }))
                        .send()
                        .await;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!("  [{ts}] ↻ Watcher auto-retry sent for channel {channel_id}");
                }
            }
            // Skip normal response relay
            full_response = String::new();
        }

        // Send the terminal response to Discord
        if !full_response.trim().is_empty() {
            let formatted = format_for_discord(&full_response);
            let prefixed = formatted.to_string();
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 👁 Relaying terminal response to Discord ({} chars, offset {})",
                prefixed.len(),
                data_start_offset
            );
            match placeholder_msg_id {
                Some(msg_id) => {
                    // Update the placeholder with final response (may need splitting)
                    if prefixed.len() <= DISCORD_MSG_LIMIT {
                        rate_limit_wait(&shared, channel_id).await;
                        let _ = channel_id
                            .edit_message(
                                &http,
                                msg_id,
                                serenity::EditMessage::new().content(&prefixed),
                            )
                            .await;
                    } else {
                        // Response too long — delete placeholder and send via send_long_message_raw
                        let _ = channel_id.delete_message(&http, msg_id).await;
                        if let Err(e) =
                            send_long_message_raw(&http, channel_id, &prefixed, &shared).await
                        {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!("  [{ts}] 👁 Failed to relay: {e}");
                        }
                    }
                }
                None => {
                    if let Err(e) =
                        send_long_message_raw(&http, channel_id, &prefixed, &shared).await
                    {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}] 👁 Failed to relay: {e}");
                    }
                }
            }
            // Record the offset range we just relayed to prevent duplicate relay.
            last_relayed_offset = Some(data_start_offset);
        } else if let Some(msg_id) = placeholder_msg_id {
            // No response text but placeholder exists — clean up
            let _ = channel_id.delete_message(&http, msg_id).await;
        }

        // Mark user message as completed: ⏳ → ✅
        // Read user_msg_id from inflight state (turn_bridge stores it there)
        if let Some((provider_kind, _)) =
            parse_provider_and_channel_from_tmux_name(&tmux_session_name)
        {
            if let Some(state) =
                super::inflight::load_inflight_state(&provider_kind, channel_id.get())
            {
                let user_msg_id = serenity::MessageId::new(state.user_msg_id);
                super::formatting::remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;
                super::formatting::add_reaction_raw(&http, channel_id, user_msg_id, '✅').await;
            }
        }

        // Update session tokens from result event and auto-compact if threshold exceeded
        if let Some(tokens) = result_tokens {
            let provider = shared.settings.read().await.provider.clone();
            let session_key =
                super::adk_session::build_adk_session_key(&shared, channel_id, &provider).await;
            let channel_name = {
                let data = shared.core.lock().await;
                data.sessions
                    .get(&channel_id)
                    .and_then(|s| s.channel_name.clone())
            };
            super::adk_session::post_adk_session_status(
                session_key.as_deref(),
                channel_name.as_deref(),
                None,
                "idle",
                &provider,
                None,
                Some(tokens),
                None,
                None,
                shared.api_port,
            )
            .await;

            let ctx_cfg = super::adk_session::fetch_context_thresholds(shared.api_port).await;
            let pct = (tokens * 100) / ctx_cfg.context_window.max(1);
            if pct >= ctx_cfg.compact_pct && !is_prompt_too_long {
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!(
                    "  [{ts}] ⚡ [watcher] Auto-compact: {} at {pct}% ({tokens} tokens)",
                    tmux_session_name
                );
                let name = tmux_session_name.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    crate::services::platform::tmux::send_keys(&name, &["/compact", "Enter"])
                })
                .await;
            }
            // Reset for next turn
            result_tokens = None;
        }
    }

    // Cleanup
    shared.tmux_watchers.remove(&channel_id);

    // Kill dead tmux session to prevent accumulation (especially for thread sessions
    // which are created per-dispatch and would otherwise linger for 24h).
    // #145: skip kill for unified-thread sessions with active auto-queue runs.
    {
        let exact_target = tmux_exact_target(&tmux_session_name);
        let sess = tmux_session_name.clone();
        let _ = tokio::task::spawn_blocking(move || {
            if tmux_session_exists(&sess) && !tmux_session_has_live_pane(&sess) {
                // Check if this is a unified-thread session before killing
                if let Some((_, ch_name)) =
                    crate::services::provider::parse_provider_and_channel_from_tmux_name(&sess)
                {
                    if crate::dispatch::is_unified_thread_channel_name_active(&ch_name) {
                        return;
                    }
                }
                record_tmux_exit_reason(&sess, "watcher cleanup: dead session after turn");
                crate::services::platform::tmux::kill_session(&sess);
            }
        })
        .await;
    }

    // Report idle status to DB so the dashboard doesn't show stale "working" state.
    // Without this, a dead tmux session leaves the DB row as working/dispatched.
    {
        let api_port = shared.api_port;
        let provider = shared.settings.read().await.provider.clone();
        let session_key =
            super::adk_session::build_adk_session_key(&shared, channel_id, &provider).await;
        let channel_name = {
            let data = shared.core.lock().await;
            data.sessions
                .get(&channel_id)
                .and_then(|s| s.channel_name.clone())
        };
        super::adk_session::post_adk_session_status(
            session_key.as_deref(),
            channel_name.as_deref(),
            None, // model
            "idle",
            &provider,
            None, // session_info
            None, // tokens
            None, // cwd
            None, // dispatch_id
            api_port,
        )
        .await;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] 👁 tmux watcher stopped for #{tmux_session_name}");
}

/// Tracks tool/thinking status during watcher output processing.
pub(super) struct WatcherToolState {
    /// Current tool status line (e.g. "⚙ Bash: `ls`")
    pub current_tool_line: Option<String>,
    /// Accumulated thinking text from streaming deltas
    pub thinking_buffer: String,
    /// Whether we are currently inside a thinking block
    pub in_thinking: bool,
    /// Whether any tool_use block has been seen in this turn
    pub any_tool_used: bool,
    /// Whether a text block was streamed after the last tool_use
    pub has_post_tool_text: bool,
}

impl WatcherToolState {
    pub fn new() -> Self {
        Self {
            current_tool_line: None,
            thinking_buffer: String::new(),
            in_thinking: false,
            any_tool_used: false,
            has_post_tool_text: false,
        }
    }
}

/// Process buffered lines for the tmux watcher.
/// Extracts text content, tracks tool status, and detects result events.
/// Returns true if a "result" event was found.
/// Return value: (found_result, is_prompt_too_long, is_auth_error, result_tokens)
pub(super) fn process_watcher_lines(
    buffer: &mut String,
    state: &mut claude::StreamLineState,
    full_response: &mut String,
    tool_state: &mut WatcherToolState,
) -> (bool, bool, bool, Option<u64>) {
    let mut found_result = false;
    let mut is_prompt_too_long = false;
    let mut is_auth_error = false;
    let mut result_tokens: Option<u64> = None;

    while let Some(pos) = buffer.find('\n') {
        let line: String = buffer.drain(..=pos).collect();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse the JSON line
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let event_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match event_type {
                "assistant" => {
                    // Text content from assistant message
                    if let Some(message) = val.get("message") {
                        if let Some(content) = message.get("content") {
                            if let Some(arr) = content.as_array() {
                                for block in arr {
                                    let block_type = block.get("type").and_then(|t| t.as_str());
                                    if block_type == Some("text") {
                                        if let Some(text) =
                                            block.get("text").and_then(|t| t.as_str())
                                        {
                                            full_response.push_str(text);
                                            if tool_state.any_tool_used {
                                                tool_state.has_post_tool_text = true;
                                            }
                                            tool_state.current_tool_line = None;
                                        }
                                    } else if block_type == Some("tool_use") {
                                        tool_state.any_tool_used = true;
                                        tool_state.has_post_tool_text = false;
                                        let name = block
                                            .get("name")
                                            .and_then(|n| n.as_str())
                                            .unwrap_or("Tool");
                                        let input_str = block
                                            .get("input")
                                            .map(|i| i.to_string())
                                            .unwrap_or_default();
                                        let summary = format_tool_input(name, &input_str);
                                        let display = if summary.is_empty() {
                                            format!("⚙ {}", name)
                                        } else {
                                            let truncated: String =
                                                summary.chars().take(120).collect();
                                            format!("⚙ {}: {}", name, truncated)
                                        };
                                        tool_state.current_tool_line = Some(display);
                                    }
                                }
                            }
                        }
                    }
                }
                "content_block_start" => {
                    if let Some(cb) = val.get("content_block") {
                        let cb_type = cb.get("type").and_then(|t| t.as_str());
                        if cb_type == Some("thinking") {
                            tool_state.in_thinking = true;
                            tool_state.thinking_buffer.clear();
                            tool_state.current_tool_line = Some("💭 Thinking...".to_string());
                        } else if cb_type == Some("tool_use") {
                            tool_state.any_tool_used = true;
                            tool_state.has_post_tool_text = false;
                            let name = cb.get("name").and_then(|n| n.as_str()).unwrap_or("Tool");
                            tool_state.current_tool_line = Some(format!("⚙ {}", name));
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = val.get("delta") {
                        if let Some(thinking) = delta.get("thinking").and_then(|t| t.as_str()) {
                            // Accumulate thinking text and update display
                            tool_state.thinking_buffer.push_str(thinking);
                            let display = tool_state.thinking_buffer.trim().to_string();
                            if !display.is_empty() {
                                tool_state.current_tool_line = Some(format!("💭 {display}"));
                            }
                        } else if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            full_response.push_str(text);
                            if tool_state.any_tool_used {
                                tool_state.has_post_tool_text = true;
                            }
                            tool_state.current_tool_line = None;
                        }
                    }
                }
                "content_block_stop" => {
                    if tool_state.in_thinking {
                        // Thinking block completed — show full text
                        tool_state.in_thinking = false;
                        let display = tool_state.thinking_buffer.trim().to_string();
                        if !display.is_empty() {
                            tool_state.current_tool_line = Some(format!("💭 {display}"));
                        }
                    } else if let Some(ref line) = tool_state.current_tool_line {
                        // Tool completed — mark with checkmark
                        if line.starts_with("⚙") {
                            tool_state.current_tool_line = Some(line.replacen("⚙", "✓", 1));
                        }
                    }
                }
                "result" => {
                    let is_error = val
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let result_str = val.get("result").and_then(|r| r.as_str()).unwrap_or("");

                    if is_error {
                        let lower = result_str.to_lowercase();
                        if lower.contains("prompt is too long")
                            || lower.contains("prompt too long")
                            || lower.contains("context_length_exceeded")
                            || lower.contains("conversation too long")
                        {
                            is_prompt_too_long = true;
                        }
                        if lower.contains("not logged in")
                            || lower.contains("authentication error")
                            || lower.contains("unauthorized")
                            || lower.contains("please run /login")
                            || lower.contains("oauth")
                            || lower.contains("token expired")
                            || lower.contains("invalid api key")
                            || lower.contains("api key")
                                && (lower.contains("missing")
                                    || lower.contains("invalid")
                                    || lower.contains("expired"))
                        {
                            is_auth_error = true;
                        }
                    }

                    // Use result text when streaming didn't capture the final response:
                    // 1. full_response is empty — no text was streamed at all
                    // 2. tools were used but no text was streamed after the last tool
                    //    (accumulated text is stale pre-tool narration)
                    if !is_prompt_too_long && !is_auth_error && !result_str.is_empty() {
                        if full_response.is_empty()
                            || (tool_state.any_tool_used && !tool_state.has_post_tool_text)
                        {
                            full_response.clear();
                            full_response.push_str(result_str);
                        }
                    }
                    // Extract token usage from result event for context tracking
                    if let Some(usage) = val.get("usage") {
                        let input = usage
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let cache_read = usage
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let cache_creation = usage
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let output = usage
                            .get("output_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        result_tokens = Some(input + cache_read + cache_creation + output);
                    }

                    state.final_result = Some(String::new());
                    found_result = true;
                }
                _ => {}
            }
        }
    }

    (
        found_result,
        is_prompt_too_long,
        is_auth_error,
        result_tokens,
    )
}

/// On startup, scan for surviving tmux sessions (AgentDesk-*) and restore watchers.
/// This handles the case where AgentDesk was restarted but tmux sessions are still alive.
pub(super) async fn restore_tmux_watchers(http: &Arc<serenity::Http>, shared: &Arc<SharedData>) {
    let provider = shared.settings.read().await.provider.clone();

    // List tmux sessions matching our naming convention
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(crate::services::platform::tmux::list_session_names),
    )
    .await
    {
        Ok(Ok(Ok(names))) => names,
        _ => return, // No tmux, timeout, or no sessions
    };

    let agent_sessions: Vec<&str> = output
        .iter()
        .map(|l| l.trim())
        .filter(|l| {
            parse_provider_and_channel_from_tmux_name(l)
                .map(|(session_provider, _)| session_provider == provider)
                .unwrap_or(false)
        })
        .collect();

    if agent_sessions.is_empty() {
        return;
    }

    // Build channel name → ChannelId map from Discord API (sessions map may be empty after restart)
    let mut name_to_channel: std::collections::HashMap<String, (ChannelId, String)> =
        std::collections::HashMap::new();

    // Try from in-memory sessions first
    {
        let data = shared.core.lock().await;
        for (&ch_id, session) in &data.sessions {
            if let Some(ref ch_name) = session.channel_name {
                let tmux_name = provider.build_tmux_session_name(ch_name);
                name_to_channel.insert(tmux_name, (ch_id, ch_name.clone()));
            }
        }
    }

    // If in-memory sessions don't cover all tmux sessions, fetch from Discord API
    let unresolved: Vec<&&str> = agent_sessions
        .iter()
        .filter(|s| !name_to_channel.contains_key(**s))
        .collect();

    if !unresolved.is_empty() {
        // Fetch guild channels via Discord API
        if let Ok(guilds) = http.get_guilds(None, None).await {
            for guild_info in &guilds {
                if let Ok(channels) = guild_info.id.channels(http).await {
                    for (ch_id, channel) in &channels {
                        let role_binding = resolve_role_binding(*ch_id, Some(&channel.name));
                        if !channel_supports_provider(
                            &provider,
                            Some(&channel.name),
                            false,
                            role_binding.as_ref(),
                        ) {
                            continue;
                        }
                        let tmux_name = provider.build_tmux_session_name(&channel.name);
                        name_to_channel
                            .entry(tmux_name)
                            .or_insert((*ch_id, channel.name.clone()));
                    }
                }
            }
        }

        // Fallback for thread sessions: guild.channels() doesn't return threads.
        // Extract thread_id from the channel name suffix (-t{id}) and use it
        // as the channel_id directly, since Discord thread IDs are channel IDs.
        let still_unresolved: Vec<&&str> = agent_sessions
            .iter()
            .filter(|s| !name_to_channel.contains_key(**s))
            .collect();
        for session_name in &still_unresolved {
            if let Some((_, ch_name)) = parse_provider_and_channel_from_tmux_name(session_name) {
                if let Some(pos) = ch_name.rfind("-t") {
                    let suffix = &ch_name[pos + 2..];
                    if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
                        if let Ok(thread_id) = suffix.parse::<u64>() {
                            let channel_id = ChannelId::new(thread_id);
                            name_to_channel
                                .entry(session_name.to_string())
                                .or_insert((channel_id, ch_name.clone()));
                        }
                    }
                }
            }
        }
    }

    // Collect sessions to restore
    struct PendingWatcher {
        channel_id: ChannelId,
        output_path: String,
        session_name: String,
        initial_offset: u64,
    }

    // Dead sessions that need DB cleanup (idle status report + tmux kill)
    struct DeadSessionCleanup {
        channel_id: ChannelId,
        channel_name: String,
        session_name: String,
    }

    let mut pending: Vec<PendingWatcher> = Vec::new();
    let mut dead_cleanups: Vec<DeadSessionCleanup> = Vec::new();
    let mut owned_sessions: std::collections::HashMap<ChannelId, String> =
        std::collections::HashMap::new();

    for session_name in &agent_sessions {
        let Some((channel_id, channel_name)) = name_to_channel.get(*session_name) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⏭ watcher skip for {} — channel mapping not found",
                session_name
            );
            continue;
        };

        // #148: Do NOT register in owned_sessions yet — QUARANTINE check below may
        // skip this session. Registering early blocks new session creation for the channel.

        if let Some(started) = shared.recovering_channels.get(channel_id) {
            if started.elapsed() < std::time::Duration::from_secs(60) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ⏳ watcher skip for {} — recovery in progress ({:.0}s ago)",
                    session_name,
                    started.elapsed().as_secs_f64()
                );
                continue;
            }
            // Stale recovery — remove marker and proceed with watcher
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ clearing stale recovery marker for {} ({:.0}s elapsed)",
                session_name,
                started.elapsed().as_secs_f64()
            );
            drop(started);
            shared.recovering_channels.remove(channel_id);
        }

        if shared.tmux_watchers.contains_key(channel_id) {
            continue;
        }

        let output_path = crate::services::tmux_common::session_temp_path(session_name, "jsonl");
        if std::fs::metadata(&output_path).is_err() {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⏭ watcher skip for {} — no output file",
                session_name
            );
            continue;
        }

        // Old-gen sessions: adopt instead of killing.
        // The tmux session and Claude CLI process are still alive from the
        // previous dcserver — just update the generation marker and re-attach
        // a watcher. Auto-retry handles stale Claude session IDs if needed.
        let gen_marker_path =
            crate::services::tmux_common::session_temp_path(session_name, "generation");
        let session_gen = std::fs::read_to_string(&gen_marker_path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let current_gen = super::runtime_store::load_generation();
        if session_gen < current_gen && current_gen > 0 {
            // Skip sessions belonging to other runtimes
            let current_owner_marker = current_tmux_owner_marker();
            if !session_belongs_to_current_runtime(session_name, &current_owner_marker) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ⏭ watcher skip for {} — owned by other runtime",
                    session_name
                );
                continue;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ↻ Adopting old-gen session {} (gen {} → {})",
                session_name, session_gen, current_gen
            );
            // Update generation marker to current gen
            let _ = std::fs::write(&gen_marker_path, current_gen.to_string());
        }

        if !tmux_session_has_live_pane(session_name) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            if let Some(diag) = build_tmux_death_diagnostic(session_name, Some(&output_path)) {
                println!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux pane dead ({diag})",
                    session_name
                );
            } else {
                println!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux pane dead",
                    session_name
                );
            }
            // Schedule DB cleanup + tmux kill for this dead session
            dead_cleanups.push(DeadSessionCleanup {
                channel_id: *channel_id,
                channel_name: channel_name.clone(),
                session_name: session_name.to_string(),
            });
            continue;
        }

        // #148: Only register in owned_sessions after passing QUARANTINE + live-pane checks.
        // Earlier registration blocked new session creation for quarantined/dead channels.
        owned_sessions
            .entry(*channel_id)
            .or_insert_with(|| channel_name.clone());

        let initial_offset = std::fs::metadata(&output_path)
            .map(|m| m.len())
            .unwrap_or(0);

        pending.push(PendingWatcher {
            channel_id: *channel_id,
            output_path,
            session_name: session_name.to_string(),
            initial_offset,
        });
    }

    // Register sessions in CoreState so cleanup_orphan_tmux_sessions recognizes them
    // and message handlers find an active session with current_path
    if !owned_sessions.is_empty() {
        let settings = shared.settings.read().await;
        let mut data = shared.core.lock().await;
        for (channel_id, channel_name) in &owned_sessions {
            let channel_key = channel_id.get().to_string();
            let last_path = settings.last_sessions.get(&channel_key).cloned();
            let remote_profile = settings.last_remotes.get(&channel_key).cloned();

            let session =
                data.sessions
                    .entry(*channel_id)
                    .or_insert_with(|| super::DiscordSession {
                        session_id: None,
                        current_path: None,
                        history: Vec::new(),
                        pending_uploads: Vec::new(),
                        cleared: false,
                        channel_name: Some(channel_name.clone()),
                        category_name: None,
                        remote_profile_name: remote_profile,
                        channel_id: Some(channel_id.get()),

                        last_active: tokio::time::Instant::now(),
                        worktree: None,

                        born_generation: super::runtime_store::load_generation(),
                    });

            // Restore current_path: DB cwd (worktree-aware) > last_sessions (yaml, main workspace)
            if session.current_path.is_none() {
                // Try DB cwd first — preserves worktree paths from previous session
                let tmux_name = provider.build_tmux_session_name(channel_name);
                let hostname = crate::services::platform::hostname_short();
                let session_key = format!("{}:{}", hostname, tmux_name);
                let db_cwd: Option<String> = shared.db.as_ref().and_then(|db| {
                    db.lock().ok().and_then(|conn| {
                        conn.query_row(
                            "SELECT cwd FROM sessions WHERE session_key = ?1",
                            [&session_key],
                            |row| row.get::<_, String>(0),
                        )
                        .ok()
                        .filter(|p| !p.is_empty() && std::path::Path::new(p).is_dir())
                    })
                });
                let effective_path = db_cwd.or(last_path);
                if let Some(path) = effective_path {
                    session.current_path = Some(path);
                }
            }
        }
    }

    // Spawn watchers
    for pw in pending {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] ↻ Restoring tmux watcher for {} (offset {})",
            pw.session_name, pw.initial_offset
        );

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
        let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let turn_delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));

        shared.tmux_watchers.insert(
            pw.channel_id,
            TmuxWatcherHandle {
                paused: paused.clone(),
                resume_offset: resume_offset.clone(),
                cancel: cancel.clone(),
                pause_epoch: pause_epoch.clone(),
                turn_delivered: turn_delivered.clone(),
            },
        );

        tokio::spawn(tmux_output_watcher(
            pw.channel_id,
            http.clone(),
            shared.clone(),
            pw.output_path,
            pw.session_name,
            pw.initial_offset,
            cancel,
            paused,
            resume_offset,
            pause_epoch,
            turn_delivered,
        ));
    }

    // Clean up dead sessions: report idle to DB and kill tmux sessions
    if !dead_cleanups.is_empty() {
        let api_port = shared.api_port;
        let provider = shared.settings.read().await.provider.clone();

        for dc in &dead_cleanups {
            let tmux_name = provider.build_tmux_session_name(&dc.channel_name);
            let hostname = crate::services::platform::hostname_short();
            let session_key = format!("{}:{}", hostname, tmux_name);

            super::adk_session::post_adk_session_status(
                Some(&session_key),
                Some(&dc.channel_name),
                None,
                "idle",
                &provider,
                None,
                None,
                None,
                None,
                api_port,
            )
            .await;

            // Kill the dead tmux session
            let sess = dc.session_name.clone();
            let _ = tokio::task::spawn_blocking(move || {
                record_tmux_exit_reason(&sess, "startup cleanup: dead session");
                crate::services::platform::tmux::kill_session(&sess);
            })
            .await;
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] 🧹 Cleaned {} dead tmux session(s) on startup",
            dead_cleanups.len()
        );
    }
}

/// Kill orphan tmux sessions (AgentDesk-*) that don't map to any known channel.
/// Called after restore_tmux_watchers to clean up sessions from renamed/deleted channels.
pub(super) async fn cleanup_orphan_tmux_sessions(shared: &Arc<SharedData>) {
    let provider = shared.settings.read().await.provider.clone();
    let current_owner_marker = current_tmux_owner_marker();

    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(crate::services::platform::tmux::list_session_names),
    )
    .await
    {
        Ok(Ok(Ok(names))) => names,
        _ => return,
    };

    let orphans: Vec<String> = {
        let data = shared.core.lock().await;
        let mut result = Vec::new();

        for session_name in output.iter().map(|s| s.trim()) {
            let Some((session_provider, _)) =
                parse_provider_and_channel_from_tmux_name(session_name)
            else {
                continue;
            };
            if session_provider != provider {
                continue;
            }
            if !session_belongs_to_current_runtime(session_name, &current_owner_marker) {
                continue;
            }

            // Check if any active channel maps to this session
            let has_owner = data.sessions.iter().any(|(_, session)| {
                session
                    .channel_name
                    .as_ref()
                    .map(|ch_name| provider.build_tmux_session_name(ch_name) == session_name)
                    .unwrap_or(false)
            });

            if !has_owner {
                // #145: skip orphan cleanup for unified-thread sessions with active runs
                if let Some((_, ref ch_name)) =
                    parse_provider_and_channel_from_tmux_name(session_name)
                        .as_ref()
                        .map(|(p, c)| (p.clone(), c.clone()))
                {
                    if crate::dispatch::is_unified_thread_channel_name_active(ch_name) {
                        continue;
                    }
                }

                // #181: Don't kill sessions with live processes in their pane.
                // During restart, dispatch threads may not yet be registered in
                // data.sessions (recover_orphan_pending_dispatches runs AFTER this).
                // A tmux pane with a running process is proof the session is in use.
                if tmux_session_has_live_pane(session_name) {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}]   skipped orphan (live pane): {}", session_name);
                    continue;
                }

                result.push(session_name.to_string());
            }
        }

        result
    };

    if orphans.is_empty() {
        return;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] 🧹 Cleaning {} orphan tmux session(s)...",
        orphans.len()
    );

    for name in &orphans {
        let exact_target = tmux_exact_target(name);
        let name_clone = name.clone();
        let killed = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                record_tmux_exit_reason(&name_clone, "orphan cleanup: no owning channel session");
                crate::services::platform::tmux::kill_session(&name_clone)
            }),
        )
        .await
        .unwrap_or(Ok(false))
        .unwrap_or(false);

        if killed {
            println!("  [{ts}]   killed orphan: {}", name);
            // Also clean associated temp files
            let _ = std::fs::remove_file(crate::services::tmux_common::session_temp_path(
                name, "jsonl",
            ));
            let _ = std::fs::remove_file(crate::services::tmux_common::session_temp_path(
                name, "input",
            ));
            let _ = std::fs::remove_file(crate::services::tmux_common::session_temp_path(
                name, "prompt",
            ));
            let _ = std::fs::remove_file(tmux_owner_path(name));
        }
    }
}

/// Periodically reap dead tmux sessions (pane_dead=1) that still have DB rows
/// showing working/dispatched status. This catches cases where the watcher
/// missed cleanup (e.g. crashed, or session died between watcher polls).
pub(super) async fn reap_dead_tmux_sessions(shared: &Arc<SharedData>) {
    let provider = shared.settings.read().await.provider.clone();
    let current_owner_marker = current_tmux_owner_marker();
    let api_port = shared.api_port;

    // List all tmux sessions
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(|| crate::services::platform::tmux::list_session_names()),
    )
    .await
    {
        Ok(Ok(Ok(names))) => names,
        _ => return,
    };

    let mut reaped = 0u32;

    for session_name in output.iter().map(|s| s.trim()) {
        let Some((session_provider, _)) = parse_provider_and_channel_from_tmux_name(session_name)
        else {
            continue;
        };
        if session_provider != provider {
            continue;
        }
        if !session_belongs_to_current_runtime(session_name, &current_owner_marker) {
            continue;
        }

        // Skip sessions that have a live pane (actually working)
        if tmux_session_has_live_pane(session_name) {
            continue;
        }

        // Skip sessions that already have an active watcher (watcher handles its own cleanup)
        let channel_id_for_session = {
            let data = shared.core.lock().await;
            data.sessions
                .iter()
                .find(|(_, s)| {
                    s.channel_name
                        .as_ref()
                        .map(|ch| provider.build_tmux_session_name(ch) == session_name)
                        .unwrap_or(false)
                })
                .map(|(ch, s)| (*ch, s.channel_name.clone()))
        };

        let Some((channel_id, channel_name)) = channel_id_for_session else {
            continue; // orphan — handled by cleanup_orphan_tmux_sessions
        };

        // If a watcher is attached, let it handle the cleanup
        if shared.tmux_watchers.contains_key(&channel_id) {
            continue;
        }

        // Dead session with no watcher — report idle to DB and kill
        let tmux_name =
            provider.build_tmux_session_name(channel_name.as_deref().unwrap_or("unknown"));
        let hostname = crate::services::platform::hostname_short();
        let session_key = format!("{}:{}", hostname, tmux_name);

        // Check if this is a thread session (channel name contains -t{15+digit})
        let is_thread = channel_name
            .as_deref()
            .and_then(|n| {
                let pos = n.rfind("-t")?;
                let suffix = &n[pos + 2..];
                (suffix.len() >= 15 && suffix.chars().all(|c| c.is_ascii_digit())).then_some(())
            })
            .is_some();

        // #145: unified-thread sessions should NOT be killed or deleted while
        // the auto-queue run is still active — mark idle instead and skip kill.
        let is_unified_active =
            is_thread && crate::dispatch::is_unified_thread_channel_active(channel_id.get());

        if is_thread && !is_unified_active {
            // Thread sessions: delete from DB entirely (they are one-shot)
            super::adk_session::delete_adk_session(&session_key, api_port).await;
        } else {
            // Fixed-channel sessions or active unified-thread: just mark idle
            super::adk_session::post_adk_session_status(
                Some(&session_key),
                channel_name.as_deref(),
                None,
                "idle",
                &provider,
                None,
                None,
                None,
                None,
                api_port,
            )
            .await;
        }

        if is_unified_active {
            // Don't kill unified-thread sessions — they'll be reused
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ♻ reaper: skipping kill for unified-thread session {session_name} — run still active"
            );
            continue;
        }

        // Kill the dead tmux session
        let exact_target = tmux_exact_target(session_name);
        let sess = session_name.to_string();
        let kill_result = tokio::task::spawn_blocking(move || {
            record_tmux_exit_reason(&sess, "reaper: dead session with no watcher");
            crate::services::platform::tmux::kill_session_output(&sess)
        })
        .await;
        match &kill_result {
            Ok(Ok(o)) if !o.status.success() => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!(
                    "  [{ts}] ⚠ reaper: tmux kill-session failed for {session_name}: {}",
                    String::from_utf8_lossy(&o.stderr)
                );
            }
            Ok(Err(e)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!("  [{ts}] ⚠ reaper: tmux kill-session error for {session_name}: {e}");
            }
            _ => {}
        }

        reaped += 1;
    }

    if reaped > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] 🪦 Reaped {reaped} dead tmux session(s)");
    }

    // #145: Process kill_unified_thread signals from auto-queue.js
    // When a unified-thread run completes, the JS policy writes a kv_meta flag
    // for us to pick up and kill the shared tmux session.
    process_unified_thread_kill_signals(shared).await;
}

/// Kill tmux sessions flagged for cleanup by auto-queue.js after unified run completion.
async fn process_unified_thread_kill_signals(shared: &Arc<SharedData>) {
    let channels = tokio::task::spawn_blocking(crate::dispatch::drain_unified_thread_kill_signals)
        .await
        .unwrap_or_default();

    for thread_channel_id in channels {
        // The kill signal carries the raw thread channel ID. Thread tmux sessions
        // are named "{parent_channel_name}-t{thread_channel_id}{env_suffix}" (see mod.rs:2601).
        // We must find the matching tmux session by scanning for the exact suffix
        // including env isolation to avoid killing sessions from other environments.
        let env_suffix = crate::services::provider::tmux_env_suffix();
        let full_suffix = format!("-t{thread_channel_id}{env_suffix}");
        let provider = shared.settings.read().await.provider.clone();
        let suffix_c = full_suffix.clone();
        let provider_c = provider.clone();
        let killed = tokio::task::spawn_blocking(move || {
            let prefix = format!("{}-", crate::services::provider::TMUX_SESSION_PREFIX);
            let names = crate::services::platform::tmux::list_session_names().ok()?;
            for name in &names {
                if name.starts_with(&prefix) && name.ends_with(&suffix_c) {
                    record_tmux_exit_reason(name, "unified-thread run completed");
                    crate::services::platform::tmux::kill_session(name);
                    return Some(name.clone());
                }
            }
            None
        })
        .await
        .unwrap_or(None);

        let ts = chrono::Local::now().format("%H:%M:%S");
        if let Some(name) = killed {
            println!(
                "  [{ts}] ♻ Killed unified-thread tmux session: {name} (thread: {thread_channel_id})"
            );
        }
    }
}
