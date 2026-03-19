use super::shared_memory::latest_shared_memory_ts;
use super::turn_bridge::stale_inflight_message;
use super::*;
use crate::services::tmux_diagnostics::{build_tmux_death_diagnostic, tmux_session_has_live_pane};

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
            serde_json::from_str::<serde_json::Value>(trimmed)
                .ok()
                .and_then(|value| {
                    value
                        .get("type")
                        .and_then(|kind| kind.as_str())
                        .map(str::to_string)
                })
                .as_deref()
                == Some("result")
        })
}

/// Extract accumulated assistant text from output JSONL after the given offset.
fn extract_response_from_output(output_path: &str, start_offset: u64) -> String {
    extract_response_from_output_pub(output_path, start_offset)
}

/// Public wrapper for turn_bridge fallback recovery.
pub(super) fn extract_response_from_output_pub(output_path: &str, start_offset: u64) -> String {
    let Ok(bytes) = std::fs::read(output_path) else {
        return String::new();
    };
    let start = usize::try_from(start_offset)
        .ok()
        .map(|offset| offset.min(bytes.len()))
        .unwrap_or(bytes.len());

    let mut response = String::new();
    for line in String::from_utf8_lossy(&bytes[start..]).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };
        let msg_type = value.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if msg_type == "assistant" {
            if let Some(content) = value.get("message").and_then(|m| m.get("content")) {
                if let Some(arr) = content.as_array() {
                    for block in arr {
                        if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                                response.push_str(text);
                            }
                        }
                    }
                }
            }
        }
    }
    response
}

fn output_has_bytes_after_offset(output_path: &str, start_offset: u64) -> bool {
    std::fs::metadata(output_path)
        .map(|meta| meta.len() > start_offset)
        .unwrap_or(false)
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
        // Generation gate: skip recovery for turns born in a previous generation.
        // These old sessions should not be followed up — the new dcserver should
        // start fresh sessions instead.
        if state.born_generation < current_gen {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⏭ skipping inflight recovery for channel {}: old generation (born={}, current={})",
                state.channel_id, state.born_generation, current_gen
            );
            // Update the Discord message so "Processing..." doesn't stay forever
            if state.current_msg_id != 0 {
                let channel_id = ChannelId::new(state.channel_id);
                let current_msg_id = MessageId::new(state.current_msg_id);
                let stale_text =
                    super::turn_bridge::stale_inflight_message(&state.full_response);
                let _ = super::formatting::replace_long_message_raw(
                    http,
                    channel_id,
                    current_msg_id,
                    &stale_text,
                    shared,
                )
                .await;
            }
            clear_inflight_state(provider, state.channel_id);
            continue;
        }

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
                super::restart_report::clear_restart_report(provider, state.channel_id);
                clear_inflight_state(provider, state.channel_id);
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
                    if name.starts_with("remoteCC-") {
                        name.to_string()
                    } else {
                        provider.build_tmux_session_name(name)
                    }
                });
            let session_alive = tmux_name
                .as_deref()
                .map_or(false, tmux_session_has_live_pane);

            if session_alive {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ↻ restart report exists but tmux session alive for channel {}: clearing report, proceeding with watcher recovery",
                    state.channel_id
                );
                super::restart_report::clear_restart_report(provider, state.channel_id);
                // Add 👀 reaction to bot placeholder to indicate watcher re-attached
                super::formatting::add_reaction_raw(
                    http,
                    ChannelId::new(state.channel_id),
                    MessageId::new(state.current_msg_id),
                    '👀',
                )
                .await;
                // Fall through to normal recovery path below (watcher re-attach)
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                if let Some(diag) = tmux_name.as_deref().and_then(|name| {
                    build_tmux_death_diagnostic(name, output_path_for_check.as_deref())
                }) {
                    println!(
                        "  [{ts}] ⏭ skipping inflight recovery for channel {}: restart report exists, session dead, delegating to flush loop ({diag})",
                        state.channel_id
                    );
                } else {
                    println!(
                        "  [{ts}] ⏭ skipping inflight recovery for channel {}: restart report exists, session dead, delegating to flush loop",
                        state.channel_id
                    );
                }
                clear_inflight_state(provider, state.channel_id);
                continue;
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
            .or_else(|| (!fallback_output.is_empty()).then_some(fallback_output.clone()));
        let input_fifo_path = state
            .input_fifo_path
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| (!fallback_input.is_empty()).then_some(fallback_input.clone()));
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
            clear_inflight_state(provider, state.channel_id);
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

        let can_recover = tmux_session_name.as_deref().map_or(false, |name| {
            std::process::Command::new("tmux")
                .args(["has-session", "-t", name])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        });

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
                    state.channel_id, best_response.len()
                );
            } else {
                println!(
                    "  [{ts}] ⚠ cannot recover inflight turn for channel {}: tmux session missing (response len: {})",
                    state.channel_id, best_response.len()
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
                    last_shared_memory_ts: None,
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
            // Restore shared memory dedup timestamp to prevent re-injection after restart
            if session.last_shared_memory_ts.is_none() {
                let role_id = resolve_role_binding(channel_id, channel_name.as_deref())
                    .map(|b| b.role_id.clone());
                if let Some(ref rid) = role_id {
                    session.last_shared_memory_ts = latest_shared_memory_ts(rid);
                }
            }
            if !data.cancel_tokens.contains_key(&channel_id) {
                shared.global_active.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            data.cancel_tokens.insert(channel_id, cancel_token.clone());
            data.active_request_owner
                .insert(channel_id, UserId::new(state.request_owner_user_id));
        }

        let role_binding = resolve_role_binding(channel_id, channel_name.as_deref());
        let pcd_session_key = build_pcd_session_key(shared, channel_id, provider).await;
        let pcd_session_name = channel_name.clone();
        let pcd_session_info = derive_pcd_session_info(
            Some(&state.user_text),
            channel_name.as_deref(),
            last_path.as_deref(),
        );
        post_pcd_session_status(
            pcd_session_key.as_deref(),
            pcd_session_name.as_deref(),
            Some(provider.as_str()),
            "working",
            provider,
            Some(&pcd_session_info),
            None,
            last_path.as_deref(),
            None, // dispatch_id not available during recovery
        )
        .await;

        let (tx, rx) = mpsc::channel();
        let cancel_for_reader = cancel_token.clone();
        let output_for_reader = output_path.clone();
        let input_for_reader = input_fifo_path.clone();
        let tmux_for_reader = tmux_session_name.clone();
        let start_offset = state.last_offset;
        let recovery_session_id = state.session_id.clone();
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
                Ok(ReadOutputResult::SessionDied { .. }) => {
                    let _ = tx.send(StreamMessage::Done {
                        result: "⚠️ RemoteCC 재시작 중 진행되던 세션을 복구하지 못했습니다."
                            .to_string(),
                        session_id: recovery_session_id,
                    });
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
                pcd_session_key,
                pcd_session_name,
                pcd_session_info: Some(pcd_session_info),
                pcd_cwd: last_path.clone(),
                dispatch_id: None,
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
    use super::{output_has_bytes_after_offset, output_has_result_after_offset};
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
}
