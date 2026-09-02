//! Local Qwen process/tmux lifecycle and fresh-session recreation.
use super::*;

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_streaming_local_tmux(
    prompt: &str,
    model: Option<&str>,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    tmux_session_name: &str,
    qwen_resolution: &crate::services::platform::BinaryResolution,
    allowed_core_tools: Option<&[String]>,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    force_fresh_provider_session: bool,
) -> Result<(), String> {
    let resume_session_id = if force_fresh_provider_session {
        None
    } else {
        validated_resume_session_id(session_id)?
    };
    let output_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    let input_fifo_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "input");
    let prompt_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "prompt");
    let owner_path = tmux_owner_path(tmux_session_name);
    // Accept either the new persistent location or the legacy /tmp location
    // so that dcserver restarts that lost /tmp files still re-attach to a
    // live tmux pane owned by an older wrapper. See issue #892.
    let session_exists = tmux_session_exists(tmux_session_name);
    let resolved_output =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "jsonl");
    let resolved_input =
        crate::services::tmux_common::resolve_session_temp_path(tmux_session_name, "input");
    let has_live_pane = tmux_session_has_live_pane(tmux_session_name);
    let session_usable = has_live_pane && resolved_output.is_some() && resolved_input.is_some();

    if force_fresh_provider_session {
        if session_exists || has_live_pane {
            record_tmux_exit_reason(
                tmux_session_name,
                "forced fresh provider session cleanup before recreate",
            );
            crate::services::platform::tmux::kill_session(
                tmux_session_name,
                "forced fresh provider session cleanup before recreate",
            );
        }
    } else if session_usable {
        let output_path = resolved_output
            .clone()
            .unwrap_or_else(|| output_path.clone());
        let input_fifo_path = resolved_input
            .clone()
            .unwrap_or_else(|| input_fifo_path.clone());
        match send_followup_to_tmux(
            prompt,
            &output_path,
            &input_fifo_path,
            sender.clone(),
            cancel_token.clone(),
            tmux_session_name,
        )? {
            FollowupResult::Delivered => return Ok(()),
            FollowupResult::RecreateSession { error } => {
                record_tmux_exit_reason(
                    tmux_session_name,
                    &format!("followup failed, recreating: {}", error),
                );
                crate::services::platform::tmux::kill_session(
                    tmux_session_name,
                    &format!("followup failed, recreating: {}", error),
                );
            }
        }
    } else if should_preserve_live_reused_provider_session(resume_session_id, has_live_pane) {
        tracing::warn!(
            tmux_session_name,
            session_id = resume_session_id.unwrap_or_default(),
            output_path_present = resolved_output.is_some(),
            input_path_present = resolved_input.is_some(),
            "refusing to kill live Qwen tmux selected for provider-session reuse"
        );
        return Err(format!(
            "live Qwen tmux session {tmux_session_name} was selected for reuse but wrapper I/O is unavailable; refusing stale cleanup/recreate"
        ));
    } else if session_exists {
        record_tmux_exit_reason(
            tmux_session_name,
            "stale local session cleanup before recreate",
        );
        crate::services::platform::tmux::kill_session(
            tmux_session_name,
            "stale local session cleanup before recreate",
        );
    }

    crate::services::tmux_common::cleanup_session_temp_files(tmux_session_name);
    if let Some(channel_id) = report_channel_id {
        crate::services::tui_prompt_dedupe::register_tmux_channel(tmux_session_name, channel_id);
    }

    std::fs::write(&output_path, "").map_err(|e| format!("Failed to create output file: {}", e))?;

    let mkfifo = Command::new("mkfifo")
        .arg(&input_fifo_path)
        .output()
        .map_err(|e| format!("Failed to create input FIFO: {}", e))?;
    if !mkfifo.status.success() {
        let _ = std::fs::remove_file(&output_path);
        return Err(format!(
            "mkfifo failed: {}",
            String::from_utf8_lossy(&mkfifo.stderr)
        ));
    }

    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("Failed to write prompt file: {}", e))?;
    write_tmux_owner_marker(tmux_session_name)?;

    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;
    let qwen_bin = qwen_resolution
        .resolved_path
        .as_deref()
        .ok_or_else(|| "Qwen CLI not found".to_string())?;
    let script_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "sh");

    let mut env_lines = String::new();
    if let Some(exec_path) = qwen_resolution.exec_path.as_deref() {
        env_lines.push_str(&format!(
            "export PATH='{}'\n",
            exec_path.replace('\'', "'\\''")
        ));
    }
    if let Ok(root_dir) = std::env::var("AGENTDESK_ROOT_DIR") {
        let trimmed = root_dir.trim();
        if !trimmed.is_empty() {
            env_lines.push_str(&format!(
                "export AGENTDESK_ROOT_DIR='{}'\n",
                trimmed.replace('\'', "'\\''")
            ));
        }
    }
    if let Some(channel_id) = report_channel_id {
        env_lines.push_str(&format!(
            "export {}={}\n",
            RESTART_REPORT_CHANNEL_ENV, channel_id
        ));
    }
    if let Some(provider) = report_provider {
        env_lines.push_str(&format!(
            "export {}={}\n",
            RESTART_REPORT_PROVIDER_ENV,
            provider.as_str()
        ));
    }

    let script_content = format!(
        "#!/bin/bash\n\
        {env}\
        exec {exe} qwen-tmux-wrapper \\\n  \
        --output-file {output} \\\n  \
        --input-fifo {input_fifo} \\\n  \
        --prompt-file {prompt} \\\n  \
        --cwd {wd} \\\n  \
        --qwen-bin {qwen_bin}{model_arg}{fresh_arg}{resume_arg}{core_tool_args}\n",
        env = env_lines,
        exe = shell_escape(&exe.display().to_string()),
        output = shell_escape(&output_path),
        input_fifo = shell_escape(&input_fifo_path),
        prompt = shell_escape(&prompt_path),
        wd = shell_escape(working_dir),
        qwen_bin = shell_escape(qwen_bin),
        model_arg = model
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!(" \\\n  --qwen-model {}", shell_escape(value)))
            .unwrap_or_default(),
        fresh_arg = if force_fresh_provider_session {
            " \\\n  --fresh-session".to_string()
        } else {
            String::new()
        },
        resume_arg = resume_session_id
            .map(|value| format!(" \\\n  --resume-session-id {}", shell_escape(value)))
            .unwrap_or_default(),
        core_tool_args = allowed_core_tools
            .map(|tools| {
                tools
                    .iter()
                    .map(|tool| format!(" \\\n  --qwen-core-tool {}", shell_escape(tool)))
                    .collect::<String>()
            })
            .unwrap_or_default(),
    );

    std::fs::write(&script_path, &script_content)
        .map_err(|e| format!("Failed to write launch script: {}", e))?;

    crate::services::tui_prompt_dedupe::record_discord_originated_prompt(
        ProviderKind::Qwen.as_str(),
        tmux_session_name,
        prompt,
    );
    let tmux_result = crate::services::platform::tmux::create_session(
        tmux_session_name,
        Some(working_dir),
        &format!("bash {}", shell_escape(&script_path)),
    )?;

    if !tmux_result.status.success() {
        let stderr = String::from_utf8_lossy(&tmux_result.stderr);
        let _ = std::fs::remove_file(&output_path);
        let _ = std::fs::remove_file(&input_fifo_path);
        let _ = std::fs::remove_file(&prompt_path);
        let _ = std::fs::remove_file(&owner_path);
        let _ = std::fs::remove_file(&script_path);
        crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
            ProviderKind::Qwen.as_str(),
            tmux_session_name,
            prompt,
        );
        return Err(format!("tmux error: {}", stderr));
    }

    crate::services::platform::tmux::set_option(tmux_session_name, "remain-on-exit", "on");

    // #3087: stamp a per-spawn nonce in a SEPARATE marker (see claude.rs). The
    // status-panel session-instance key reads this unique nonce instead of the
    // `.generation` mtime, eliminating mtime missing/duplicate collisions.
    if let Err(e) = crate::services::discord::stamp_spawn_markers(tmux_session_name) {
        tracing::warn!("failed to write spawn nonce for {tmux_session_name}: {e}");
    }

    if let Some(ref token) = cancel_token {
        token.bind_unmanaged_session_name(tmux_session_name);
    }

    let read_result = match qwen_read_output_file_until_result(
        &output_path,
        0,
        sender.clone(),
        cancel_token,
        SessionProbe::tmux(tmux_session_name.to_string(), ProviderKind::Qwen),
        Some(tmux_session_name),
    ) {
        Ok(read_result) => read_result,
        Err(error) => {
            crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
                ProviderKind::Qwen.as_str(),
                tmux_session_name,
                prompt,
            );
            return Err(error);
        }
    };

    match read_result {
        ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
            register_qwen_tmux_runtime_binding(
                tmux_session_name,
                &output_path,
                &input_fifo_path,
                offset,
            );
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path,
                input_fifo_path,
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
        }
        ReadOutputResult::SessionDied { .. } => {
            crate::services::tui_prompt_dedupe::remove_discord_originated_prompt(
                ProviderKind::Qwen.as_str(),
                tmux_session_name,
                prompt,
            );
            let _ = sender.send(StreamMessage::Done {
                result: "⚠ 세션이 종료되었습니다. 새 메시지를 보내면 새 세션이 시작됩니다."
                    .to_string(),
                session_id: None,
            });
        }
    }

    Ok(())
}
#[allow(clippy::too_many_arguments)]
pub(super) fn execute_streaming_local_process(
    prompt: &str,
    model: Option<&str>,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    session_name: &str,
    qwen_resolution: &crate::services::platform::BinaryResolution,
    allowed_core_tools: Option<&[String]>,
    force_fresh_provider_session: bool,
) -> Result<(), String> {
    use crate::services::session_backend::{ProcessBackend, SessionBackend, SessionConfig};

    let resume_session_id = if force_fresh_provider_session {
        None
    } else {
        validated_resume_session_id(session_id)?
    };
    let output_path = format!(
        "{}/agentdesk-{}.jsonl",
        std::env::temp_dir().display(),
        session_name
    );
    let prompt_path = format!(
        "{}/agentdesk-{}.prompt",
        std::env::temp_dir().display(),
        session_name
    );

    if force_fresh_provider_session {
        // A fresh routine must not send its prompt to a warm pipe-mode wrapper.
        // Drop and terminate any registered process before creating the new one.
        let _ = terminate_process_session(session_name);
    } else if process_session_is_alive(session_name) {
        let start_offset = std::fs::metadata(&output_path)
            .map(|m| m.len())
            .unwrap_or(0);
        let encoded = format!(
            "{}{}",
            TMUX_PROMPT_B64_PREFIX,
            BASE64_STANDARD.encode(prompt.as_bytes())
        );
        send_process_session_input(session_name, &encoded)?;
        let read_result = qwen_read_output_file_until_result(
            &output_path,
            start_offset,
            sender.clone(),
            cancel_token,
            process_session_probe(session_name),
            None,
        )?;

        match read_result {
            ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
                let _ = sender.send(StreamMessage::ProcessReady {
                    output_path: output_path.to_string(),
                    session_name: session_name.to_string(),
                    last_offset: offset,
                });
            }
            ReadOutputResult::SessionDied { .. } => {
                let _ = sender.send(StreamMessage::Done {
                    result: "⚠ 세션이 종료되었습니다.".to_string(),
                    session_id: None,
                });
                remove_process_session(session_name);
            }
        }
        return Ok(());
    }

    let _ = std::fs::remove_file(&output_path);
    let _ = std::fs::remove_file(&prompt_path);
    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("Failed to write prompt file: {}", e))?;

    let qwen_bin = qwen_resolution
        .resolved_path
        .as_deref()
        .ok_or_else(|| "Qwen CLI not found".to_string())?;
    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;

    let config = SessionConfig {
        session_name: session_name.to_string(),
        working_dir: working_dir.to_string(),
        agentdesk_exe: exe.display().to_string(),
        output_path: output_path.clone(),
        prompt_path: prompt_path.clone(),
        wrapper_subcommand: "qwen-tmux-wrapper".to_string(),
        wrapper_args: {
            let mut args = vec!["--qwen-bin".to_string(), qwen_bin.to_string()];
            if let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) {
                args.push("--qwen-model".to_string());
                args.push(model.to_string());
            }
            if force_fresh_provider_session {
                args.push("--fresh-session".to_string());
            }
            if let Some(session_id) = resume_session_id {
                args.push("--resume-session-id".to_string());
                args.push(session_id.to_string());
            }
            if let Some(core_tools) = allowed_core_tools {
                for tool in core_tools {
                    args.push("--qwen-core-tool".to_string());
                    args.push(tool.to_string());
                }
            }
            args
        },
        env_vars: qwen_resolution
            .exec_path
            .as_ref()
            .map(|exec_path| vec![("PATH".to_string(), exec_path.clone())])
            .unwrap_or_default(),
    };

    let backend = ProcessBackend::new();
    let handle = backend.create_session(&config)?;

    register_child_pid(cancel_token.as_deref(), handle.pid());

    insert_process_session(session_name.to_string(), handle);

    let read_result = qwen_read_output_file_until_result(
        &output_path,
        0,
        sender.clone(),
        cancel_token,
        process_session_probe(session_name),
        None,
    )?;

    match read_result {
        ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
            let _ = sender.send(StreamMessage::ProcessReady {
                output_path,
                session_name: session_name.to_string(),
                last_offset: offset,
            });
        }
        ReadOutputResult::SessionDied { .. } => {
            let _ = sender.send(StreamMessage::Done {
                result: "⚠ 프로세스가 종료되었습니다.".to_string(),
                session_id: None,
            });
            remove_process_session(session_name);
        }
    }

    Ok(())
}
