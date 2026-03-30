use super::handoff::{HandoffRecord, save_handoff};
use super::turn_bridge::stale_inflight_message;
use super::*;
use crate::services::tmux_common::tmux_exact_target;
#[cfg(unix)]
use crate::services::tmux_diagnostics::{build_tmux_death_diagnostic, tmux_session_has_live_pane};
use crate::utils::format::tail_with_ellipsis;

#[cfg(not(unix))]
fn tmux_session_has_live_pane(_name: &str) -> bool {
    false
}

/// Retry-aware tmux session check for recovery after dcserver restart.
/// The first check can false-negative if tmux CLI hasn't fully initialized yet.
fn tmux_session_alive_with_retry(name: &str) -> bool {
    if tmux_session_has_live_pane(name) {
        return true;
    }
    // Retry up to 2 more times with 1-second gaps
    for attempt in 1..=2 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if tmux_session_has_live_pane(name) {
            println!(
                "  [recovery] tmux pane alive on retry {} for {}",
                attempt, name
            );
            return true;
        }
    }
    false
}

pub(super) const RESTART_SESSION_DIED_HANDOFF_SENTINEL: &str = "__restart_session_died_handoff__";

/// Retry-aware tmux has_session check.
fn tmux_has_session_with_retry(name: &str) -> bool {
    if crate::services::platform::tmux::has_session(name) {
        return true;
    }
    for attempt in 1..=2 {
        std::thread::sleep(std::time::Duration::from_secs(1));
        if crate::services::platform::tmux::has_session(name) {
            println!(
                "  [recovery] tmux session found on retry {} for {}",
                attempt, name
            );
            return true;
        }
    }
    false
}

#[cfg(not(unix))]
fn build_tmux_death_diagnostic(_name: &str, _output_path: Option<&str>) -> Option<String> {
    None
}

fn save_missing_session_handoff(
    provider: &ProviderKind,
    state: &inflight::InflightTurnState,
    best_response: &str,
) {
    let partial = best_response.trim();
    let partial_context = if partial.is_empty() {
        "(재시작 전까지 전달된 partial 응답 없음)".to_string()
    } else {
        tail_with_ellipsis(partial, 1200)
    };
    let context = format!(
        "재시작 중 기존 tmux 세션이 사라져 동일 turn에 재연결하지 못했습니다.\n\n원래 사용자 요청:\n{}\n\n재시작 전 partial 응답:\n{}",
        state.user_text.trim(),
        partial_context,
    );
    let handoff = HandoffRecord::new(
        provider,
        state.channel_id,
        state.channel_name.clone(),
        "재시작 중 중단된 응답을 이어서 마무리",
        context,
        None,
        Some(state.user_msg_id),
    );
    if let Err(e) = save_handoff(&handoff) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        eprintln!(
            "  [{ts}] ⚠ failed to save recovery handoff for channel {}: {e}",
            state.channel_id
        );
    }
}

/// Check whether a **successful** result record exists after the given offset.
/// Error results are not considered completion — they should not trigger the
/// recovery completed-turn path (✅ reaction, idle dispatch, etc.).
fn output_has_result_after_offset(output_path: &str, start_offset: u64) -> bool {
    let Ok(bytes) = std::fs::read(output_path) else {
        return false;
    };
    let start = usize::try_from(start_offset)
        .ok()
        .map(|offset| offset.min(bytes.len()))
        .unwrap_or(bytes.len());

    String::from_utf8_lossy(&bytes[start..])
        .lines()
        .any(|line| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return false;
            }
            let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
                return false;
            };
            let is_result = value.get("type").and_then(|v| v.as_str()) == Some("result");
            let is_error = value
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            is_result && !is_error
        })
}

/// Extract accumulated assistant text from output JSONL after the given offset.
fn extract_response_from_output(output_path: &str, start_offset: u64) -> String {
    extract_response_from_output_pub(output_path, start_offset)
}

/// Public wrapper for turn_bridge fallback recovery.
///
/// Mirrors the `resolve_done_response` logic from `turn_bridge.rs`:
/// when tool_use was seen and no post-tool assistant text followed,
/// prefer the `result` record over stale pre-tool narration.
pub(super) fn extract_response_from_output_pub(output_path: &str, start_offset: u64) -> String {
    let Ok(bytes) = std::fs::read(output_path) else {
        return String::new();
    };
    let start = usize::try_from(start_offset)
        .ok()
        .map(|offset| offset.min(bytes.len()))
        .unwrap_or(bytes.len());

    let mut response = String::new();
    let mut any_tool_used = false;
    let mut has_post_tool_text = false;
    let mut result_text = String::new();

    for line in String::from_utf8_lossy(&bytes[start..]).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let msg_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match msg_type {
            "assistant" => {
                if let Some(content) = value.get("message").and_then(|m| m.get("content")) {
                    if let Some(arr) = content.as_array() {
                        let mut block_has_tool = false;
                        let mut block_has_text = false;
                        for block in arr {
                            match block.get("type").and_then(|t| t.as_str()) {
                                Some("text") => {
                                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                        if !text.is_empty() {
                                            response.push_str(text);
                                            block_has_text = true;
                                        }
                                    }
                                }
                                Some("tool_use") => {
                                    block_has_tool = true;
                                }
                                _ => {}
                            }
                        }
                        if block_has_tool {
                            any_tool_used = true;
                            // Reset: text in a block that also has tool_use is pre-tool narration
                            has_post_tool_text = false;
                        } else if block_has_text && any_tool_used {
                            has_post_tool_text = true;
                        }
                    }
                }
            }
            "result" => {
                let subtype = value.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
                if subtype == "success" {
                    if let Some(r) = value.get("result").and_then(|v| v.as_str()) {
                        result_text = r.to_string();
                    }
                }
            }
            _ => {}
        }
    }

    // Apply resolve_done_response logic: if tool was used and no post-tool
    // assistant text followed, the accumulated response is stale narration —
    // prefer the authoritative result record.
    if !result_text.is_empty() {
        if response.trim().is_empty() {
            return result_text;
        }
        if any_tool_used && !has_post_tool_text {
            return result_text;
        }
    }
    response
}

fn output_has_bytes_after_offset(output_path: &str, start_offset: u64) -> bool {
    std::fs::metadata(output_path)
        .map(|meta| meta.len() > start_offset)
        .unwrap_or(false)
}

fn recovery_watcher_start_offset(output_path: &str, saved_last_offset: u64) -> (u64, u64, bool) {
    let current_len = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);
    if current_len >= saved_last_offset {
        (saved_last_offset, current_len, false)
    } else {
        // The output file was recreated or truncated while dcserver was down.
        // Resume from the beginning of the new file so we do not skip the
        // entire restarted session output.
        (0, current_len, true)
    }
}

pub(super) async fn restore_inflight_turns(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let states = load_inflight_states(provider);
    if states.is_empty() {
        return;
    }

    let settings_snapshot = shared.settings.read().await.clone();

    let current_gen = shared.current_generation;

    for state in states {
        // No generation gate — adopt mode allows old-gen session recovery.
        // If a restart report exists for this channel, check whether the agent
        // has already finished before deciding to skip recovery.  When the output
        // file contains a completed result we deliver it directly and clear both
        // the inflight state and the restart report, so the flush loop won't
        // overwrite the message with a generic follow-up.
        if super::restart_report::load_restart_report(provider, state.channel_id).is_some() {
            let output_path_for_check: Option<String> = state
                .output_path
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    state
                        .channel_name
                        .as_ref()
                        .map(|name| tmux_runtime_paths(&provider.build_tmux_session_name(name)).0)
                });
            let completed_during_downtime = output_path_for_check
                .as_deref()
                .map(|path| output_has_result_after_offset(path, state.last_offset))
                .unwrap_or(false);

            if completed_during_downtime {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ✓ recovering completed turn for channel {} (restart report exists but output has result)",
                    state.channel_id
                );
                let extracted = output_path_for_check
                    .as_deref()
                    .map(|p| extract_response_from_output(p, state.last_offset))
                    .unwrap_or_default();
                let final_text = if extracted.trim().is_empty() {
                    if state.full_response.trim().is_empty() {
                        "(복구됨 — 응답 텍스트 없음)".to_string()
                    } else {
                        super::formatting::format_for_discord(&state.full_response)
                    }
                } else {
                    super::formatting::format_for_discord(&extracted)
                };
                let channel_id = ChannelId::new(state.channel_id);
                let current_msg_id = MessageId::new(state.current_msg_id);
                let _ = super::formatting::replace_long_message_raw(
                    http,
                    channel_id,
                    current_msg_id,
                    &final_text,
                    shared,
                )
                .await;
                // Mark user message as completed: ⏳ → ✅
                let user_msg_id = MessageId::new(state.user_msg_id);
                super::formatting::remove_reaction_raw(http, channel_id, user_msg_id, '⏳').await;
                super::formatting::add_reaction_raw(http, channel_id, user_msg_id, '✅').await;
                // Complete the dispatch if this was a dispatch turn — the normal
                // completion path was lost when dcserver restarted.
                // #142: Check dispatch type — implementation/rework need explicit completion,
                // review can use idle auto-complete.
                let recovered_dispatch_id = parse_dispatch_id(&state.user_text);
                let mut dispatch_completed = recovered_dispatch_id.is_none();
                if let Some(ref did) = recovered_dispatch_id {
                    // #143: Use finalize_dispatch directly with retry.
                    if let (Some(db), Some(engine)) = (&shared.db, &shared.engine) {
                        for attempt in 1..=3u8 {
                            match crate::dispatch::finalize_dispatch(
                                db,
                                engine,
                                did,
                                "recovery_completed_during_downtime",
                                None,
                            ) {
                                Ok(_) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!(
                                        "  [{ts}] ✓ recovery: completed dispatch {did} via finalize_dispatch"
                                    );
                                    crate::server::routes::dispatches::queue_dispatch_followup(
                                        db, &did,
                                    );
                                    dispatch_completed = true;
                                    break;
                                }
                                Err(e) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    eprintln!(
                                        "  [{ts}] ⚠ recovery: finalize_dispatch failed for {did} (attempt {attempt}/3): {e}"
                                    );
                                    if attempt < 3 {
                                        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                                    }
                                }
                            }
                        }
                        // All retries exhausted — DB fallback via pool, then runtime-root
                        if !dispatch_completed {
                            let pool_ok = db.separate_conn().ok().map_or(false, |conn| {
                                let changed = conn.execute(
                                    "UPDATE task_dispatches SET status = 'completed', \
                                     result = '{\"completion_source\":\"recovery_db_fallback\",\"needs_reconcile\":true}', \
                                     updated_at = datetime('now') WHERE id = ?1 AND status IN ('pending', 'dispatched')",
                                    [did.as_str()],
                                ).unwrap_or(0);
                                if changed > 0 {
                                    conn.execute(
                                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                                        rusqlite::params![format!("reconcile_dispatch:{did}"), did.as_str()],
                                    ).ok();
                                }
                                changed > 0
                            });
                            if pool_ok {
                                dispatch_completed = true;
                            } else {
                                dispatch_completed =
                                    super::turn_bridge::runtime_db_fallback_complete(
                                        did,
                                        "recovery_db_fallback",
                                    );
                            }
                        }
                    } else {
                        // Db/Engine not available — fall back to API PATCH with retry
                        let complete_url = crate::config::local_api_url(
                            shared.api_port,
                            &format!("/api/dispatches/{}", did),
                        );
                        let payload = serde_json::json!({
                            "status": "completed",
                            "result": {"completion_source": "recovery_completed_during_downtime"},
                        });
                        for attempt in 1..=3u8 {
                            match reqwest::Client::new()
                                .patch(&complete_url)
                                .json(&payload)
                                .send()
                                .await
                            {
                                Ok(resp) if resp.status().is_success() => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!(
                                        "  [{ts}] ✓ recovery: completed dispatch {did} via API"
                                    );
                                    dispatch_completed = true;
                                    break;
                                }
                                Ok(resp) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    eprintln!(
                                        "  [{ts}] ⚠ recovery: dispatch {did} API completion failed (attempt {attempt}/3): {}",
                                        resp.status()
                                    );
                                }
                                Err(e) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    eprintln!(
                                        "  [{ts}] ⚠ recovery: dispatch {did} API call failed (attempt {attempt}/3): {e}"
                                    );
                                }
                            }
                            if attempt < 3 {
                                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                            }
                        }
                        // API retries exhausted — runtime-root DB fallback
                        if !dispatch_completed {
                            dispatch_completed = super::turn_bridge::runtime_db_fallback_complete(
                                did,
                                "recovery_db_fallback",
                            );
                        }
                    }
                }
                // Only clear recovery bookkeeping if dispatch was completed (or no dispatch).
                // Preserving state on failure allows the next recovery pass to retry.
                if dispatch_completed {
                    super::restart_report::clear_restart_report(provider, state.channel_id);
                    clear_inflight_state(provider, state.channel_id);
                } else if let Some(ref did) = recovered_dispatch_id {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    eprintln!(
                        "  [{ts}] ❌ recovery: dispatch {did} completion failed — preserving state for next recovery pass"
                    );
                }
                continue;
            }

            // Agent may still be running.  If the tmux session is alive, clear
            // the restart report and fall through to normal recovery (which
            // re-attaches a watcher to pick up the remaining output).
            // If the session is dead, delegate to the flush loop for fallback.
            let tmux_name = state
                .tmux_session_name
                .as_deref()
                .or_else(|| state.channel_name.as_deref())
                .map(|name| {
                    if name.starts_with(&format!(
                        "{}-",
                        crate::services::provider::TMUX_SESSION_PREFIX
                    )) {
                        name.to_string()
                    } else {
                        provider.build_tmux_session_name(name)
                    }
                });
            let session_alive = tmux_name
                .as_deref()
                .map_or(false, tmux_session_alive_with_retry);

            if session_alive {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ↻ restart report exists but tmux session alive for channel {}: clearing report, spawning watcher immediately",
                    state.channel_id
                );
                super::restart_report::clear_restart_report(provider, state.channel_id);
                // Register session in-memory so handlers can find it.
                // Derive channel_name from tmux session name if not in inflight state.
                let effective_channel_name = state.channel_name.clone().or_else(|| {
                    tmux_name.as_deref().and_then(|name| {
                        crate::services::provider::parse_provider_and_channel_from_tmux_name(name)
                            .map(|(_, ch)| ch)
                    })
                });
                let channel_id = ChannelId::new(state.channel_id);
                {
                    let mut data = shared.core.lock().await;
                    let session =
                        data.sessions
                            .entry(channel_id)
                            .or_insert_with(|| DiscordSession {
                                session_id: state.session_id.clone(),
                                current_path: None,
                                history: Vec::new(),
                                pending_uploads: Vec::new(),
                                cleared: false,
                                remote_profile_name: None,
                                channel_id: Some(state.channel_id),
                                channel_name: effective_channel_name.clone(),
                                category_name: None,
                                last_active: tokio::time::Instant::now(),
                                worktree: None,
                                born_generation: super::runtime_store::load_generation(),
                            });
                    session.channel_id = Some(state.channel_id);
                    session.last_active = tokio::time::Instant::now();
                    if session.channel_name.is_none() {
                        session.channel_name = effective_channel_name;
                    }
                }

                // Immediately spawn a tmux watcher instead of deferring to
                // restore_tmux_watchers().  The previous "watcher will adopt"
                // approach had a race condition: the tmux session could die
                // between recovery (now) and restore_tmux_watchers (~50s later),
                // losing the in-progress response entirely.
                if let Some(ref tmux_session_name) = tmux_name {
                    let output_path =
                        crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
                    if std::fs::metadata(&output_path).is_ok()
                        && !shared.tmux_watchers.contains_key(&channel_id)
                    {
                        let (initial_offset, current_len, truncated) =
                            recovery_watcher_start_offset(&output_path, state.last_offset);
                        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let resume_offset = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
                        let pause_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                        let turn_delivered =
                            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        shared.tmux_watchers.insert(
                            channel_id,
                            TmuxWatcherHandle {
                                paused: paused.clone(),
                                resume_offset: resume_offset.clone(),
                                cancel: cancel.clone(),
                                pause_epoch: pause_epoch.clone(),
                                turn_delivered: turn_delivered.clone(),
                            },
                        );
                        let ts2 = chrono::Local::now().format("%H:%M:%S");
                        if truncated {
                            println!(
                                "  [{ts2}] ↻ recovery: output truncated for #{} (saved offset {}, file len {}), restarting watcher from 0",
                                tmux_session_name, state.last_offset, current_len
                            );
                        }
                        println!(
                            "  [{ts2}] 👁 recovery: spawned watcher for #{} at offset {}",
                            tmux_session_name, initial_offset
                        );
                        tokio::spawn(super::tmux::tmux_output_watcher(
                            channel_id,
                            http.clone(),
                            shared.clone(),
                            output_path,
                            tmux_session_name.clone(),
                            initial_offset,
                            cancel,
                            paused,
                            resume_offset,
                            pause_epoch,
                            turn_delivered,
                        ));
                    }
                }

                // Keep the inflight state until the watcher either relays the
                // final response or triggers watcher-death handoff. Clearing it
                // here breaks the handoff path if the recovered tmux session
                // dies before producing a result.
                continue;
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                if let Some(diag) = tmux_name.as_deref().and_then(|name| {
                    build_tmux_death_diagnostic(name, output_path_for_check.as_deref())
                }) {
                    println!(
                        "  [{ts}] ↻ restart report exists but tmux session is dead for channel {}: clearing report, continuing with direct fallback recovery ({diag})",
                        state.channel_id
                    );
                } else {
                    println!(
                        "  [{ts}] ↻ restart report exists but tmux session is dead for channel {}: clearing report, continuing with direct fallback recovery",
                        state.channel_id
                    );
                }
                super::restart_report::clear_restart_report(provider, state.channel_id);
            }
        }

        let channel_id = ChannelId::new(state.channel_id);
        let current_msg_id = MessageId::new(state.current_msg_id);
        let user_msg_id = MessageId::new(state.user_msg_id);
        let channel_name = state.channel_name.clone();
        let tmux_session_name = state.tmux_session_name.clone().or_else(|| {
            channel_name
                .as_ref()
                .map(|name| provider.build_tmux_session_name(name))
        });
        let (fallback_output, fallback_input) = tmux_session_name
            .as_deref()
            .map(tmux_runtime_paths)
            .unwrap_or_else(|| (String::new(), String::new()));
        let output_path = state
            .output_path
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                if !fallback_output.is_empty() {
                    Some(fallback_output.clone())
                } else {
                    None
                }
            });
        let input_fifo_path = state
            .input_fifo_path
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                if !fallback_input.is_empty() {
                    Some(fallback_input.clone())
                } else {
                    None
                }
            });
        // Check exit reason file for post-mortem diagnostics
        if let Some(ref op) = output_path {
            let exit_reason_path = format!("{}.exit_reason", op);
            if let Ok(reason) = std::fs::read_to_string(&exit_reason_path) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] 🔍 exit_reason for channel {}: {}",
                    state.channel_id,
                    reason.trim()
                );
                // Clean up exit reason file after reading
                let _ = std::fs::remove_file(&exit_reason_path);
            }
        }

        let output_already_completed = output_path
            .as_deref()
            .map(|path| output_has_result_after_offset(path, state.last_offset))
            .unwrap_or(false);
        let output_has_new_bytes = output_path
            .as_deref()
            .map(|path| output_has_bytes_after_offset(path, state.last_offset))
            .unwrap_or(false);

        if output_already_completed {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ✓ recovering completed turn for channel {}: output contains result after offset {}",
                state.channel_id, state.last_offset
            );
            // Deliver the result to Discord before clearing the inflight state
            let extracted = output_path
                .as_deref()
                .map(|p| extract_response_from_output(p, state.last_offset))
                .unwrap_or_default();
            let final_text = if extracted.trim().is_empty() {
                if state.full_response.trim().is_empty() {
                    "(복구됨 — 응답 텍스트 없음)".to_string()
                } else {
                    super::formatting::format_for_discord(&state.full_response)
                }
            } else {
                super::formatting::format_for_discord(&extracted)
            };
            let _ = super::formatting::replace_long_message_raw(
                http,
                channel_id,
                current_msg_id,
                &final_text,
                shared,
            )
            .await;
            // Keep the inflight state until the watcher either relays the
            // final response or triggers watcher-death handoff. Clearing it
            // here breaks the handoff path if the recovered tmux session dies
            // before producing a result.
            continue;
        }

        let tmux_ready_without_new_output = tmux_session_name.as_deref().map_or(false, |name| {
            !output_has_new_bytes && claude::tmux_session_ready_for_input(name)
        });

        if tmux_ready_without_new_output {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ✓ clearing inflight turn for channel {}: tmux is ready for input and output is idle after offset {}",
                state.channel_id, state.last_offset
            );
            let final_text = if state.full_response.trim().is_empty() {
                stale_inflight_message("")
            } else {
                super::formatting::format_for_discord(&state.full_response)
            };
            let _ = super::formatting::replace_long_message_raw(
                http,
                channel_id,
                current_msg_id,
                &final_text,
                shared,
            )
            .await;
            clear_inflight_state(provider, state.channel_id);
            continue;
        }

        let can_recover = tmux_session_name
            .as_deref()
            .map_or(false, |name| tmux_has_session_with_retry(name));

        if !can_recover {
            let ts = chrono::Local::now().format("%H:%M:%S");
            // Even without a live tmux session, the output file may contain
            // response data. Try extracting from the full file first, then
            // fall back to saved partial response.
            let extracted_full = output_path
                .as_deref()
                .map(|p| extract_response_from_output(p, 0))
                .unwrap_or_default();
            let best_response = if !extracted_full.trim().is_empty() {
                extracted_full
            } else {
                state.full_response.clone()
            };
            let stale_text = stale_inflight_message(&best_response);
            if let Some(diag) = tmux_session_name
                .as_deref()
                .and_then(|name| build_tmux_death_diagnostic(name, output_path.as_deref()))
            {
                println!(
                    "  [{ts}] ⚠ cannot recover inflight turn for channel {}: tmux session missing (response len: {}, {diag})",
                    state.channel_id,
                    best_response.len()
                );
            } else {
                println!(
                    "  [{ts}] ⚠ cannot recover inflight turn for channel {}: tmux session missing (response len: {})",
                    state.channel_id,
                    best_response.len()
                );
            }
            let _ = super::formatting::replace_long_message_raw(
                http,
                channel_id,
                current_msg_id,
                &stale_text,
                shared,
            )
            .await;
            save_missing_session_handoff(provider, &state, &best_response);
            clear_inflight_state(provider, state.channel_id);
            continue;
        }

        let Some(tmux_session_name) = tmux_session_name else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ clearing inflight turn for channel {}: tmux session name missing",
                state.channel_id
            );
            clear_inflight_state(provider, state.channel_id);
            continue;
        };
        let Some(output_path) = output_path else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ clearing inflight turn for channel {}: output path missing",
                state.channel_id
            );
            clear_inflight_state(provider, state.channel_id);
            continue;
        };
        let Some(input_fifo_path) = input_fifo_path else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ clearing inflight turn for channel {}: input fifo path missing",
                state.channel_id
            );
            clear_inflight_state(provider, state.channel_id);
            continue;
        };

        // If tmux pane is alive, skip recovery reader entirely.
        // The session is idle (waiting for input) — spawn a watcher immediately
        // instead of deferring to restore_tmux_watchers() to avoid a race
        // condition where the session could die in the gap between recovery and
        // restore_tmux_watchers (~50s), losing the response.
        let pane_alive = tmux_session_alive_with_retry(&tmux_session_name);
        if pane_alive {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ↻ inflight recovery: pane alive for channel {}, spawning watcher immediately",
                state.channel_id
            );
            // Register session in-memory so handlers can find it.
            let effective_channel_name = channel_name.clone().or_else(|| {
                crate::services::provider::parse_provider_and_channel_from_tmux_name(
                    &tmux_session_name,
                )
                .map(|(_, ch)| ch)
            });
            {
                let channel_key = channel_id.get().to_string();
                let last_path = settings_snapshot.last_sessions.get(&channel_key).cloned();
                let saved_remote = settings_snapshot.last_remotes.get(&channel_key).cloned();
                let mut data = shared.core.lock().await;
                let session = data
                    .sessions
                    .entry(channel_id)
                    .or_insert_with(|| DiscordSession {
                        session_id: state.session_id.clone(),
                        current_path: None,
                        history: Vec::new(),
                        pending_uploads: Vec::new(),
                        cleared: false,
                        remote_profile_name: saved_remote,
                        channel_id: Some(channel_id.get()),
                        channel_name: effective_channel_name.clone(),
                        category_name: None,
                        last_active: tokio::time::Instant::now(),
                        worktree: None,
                        born_generation: super::runtime_store::load_generation(),
                    });
                session.channel_id = Some(channel_id.get());
                session.last_active = tokio::time::Instant::now();
                if session.current_path.is_none() {
                    session.current_path = last_path;
                }
                if session.channel_name.is_none() {
                    session.channel_name = effective_channel_name;
                }
            }

            // Immediately spawn watcher to avoid race condition.
            if std::fs::metadata(&output_path).is_ok()
                && !shared.tmux_watchers.contains_key(&channel_id)
            {
                let (initial_offset, current_len, truncated) =
                    recovery_watcher_start_offset(&output_path, state.last_offset);
                let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let resume_offset = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
                let pause_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                let turn_delivered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                shared.tmux_watchers.insert(
                    channel_id,
                    TmuxWatcherHandle {
                        paused: paused.clone(),
                        resume_offset: resume_offset.clone(),
                        cancel: cancel.clone(),
                        pause_epoch: pause_epoch.clone(),
                        turn_delivered: turn_delivered.clone(),
                    },
                );
                let ts2 = chrono::Local::now().format("%H:%M:%S");
                if truncated {
                    println!(
                        "  [{ts2}] ↻ recovery: output truncated for #{} (saved offset {}, file len {}), restarting watcher from 0",
                        tmux_session_name, state.last_offset, current_len
                    );
                }
                println!(
                    "  [{ts2}] 👁 recovery: spawned watcher for #{} at offset {}",
                    tmux_session_name, initial_offset
                );
                tokio::spawn(super::tmux::tmux_output_watcher(
                    channel_id,
                    http.clone(),
                    shared.clone(),
                    output_path.clone(),
                    tmux_session_name.clone(),
                    initial_offset,
                    cancel,
                    paused,
                    resume_offset,
                    pause_epoch,
                    turn_delivered,
                ));
            }

            clear_inflight_state(provider, state.channel_id);
            continue;
        }

        shared
            .recovering_channels
            .insert(channel_id, std::time::Instant::now());

        let channel_key = channel_id.get().to_string();
        let last_path = settings_snapshot.last_sessions.get(&channel_key).cloned();
        let saved_remote = settings_snapshot.last_remotes.get(&channel_key).cloned();

        let cancel_token = Arc::new(CancelToken::new());
        if let Ok(mut guard) = cancel_token.tmux_session.lock() {
            *guard = Some(tmux_session_name.clone());
        }

        {
            let mut data = shared.core.lock().await;
            let session = data
                .sessions
                .entry(channel_id)
                .or_insert_with(|| DiscordSession {
                    session_id: state.session_id.clone(),
                    current_path: None,
                    history: Vec::new(),
                    pending_uploads: Vec::new(),
                    cleared: false,
                    remote_profile_name: saved_remote.clone(),
                    channel_id: Some(channel_id.get()),
                    channel_name: channel_name.clone(),
                    category_name: None,
                    last_active: tokio::time::Instant::now(),
                    worktree: None,

                    born_generation: super::runtime_store::load_generation(),
                });
            session.channel_id = Some(channel_id.get());
            session.last_active = tokio::time::Instant::now();
            if session.current_path.is_none() {
                session.current_path = last_path.clone();
            }
            if session.channel_name.is_none() {
                session.channel_name = channel_name.clone();
            }
            if session.remote_profile_name.is_none() {
                session.remote_profile_name = saved_remote;
            }
            if !data.cancel_tokens.contains_key(&channel_id) {
                shared
                    .global_active
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            data.cancel_tokens.insert(channel_id, cancel_token.clone());
            data.active_request_owner
                .insert(channel_id, UserId::new(state.request_owner_user_id));
        }

        let role_binding = resolve_role_binding(channel_id, channel_name.as_deref());
        let adk_session_key = build_adk_session_key(shared, channel_id, provider).await;
        let adk_session_name = channel_name.clone();
        let adk_session_info = derive_adk_session_info(
            Some(&state.user_text),
            channel_name.as_deref(),
            last_path.as_deref(),
        );
        post_adk_session_status(
            adk_session_key.as_deref(),
            adk_session_name.as_deref(),
            Some(provider.as_str()),
            "working",
            provider,
            Some(&adk_session_info),
            None,
            last_path.as_deref(),
            parse_dispatch_id(&state.user_text).as_deref(),
            shared.api_port,
        )
        .await;

        let (tx, rx) = mpsc::channel();
        let cancel_for_reader = cancel_token.clone();
        let output_for_reader = output_path.clone();
        let input_for_reader = input_fifo_path.clone();
        let tmux_for_reader = tmux_session_name.clone();
        let start_offset = state.last_offset;
        let recovery_session_id = state.session_id.clone();
        let retry_channel_id = channel_id.get();
        std::thread::spawn(move || {
            match claude::read_output_file_until_result(
                &output_for_reader,
                start_offset,
                tx.clone(),
                Some(cancel_for_reader),
                claude::SessionProbe::tmux(tmux_for_reader.clone()),
            ) {
                Ok(ReadOutputResult::Completed { offset })
                | Ok(ReadOutputResult::Cancelled { offset }) => {
                    let _ = tx.send(StreamMessage::TmuxReady {
                        output_path: output_for_reader,
                        input_fifo_path: input_for_reader,
                        tmux_session_name: tmux_for_reader,
                        last_offset: offset,
                    });
                }
                Ok(ReadOutputResult::SessionDied { offset }) => {
                    // Check if tmux pane is actually alive — dcserver restart
                    // may cause SessionDied because no new output arrived, but
                    // the Claude CLI process could still be idle (waiting for input).
                    let pane_alive = tmux_session_alive_with_retry(&tmux_for_reader);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    if pane_alive {
                        // Session is alive but idle — hand off to watcher instead of retrying
                        eprintln!(
                            "  [{ts}] ↻ Recovery: session idle but pane alive — handing off to watcher (channel {})",
                            retry_channel_id
                        );
                        let _ = tx.send(StreamMessage::TmuxReady {
                            output_path: output_for_reader,
                            input_fifo_path: input_for_reader,
                            tmux_session_name: tmux_for_reader,
                            last_offset: offset,
                        });
                    } else {
                        // Session truly dead during restart recovery — hand off
                        // to the internal post-restart follow-up path instead
                        // of exposing Discord auto-retry history to the user.
                        eprintln!(
                            "  [{ts}] ↻ Recovery: session died, signaling internal handoff (channel {})",
                            retry_channel_id
                        );
                        let _ = tx.send(StreamMessage::Done {
                            result: RESTART_SESSION_DIED_HANDOFF_SENTINEL.to_string(),
                            session_id: recovery_session_id,
                        });
                    }
                }
                Err(e) => {
                    let _ = tx.send(StreamMessage::Error {
                        message: e,
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: None,
                    });
                }
            }
        });

        let recovery_dispatch_id = parse_dispatch_id(&state.user_text);
        // Backfill session_key/dispatch_id on inflight state for long-turn detection ([L]).
        let mut state = state;
        state.session_key = state.session_key.or_else(|| adk_session_key.clone());
        state.dispatch_id = state.dispatch_id.or_else(|| recovery_dispatch_id.clone());

        spawn_turn_bridge(
            http.clone(),
            shared.clone(),
            cancel_token,
            rx,
            TurnBridgeContext {
                provider: provider.clone(),
                channel_id,
                user_msg_id,
                user_text_owned: state.user_text.clone(),
                request_owner_name: String::new(),
                request_owner: None,
                serenity_ctx: None,
                token: None,
                role_binding,
                adk_session_key,
                adk_session_name,
                adk_session_info: Some(adk_session_info),
                adk_cwd: last_path.clone(),
                dispatch_id: recovery_dispatch_id,
                current_msg_id,
                response_sent_offset: state.response_sent_offset,
                full_response: state.full_response.clone(),
                tmux_last_offset: Some(state.last_offset),
                new_session_id: state.session_id.clone(),
                defer_watcher_resume: false,
                completion_tx: None,
                inflight_state: state,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::provider::ProviderKind;
    use std::io::Write;

    #[test]
    fn detects_result_after_offset_only_in_remaining_slice() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"before\"}}]}}}}"
        )
        .unwrap();
        let offset = file.as_file().metadata().unwrap().len();
        writeln!(
            file,
            "{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}}"
        )
        .unwrap();

        assert!(output_has_result_after_offset(
            file.path().to_str().unwrap(),
            offset
        ));
    }

    #[test]
    fn ignores_result_before_offset() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(
            file,
            "{{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}}"
        )
        .unwrap();
        let offset = file.as_file().metadata().unwrap().len();
        writeln!(
            file,
            "{{\"type\":\"assistant\",\"message\":{{\"content\":[{{\"type\":\"text\",\"text\":\"after\"}}]}}}}"
        )
        .unwrap();

        assert!(!output_has_result_after_offset(
            file.path().to_str().unwrap(),
            offset
        ));
    }

    #[test]
    fn detects_new_bytes_after_offset() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "before").unwrap();
        let offset = file.as_file().metadata().unwrap().len();
        writeln!(file, "after").unwrap();

        assert!(output_has_bytes_after_offset(
            file.path().to_str().unwrap(),
            offset
        ));
    }

    #[test]
    fn ignores_missing_new_bytes_after_offset() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "before").unwrap();
        let offset = file.as_file().metadata().unwrap().len();

        assert!(!output_has_bytes_after_offset(
            file.path().to_str().unwrap(),
            offset
        ));
    }

    fn write_jsonl(lines: &[&str]) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for line in lines {
            writeln!(file, "{}", line).unwrap();
        }
        file.flush().unwrap();
        file
    }

    #[test]
    fn recovery_text_then_tool_then_result_prefers_result() {
        // Text -> ToolUse -> Done(result): pre-tool narration should be replaced
        let file = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"이슈를 생성합니다."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"echo hi"}}]}}"#,
            r#"{"type":"result","subtype":"success","result":"이슈 #42를 생성했습니다."}"#,
        ]);
        let resp = extract_response_from_output_pub(file.path().to_str().unwrap(), 0);
        assert_eq!(resp, "이슈 #42를 생성했습니다.");
    }

    #[test]
    fn recovery_text_only_returns_text() {
        let file = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"안녕하세요"}]}}"#,
            r#"{"type":"result","subtype":"success","result":"done"}"#,
        ]);
        let resp = extract_response_from_output_pub(file.path().to_str().unwrap(), 0);
        assert_eq!(resp, "안녕하세요");
    }

    #[test]
    fn recovery_mixed_text_tool_in_single_block_prefers_result() {
        // Single assistant message with [text, tool_use] — text is pre-tool narration
        let file = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"작업 시작"},{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"result","subtype":"success","result":"완료했습니다."}"#,
        ]);
        let resp = extract_response_from_output_pub(file.path().to_str().unwrap(), 0);
        assert_eq!(resp, "완료했습니다.");
    }

    #[test]
    fn recovery_tool_then_post_tool_text_keeps_text() {
        // Text -> ToolUse -> post-tool Text: should keep accumulated text
        let file = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"시작합니다."}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"결과를 확인했습니다."}]}}"#,
            r#"{"type":"result","subtype":"success","result":"done"}"#,
        ]);
        let resp = extract_response_from_output_pub(file.path().to_str().unwrap(), 0);
        assert_eq!(resp, "시작합니다.결과를 확인했습니다.");
    }

    #[test]
    fn recovery_empty_response_uses_result() {
        let file =
            write_jsonl(&[r#"{"type":"result","subtype":"success","result":"결과만 있음"}"#]);
        let resp = extract_response_from_output_pub(file.path().to_str().unwrap(), 0);
        assert_eq!(resp, "결과만 있음");
    }

    #[test]
    fn recovery_error_result_not_used() {
        // Error results should not override text
        let file = write_jsonl(&[
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"진행 중"}]}}"#,
            r#"{"type":"result","subtype":"error","is_error":true,"result":"crash"}"#,
        ]);
        let resp = extract_response_from_output_pub(file.path().to_str().unwrap(), 0);
        assert_eq!(resp, "진행 중");
    }

    #[test]
    fn recovery_respects_start_offset() {
        // Only data after offset should be considered
        let mut file = tempfile::NamedTempFile::new().unwrap();
        let line1 =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"이전 턴"}]}}"#;
        writeln!(file, "{}", line1).unwrap();
        let offset = file.as_file().metadata().unwrap().len();
        let line2 =
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"새 턴"}]}}"#;
        writeln!(file, "{}", line2).unwrap();
        file.flush().unwrap();

        let resp = extract_response_from_output_pub(file.path().to_str().unwrap(), offset);
        assert_eq!(resp, "새 턴");
    }

    // ========== output_has_result_after_offset: error result tests ==========

    #[test]
    fn error_result_not_treated_as_completion() {
        let file = write_jsonl(&[
            r#"{"type":"result","subtype":"error","is_error":true,"errors":["crash"]}"#,
        ]);
        assert!(!output_has_result_after_offset(
            file.path().to_str().unwrap(),
            0
        ));
    }

    #[test]
    fn success_result_treated_as_completion() {
        let file = write_jsonl(&[r#"{"type":"result","subtype":"success","result":"done"}"#]);
        assert!(output_has_result_after_offset(
            file.path().to_str().unwrap(),
            0
        ));
    }

    #[test]
    fn error_result_before_success_still_completes() {
        // Error followed by success — the success should be detected
        let file = write_jsonl(&[
            r#"{"type":"result","subtype":"error","is_error":true,"errors":["retry"]}"#,
            r#"{"type":"result","subtype":"success","result":"ok"}"#,
        ]);
        assert!(output_has_result_after_offset(
            file.path().to_str().unwrap(),
            0
        ));
    }

    #[test]
    fn missing_session_recovery_saves_handoff_for_followup_turn() {
        let _lock = super::super::runtime_store::test_env_lock().lock().unwrap();
        let temp = tempfile::TempDir::new().unwrap();
        let root = temp.path().join("agentdesk-root");
        std::fs::create_dir_all(root.join("runtime")).unwrap();

        struct EnvReset;
        impl Drop for EnvReset {
            fn drop(&mut self) {
                unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
            }
        }

        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", &root) };
        let _reset = EnvReset;

        let state = crate::services::discord::inflight::InflightTurnState {
            version: 1,
            provider: "codex".to_string(),
            channel_id: 1486333430516945008,
            channel_name: Some("adk-cdx-t1486333430516945008".to_string()),
            request_owner_user_id: 343742347365974026,
            user_msg_id: 1487795113240559788,
            current_msg_id: 1487799916758827138,
            current_msg_len: 0,
            user_text: "릴리즈하다가 응답이 끊겼어. 이어서 설명해줘.".to_string(),
            session_id: Some("session-1".to_string()),
            tmux_session_name: Some("AgentDesk-codex-adk-cdx-t1486333430516945008".to_string()),
            output_path: Some("/tmp/agentdesk-test.jsonl".to_string()),
            input_fifo_path: Some("/tmp/agentdesk-test.input".to_string()),
            last_offset: 123,
            full_response: "중간까지 정리했습니다.".to_string(),
            response_sent_offset: 0,
            current_tool_line: None,
            started_at: "2026-03-29 22:00:34".to_string(),
            updated_at: "2026-03-29 22:03:53".to_string(),
            born_generation: 7,
            any_tool_used: true,
            has_post_tool_text: false,
            session_key: None,
            dispatch_id: None,
        };

        save_missing_session_handoff(
            &ProviderKind::Codex,
            &state,
            "이미 확인한 내용은 여기까지입니다. 이어서 원인과 대응을 설명하겠습니다.",
        );

        let handoffs = crate::services::discord::handoff::load_handoffs(&ProviderKind::Codex);
        assert_eq!(handoffs.len(), 1);
        assert_eq!(handoffs[0].channel_id, state.channel_id);
        assert_eq!(handoffs[0].user_msg_id, Some(state.user_msg_id));
        assert!(handoffs[0].intent.contains("중단된 응답"));
        assert!(handoffs[0].context.contains("원래 사용자 요청"));
        assert!(handoffs[0].context.contains("partial 응답"));
        assert!(
            handoffs[0]
                .context
                .contains("이어서 원인과 대응을 설명하겠습니다")
        );
    }

    #[test]
    fn recovery_watcher_resume_uses_saved_offset_when_file_grew() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "before").unwrap();
        let saved_offset = file.as_file().metadata().unwrap().len();
        writeln!(file, "after").unwrap();
        file.flush().unwrap();

        let (offset, current_len, truncated) =
            recovery_watcher_start_offset(file.path().to_str().unwrap(), saved_offset);
        assert_eq!(offset, saved_offset);
        assert!(current_len >= saved_offset);
        assert!(!truncated);
    }

    #[test]
    fn recovery_watcher_resume_rewinds_when_file_was_truncated() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        writeln!(file, "new session output").unwrap();
        file.flush().unwrap();

        let saved_offset = file.as_file().metadata().unwrap().len() + 100;
        let (offset, current_len, truncated) =
            recovery_watcher_start_offset(file.path().to_str().unwrap(), saved_offset);
        assert_eq!(offset, 0);
        assert!(current_len < saved_offset);
        assert!(truncated);
    }
}
