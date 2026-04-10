use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;

use crate::services::agent_protocol::StreamMessage;
use crate::services::claude;
use crate::services::discord::restart_report::{
    RESTART_REPORT_CHANNEL_ENV, RESTART_REPORT_PROVIDER_ENV,
};
use crate::services::process::{kill_child_tree, shell_escape};
use crate::services::provider::{
    CancelToken, FollowupResult, ProviderKind, SessionProbe, cancel_requested,
    fold_read_output_result, register_child_pid,
};
use crate::services::remote::RemoteProfile;
use crate::services::session_backend::{
    insert_process_session, process_session_is_alive, process_session_probe,
    read_output_file_until_result, remove_process_session, send_process_session_input,
};
#[cfg(unix)]
use crate::services::tmux_diagnostics::{
    record_tmux_exit_reason, should_recreate_session_after_followup_fifo_error,
    tmux_session_exists, tmux_session_has_live_pane,
};

const TMUX_PROMPT_B64_PREFIX: &str = "__AGENTDESK_B64__:";

/// Public so onboarding/health-check can use the exact same resolution contract.
#[allow(dead_code)]
pub fn resolve_codex_path() -> Option<String> {
    crate::services::platform::resolve_provider_binary("codex").resolved_path
}

fn resolve_codex_binary() -> crate::services::platform::BinaryResolution {
    crate::services::platform::resolve_provider_binary("codex")
}

fn build_tmux_launch_env_lines(
    exec_path: Option<&str>,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
) -> String {
    let mut env_lines = String::from("unset CLAUDECODE\n");
    if let Some(exec_path) = exec_path {
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

    env_lines
}

#[cfg(unix)]
use crate::services::tmux_common::{tmux_owner_path, write_tmux_owner_marker};

pub fn execute_command_simple(prompt: &str) -> Result<String, String> {
    let resolution = resolve_codex_binary();
    let codex_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Codex CLI not found".to_string())?;
    let args = base_exec_args(None, prompt, None);
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let mut command = Command::new(&codex_bin);
    crate::services::platform::apply_binary_resolution(&mut command, &resolution);
    let output = command
        .args(&args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("Failed to start Codex: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("Codex exited with code {:?}", output.status.code())
        } else {
            stderr
        });
    }

    let mut final_text = String::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        if line.trim().is_empty() {
            continue;
        }
        let Ok(json) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if json.get("type").and_then(|v| v.as_str()) != Some("item.completed") {
            continue;
        }
        let Some(item) = json.get("item") else {
            continue;
        };
        if item.get("type").and_then(|v| v.as_str()) != Some("agent_message") {
            continue;
        }
        let text = item
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if text.is_empty() {
            continue;
        }
        if !final_text.is_empty() {
            final_text.push_str("\n\n");
        }
        final_text.push_str(text);
    }

    let final_text = final_text.trim().to_string();
    if final_text.is_empty() {
        Err("Empty response from Codex".to_string())
    } else {
        Ok(final_text)
    }
}

pub fn execute_command_streaming(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    remote_profile: Option<&RemoteProfile>,
    tmux_session_name: Option<&str>,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    model: Option<&str>,
    compact_token_limit: Option<u64>,
) -> Result<(), String> {
    let prompt = compose_codex_prompt(prompt, system_prompt, allowed_tools);

    if let Some(profile) = remote_profile {
        #[cfg(unix)]
        {
            let use_remote_tmux = tmux_session_name.is_some()
                && std::env::var("AGENTDESK_CODEX_REMOTE_TMUX")
                    .map(|value| {
                        let normalized = value.trim().to_ascii_lowercase();
                        normalized == "1" || normalized == "true" || normalized == "yes"
                    })
                    .unwrap_or(false);
            if use_remote_tmux {
                let tmux_name = tmux_session_name.expect("checked is_some above");
                return execute_streaming_remote_tmux(
                    profile,
                    &prompt,
                    model,
                    working_dir,
                    sender,
                    cancel_token,
                    tmux_name,
                    report_channel_id,
                    report_provider,
                );
            }
        }
        return execute_streaming_remote_direct(
            profile,
            session_id,
            &prompt,
            model,
            working_dir,
            sender,
            cancel_token,
        );
    }

    if let Some(tmux_name) = tmux_session_name {
        #[cfg(unix)]
        if claude::is_tmux_available() {
            return execute_streaming_local_tmux(
                &prompt,
                model,
                session_id,
                working_dir,
                sender,
                cancel_token,
                tmux_name,
                report_channel_id,
                report_provider,
                compact_token_limit,
            );
        }
        // ProcessBackend fallback for Codex (no tmux or non-unix)
        return execute_streaming_local_process_codex(
            &prompt,
            model,
            session_id,
            working_dir,
            sender,
            cancel_token,
            tmux_name,
            compact_token_limit,
        );
    }

    execute_streaming_direct(
        &prompt,
        session_id,
        model,
        working_dir,
        sender,
        cancel_token,
        report_channel_id,
        report_provider,
        compact_token_limit,
    )
}

fn compose_codex_prompt(
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
) -> String {
    crate::services::provider::compose_structured_turn_prompt(prompt, system_prompt, allowed_tools)
}

fn execute_streaming_direct(
    prompt: &str,
    session_id: Option<&str>,
    model: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    compact_token_limit: Option<u64>,
) -> Result<(), String> {
    let resolution = resolve_codex_binary();
    let codex_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Codex CLI not found".to_string())?;
    let mut args = base_exec_args(session_id, prompt, model);
    if let Some(limit) = compact_token_limit.filter(|&l| l > 0) {
        // Insert -c config before the "exec" subcommand.
        let exec_pos = args.iter().position(|a| a == "exec").unwrap_or(0);
        args.insert(
            exec_pos,
            format!("model_auto_compact_token_limit={}", limit),
        );
        args.insert(exec_pos, "-c".to_string());
    }

    let mut command = Command::new(&codex_bin);
    crate::services::platform::apply_binary_resolution(&mut command, &resolution);
    command
        .args(&args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(channel_id) = report_channel_id {
        command.env(RESTART_REPORT_CHANNEL_ENV, channel_id.to_string());
    }
    if let Some(provider) = report_provider {
        command.env(RESTART_REPORT_PROVIDER_ENV, provider.as_str());
    }

    let mut child = command
        .spawn()
        .map_err(|e| format!("Failed to start Codex: {}", e))?;

    register_child_pid(cancel_token.as_deref(), child.id());
    // Race condition fix: if /stop arrived before PID was stored, kill now
    if cancel_requested(cancel_token.as_deref()) {
        kill_child_tree(&mut child);
        let _ = child.wait();
        return Ok(());
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture Codex stdout".to_string())?;
    let reader = BufReader::new(stdout);

    let mut current_thread_id = session_id.map(str::to_string);
    let mut final_text = String::new();
    let mut saw_done = false;
    let started_at = std::time::Instant::now();

    for line in reader.lines() {
        if cancel_requested(cancel_token.as_deref()) {
            kill_child_tree(&mut child);
            return Ok(());
        }

        let line = match line {
            Ok(line) => line,
            Err(e) => return Err(format!("Failed to read Codex output: {}", e)),
        };

        if let Some(done) = handle_codex_json_line(
            &line,
            &sender,
            &mut current_thread_id,
            &mut final_text,
            started_at,
        )? {
            saw_done = saw_done || done;
        }
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for Codex: {}", e))?;

    if !output.status.success() && !saw_done {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("Codex exited with code {:?}", output.status.code())
        } else {
            stderr
        };
        let _ = sender.send(StreamMessage::Error {
            message,
            stdout: String::new(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code(),
        });
        return Ok(());
    }

    if !saw_done {
        let _ = sender.send(StreamMessage::Done {
            result: final_text,
            session_id: current_thread_id,
        });
    }

    Ok(())
}

fn execute_streaming_remote_direct(
    _profile: &RemoteProfile,
    _session_id: Option<&str>,
    _prompt: &str,
    _model: Option<&str>,
    _working_dir: &str,
    _sender: Sender<StreamMessage>,
    _cancel_token: Option<std::sync::Arc<CancelToken>>,
) -> Result<(), String> {
    Err("Remote SSH execution is not available in AgentDesk".to_string())
}

#[cfg(unix)]
fn execute_streaming_remote_tmux(
    _profile: &RemoteProfile,
    _prompt: &str,
    _model: Option<&str>,
    _working_dir: &str,
    _sender: Sender<StreamMessage>,
    _cancel_token: Option<std::sync::Arc<CancelToken>>,
    _tmux_session_name: &str,
    _report_channel_id: Option<u64>,
    _report_provider: Option<ProviderKind>,
) -> Result<(), String> {
    Err("Remote SSH tmux execution is not available in AgentDesk".to_string())
}

#[cfg(unix)]
fn execute_streaming_local_tmux(
    prompt: &str,
    model: Option<&str>,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
    compact_token_limit: Option<u64>,
) -> Result<(), String> {
    let output_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    let input_fifo_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "input");
    let prompt_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "prompt");
    let owner_path = tmux_owner_path(tmux_session_name);

    let session_exists = tmux_session_exists(tmux_session_name);
    let session_usable = tmux_session_has_live_pane(tmux_session_name)
        && std::fs::metadata(&output_path).is_ok()
        && std::path::Path::new(&input_fifo_path).exists();

    if session_usable {
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
                crate::services::platform::tmux::kill_session(tmux_session_name);
                // Fall through to new session creation below
            }
        }
    } else if session_exists {
        record_tmux_exit_reason(
            tmux_session_name,
            "stale local session cleanup before recreate",
        );
        crate::services::platform::tmux::kill_session(tmux_session_name);
    }

    let _ = std::fs::remove_file(&output_path);
    let _ = std::fs::remove_file(&input_fifo_path);
    let _ = std::fs::remove_file(&prompt_path);
    let _ = std::fs::remove_file(&owner_path);
    let _ = std::fs::remove_file(crate::services::tmux_common::session_temp_path(
        tmux_session_name,
        "sh",
    ));

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
    let resolution = resolve_codex_binary();
    let codex_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Codex CLI not found".to_string())?;

    // Write launch script to file to avoid tmux "command too long" errors
    let script_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "sh");

    let env_lines = build_tmux_launch_env_lines(
        resolution.exec_path.as_deref(),
        report_channel_id,
        report_provider,
    );

    let script_content = format!(
        "#!/bin/bash\n\
        {env}\
        exec {exe} codex-tmux-wrapper \\\n  \
        --output-file {output} \\\n  \
        --input-fifo {input_fifo} \\\n  \
        --prompt-file {prompt} \\\n  \
        --cwd {wd} \\\n  \
        --codex-bin {codex_bin}{model_arg}{effort_arg}{resume_arg}{compact_arg}\n",
        env = env_lines,
        exe = shell_escape(&exe.display().to_string()),
        output = shell_escape(&output_path),
        input_fifo = shell_escape(&input_fifo_path),
        prompt = shell_escape(&prompt_path),
        wd = shell_escape(working_dir),
        codex_bin = shell_escape(&codex_bin),
        model_arg = model
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!(" \\\n  --codex-model {}", shell_escape(value)))
            .unwrap_or_default(),
        compact_arg = compact_token_limit
            .filter(|&l| l > 0)
            .map(|l| format!(" \\\n  --compact-token-limit {}", l))
            .unwrap_or_default(),
        effort_arg = std::env::var("AGENTDESK_CODEX_REASONING_EFFORT")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map(|v| format!(" \\\n  --reasoning-effort {}", shell_escape(&v)))
            .unwrap_or_default(),
        resume_arg = session_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| format!(" \\\n  --resume-session-id {}", shell_escape(value)))
            .unwrap_or_default(),
    );

    std::fs::write(&script_path, &script_content)
        .map_err(|e| format!("Failed to write launch script: {}", e))?;

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
        return Err(format!("tmux error: {}", stderr));
    }

    // Keep tmux session alive after process exits for post-mortem analysis
    crate::services::platform::tmux::set_option(tmux_session_name, "remain-on-exit", "on");

    // Stamp generation marker so post-restart watcher restore can detect old sessions
    let gen_marker_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "generation");
    let current_gen = crate::services::discord::runtime_store::load_generation();
    let _ = std::fs::write(&gen_marker_path, current_gen.to_string());

    if let Some(ref token) = cancel_token {
        *token.tmux_session.lock().unwrap() = Some(tmux_session_name.to_string());
    }

    let read_result = read_output_file_until_result(
        &output_path,
        0,
        sender.clone(),
        cancel_token.clone(),
        SessionProbe::tmux(tmux_session_name.to_string()),
    )?;

    fold_read_output_result(
        read_result,
        |offset| {
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path,
                input_fifo_path,
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
        },
        |_| {
            let _ = sender.send(StreamMessage::Done {
                result: "⚠ 세션이 종료되었습니다. 새 메시지를 보내면 새 세션이 시작됩니다."
                    .to_string(),
                session_id: None,
            });
        },
    );

    Ok(())
}

#[cfg(unix)]
fn send_followup_to_tmux(
    prompt: &str,
    output_path: &str,
    input_fifo_path: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
) -> Result<FollowupResult, String> {
    let start_offset = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    // Write to input FIFO — if the pipe is broken or missing, request recreation
    let write_result = std::fs::OpenOptions::new()
        .write(true)
        .open(input_fifo_path)
        .map_err(|e| format!("Failed to open input FIFO: {}", e))
        .and_then(|mut fifo| {
            let encoded = format!(
                "{}{}",
                TMUX_PROMPT_B64_PREFIX,
                BASE64_STANDARD.encode(prompt.as_bytes())
            );
            writeln!(fifo, "{}", encoded)
                .map_err(|e| format!("Failed to write to input FIFO: {}", e))?;
            fifo.flush()
                .map_err(|e| format!("Failed to flush input FIFO: {}", e))?;
            Ok(())
        });

    if let Err(e) = write_result {
        if should_recreate_session_after_followup_fifo_error(&e) {
            return Ok(FollowupResult::RecreateSession { error: e });
        }
        return Err(e);
    }

    if let Some(ref token) = cancel_token {
        *token.tmux_session.lock().unwrap() = Some(tmux_session_name.to_string());
    }

    let read_result = read_output_file_until_result(
        output_path,
        start_offset,
        sender.clone(),
        cancel_token,
        SessionProbe::tmux(tmux_session_name.to_string()),
    )?;

    Ok(fold_read_output_result(
        read_result,
        |offset| {
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path: output_path.to_string(),
                input_fifo_path: input_fifo_path.to_string(),
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
            FollowupResult::Delivered
        },
        |_| FollowupResult::RecreateSession {
            error: "session died during follow-up output reading".to_string(),
        },
    ))
}

/// Execute Codex via ProcessBackend (direct child process, no tmux).
fn execute_streaming_local_process_codex(
    prompt: &str,
    model: Option<&str>,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    session_name: &str,
    compact_token_limit: Option<u64>,
) -> Result<(), String> {
    use crate::services::session_backend::{ProcessBackend, SessionBackend, SessionConfig};

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

    // Check for existing process session
    if process_session_is_alive(session_name) {
        // Snapshot file length BEFORE sending input to avoid race:
        // Codex wrapper appends JSONL immediately on stdin, so a fast
        // response could be written before we read the offset.
        let start_offset = std::fs::metadata(&output_path)
            .map(|m| m.len())
            .unwrap_or(0);

        let encoded = format!(
            "{}{}",
            TMUX_PROMPT_B64_PREFIX,
            BASE64_STANDARD.encode(prompt.as_bytes())
        );
        send_process_session_input(session_name, &encoded)?;
        let read_result = read_output_file_until_result(
            &output_path,
            start_offset,
            sender.clone(),
            cancel_token,
            process_session_probe(session_name),
        )?;

        fold_read_output_result(
            read_result,
            |offset| {
                let _ = sender.send(StreamMessage::ProcessReady {
                    output_path: output_path.to_string(),
                    session_name: session_name.to_string(),
                    last_offset: offset,
                });
            },
            |_| {
                let _ = sender.send(StreamMessage::Done {
                    result: "⚠ 세션이 종료되었습니다.".to_string(),
                    session_id: None,
                });
                remove_process_session(session_name);
            },
        );
        return Ok(());
    }

    // Clean up and create new session
    let _ = std::fs::remove_file(&output_path);
    let _ = std::fs::remove_file(&prompt_path);
    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("Failed to write prompt file: {}", e))?;

    let resolution = resolve_codex_binary();
    let codex_bin = resolution
        .resolved_path
        .clone()
        .ok_or_else(|| "Codex CLI not found".to_string())?;
    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;

    let config = SessionConfig {
        session_name: session_name.to_string(),
        working_dir: working_dir.to_string(),
        agentdesk_exe: exe.display().to_string(),
        output_path: output_path.clone(),
        prompt_path: prompt_path.clone(),
        wrapper_subcommand: "codex-tmux-wrapper".to_string(),
        wrapper_args: {
            let mut args = vec!["--codex-bin".to_string(), codex_bin.to_string()];
            if let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) {
                args.push("--codex-model".to_string());
                args.push(model.to_string());
            }
            if let Ok(effort) = std::env::var("AGENTDESK_CODEX_REASONING_EFFORT") {
                if !effort.trim().is_empty() {
                    args.push("--reasoning-effort".to_string());
                    args.push(effort);
                }
            }
            if let Some(session_id) = session_id.map(str::trim).filter(|value| !value.is_empty()) {
                args.push("--resume-session-id".to_string());
                args.push(session_id.to_string());
            }
            if let Some(limit) = compact_token_limit.filter(|&l| l > 0) {
                args.push("--compact-token-limit".to_string());
                args.push(limit.to_string());
            }
            args
        },
        env_vars: resolution
            .exec_path
            .clone()
            .map(|path| vec![("PATH".to_string(), path)])
            .unwrap_or_default(),
    };

    let backend = ProcessBackend::new();
    let handle = backend.create_session(&config)?;

    if let Some(ref token) = cancel_token {
        *token.child_pid.lock().unwrap() = Some(handle.pid());
    }

    insert_process_session(session_name.to_string(), handle);

    let read_result = read_output_file_until_result(
        &output_path,
        0,
        sender.clone(),
        cancel_token,
        process_session_probe(session_name),
    )?;

    fold_read_output_result(
        read_result,
        |offset| {
            let _ = sender.send(StreamMessage::ProcessReady {
                output_path,
                session_name: session_name.to_string(),
                last_offset: offset,
            });
        },
        |_| {
            let _ = sender.send(StreamMessage::Done {
                result: "⚠ 프로세스가 종료되었습니다.".to_string(),
                session_id: None,
            });
            remove_process_session(session_name);
        },
    );

    Ok(())
}

fn base_exec_args(session_id: Option<&str>, prompt: &str, model: Option<&str>) -> Vec<String> {
    let mut args = Vec::new();
    if let Some(model) = model.map(str::trim).filter(|value| !value.is_empty()) {
        args.push("-c".to_string());
        args.push(r#"model_reasoning_effort="high""#.to_string());
        args.push("-m".to_string());
        args.push(model.to_string());
    }
    args.push("exec".to_string());
    if let Some(existing_thread_id) = session_id {
        args.push("resume".to_string());
        args.push(existing_thread_id.to_string());
    }
    args.extend([
        "--skip-git-repo-check".to_string(),
        "--json".to_string(),
        "--dangerously-bypass-approvals-and-sandbox".to_string(),
        "--".to_string(),
        prompt.to_string(),
    ]);
    args
}

fn handle_codex_json_line(
    line: &str,
    sender: &Sender<StreamMessage>,
    current_thread_id: &mut Option<String>,
    final_text: &mut String,
    started_at: std::time::Instant,
) -> Result<Option<bool>, String> {
    if line.trim().is_empty() {
        return Ok(None);
    }

    let json = serde_json::from_str::<Value>(line)
        .map_err(|e| format!("Failed to parse Codex JSON: {}", e))?;

    match json.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "thread.started" => {
            if let Some(thread_id) = json.get("thread_id").and_then(|v| v.as_str()) {
                *current_thread_id = Some(thread_id.to_string());
                let _ = sender.send(StreamMessage::Init {
                    session_id: thread_id.to_string(),
                });
            }
        }
        "item.started" => {
            if let Some(item) = json.get("item") {
                match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "command_execution" => {
                        let command = item.get("command").and_then(|v| v.as_str()).unwrap_or("");
                        let input = serde_json::json!({ "command": command }).to_string();
                        let _ = sender.send(StreamMessage::ToolUse {
                            name: "Bash".to_string(),
                            input,
                        });
                    }
                    "reasoning" => {
                        // Codex reasoning: extract summary text if available
                        let summary = item
                            .get("summary")
                            .and_then(|v| v.as_array())
                            .and_then(|arr| arr.first())
                            .and_then(|s| s.get("text"))
                            .and_then(|v| v.as_str())
                            .and_then(|t| t.lines().find(|l| !l.trim().is_empty()))
                            .map(|l| l.trim().to_string());
                        let _ = sender.send(StreamMessage::Thinking { summary });
                    }
                    _ => {}
                }
            }
        }
        "item.completed" => {
            if let Some(item) = json.get("item") {
                match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "agent_message" => {
                        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
                        if !text.is_empty() {
                            if !final_text.is_empty() {
                                final_text.push_str("\n\n");
                            }
                            final_text.push_str(text);
                            let _ = sender.send(StreamMessage::Text {
                                content: text.to_string(),
                            });
                        }
                    }
                    "command_execution" => {
                        let content = item
                            .get("aggregated_output")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let is_error = item
                            .get("exit_code")
                            .and_then(|v| v.as_i64())
                            .map(|code| code != 0)
                            .unwrap_or(false);
                        let _ = sender.send(StreamMessage::ToolResult { content, is_error });
                    }
                    "reasoning" => {
                        let summary = item
                            .get("summary")
                            .and_then(|v| v.as_array())
                            .and_then(|arr| arr.first())
                            .and_then(|s| s.get("text"))
                            .and_then(|v| v.as_str())
                            .and_then(|t| t.lines().find(|l| !l.trim().is_empty()))
                            .map(|l| l.trim().to_string());
                        let _ = sender.send(StreamMessage::Thinking { summary });
                    }
                    _ => {}
                }
            }
        }
        "turn.completed" => {
            let usage = json.get("usage").cloned().unwrap_or_default();
            let input_tokens = usage.get("input_tokens").and_then(|v| v.as_u64());
            let output_tokens = usage.get("output_tokens").and_then(|v| v.as_u64());
            let _ = sender.send(StreamMessage::StatusUpdate {
                model: Some("codex".to_string()),
                cost_usd: None,
                total_cost_usd: None,
                duration_ms: Some(started_at.elapsed().as_millis() as u64),
                num_turns: None,
                input_tokens,
                output_tokens,
            });
            let _ = sender.send(StreamMessage::Done {
                result: final_text.clone(),
                session_id: current_thread_id.clone(),
            });
            return Ok(Some(true));
        }
        "error" => {
            let message = json
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Codex error");
            let _ = sender.send(StreamMessage::Error {
                message: message.to_string(),
                stdout: String::new(),
                stderr: String::new(),
                exit_code: None,
            });
            return Ok(Some(true));
        }
        _ => {}
    }

    Ok(Some(false))
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::{
        TMUX_PROMPT_B64_PREFIX, base_exec_args, build_tmux_launch_env_lines, compose_codex_prompt,
        handle_codex_json_line,
    };
    use crate::services::agent_protocol::StreamMessage;
    use crate::services::discord::restart_report::{
        RESTART_REPORT_CHANNEL_ENV, RESTART_REPORT_PROVIDER_ENV,
    };
    use crate::services::provider::ProviderKind;
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};

    #[test]
    fn test_tmux_launch_env_lines_include_exec_path_and_report_envs() {
        let env_lines = build_tmux_launch_env_lines(
            Some("/tmp/provider:/usr/bin"),
            Some(42),
            Some(ProviderKind::Codex),
        );

        assert!(env_lines.contains("unset CLAUDECODE"));
        assert!(env_lines.contains("export PATH='/tmp/provider:/usr/bin'"));
        assert!(env_lines.contains(&format!("export {}=42", RESTART_REPORT_CHANNEL_ENV)));
        assert!(env_lines.contains(&format!("export {}=codex", RESTART_REPORT_PROVIDER_ENV)));
    }

    #[test]
    fn test_handle_codex_json_line_maps_thread_and_turn_completion() {
        let (tx, rx) = mpsc::channel();
        let mut thread_id = None;
        let mut final_text = String::new();
        let started_at = std::time::Instant::now();

        let _ = handle_codex_json_line(
            r#"{"type":"thread.started","thread_id":"thread-1"}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();
        let _ = handle_codex_json_line(
            r#"{"type":"item.completed","item":{"type":"agent_message","text":"hello"}} "#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();
        let done = handle_codex_json_line(
            r#"{"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":3}}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();

        assert_eq!(thread_id.as_deref(), Some("thread-1"));
        assert_eq!(done, Some(true));

        let items: Vec<StreamMessage> = rx.try_iter().collect();
        assert!(matches!(items[0], StreamMessage::Init { .. }));
        assert!(matches!(items[1], StreamMessage::Text { .. }));
        assert!(matches!(items[2], StreamMessage::StatusUpdate { .. }));
        assert!(matches!(items[3], StreamMessage::Done { .. }));
    }

    #[test]
    fn test_compose_codex_prompt_includes_authoritative_sections() {
        let prompt = compose_codex_prompt(
            "role과 mission만 답해줘.",
            Some("role: PMD\nmission: 백로그 관리"),
            Some(&["Bash".to_string(), "Read".to_string()]),
        );

        assert!(prompt.contains("[Authoritative Instructions]"));
        assert!(prompt.contains("role: PMD"));
        assert!(prompt.contains("[Tool Policy]"));
        assert!(prompt.contains("Bash, Read"));
        assert!(prompt.contains("[User Request]\nrole과 mission만 답해줘."));
    }

    #[test]
    fn test_compose_codex_prompt_returns_plain_prompt_without_overrides() {
        let prompt = compose_codex_prompt("just answer", None, None);
        assert_eq!(prompt, "just answer");
    }

    #[test]
    fn test_codex_reasoning_started_sends_thinking() {
        let (tx, rx) = mpsc::channel();
        let mut thread_id = None;
        let mut final_text = String::new();
        let started_at = std::time::Instant::now();

        let _ = handle_codex_json_line(
            r#"{"type":"item.started","item":{"type":"reasoning","id":"rs_001","summary":[]}}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();

        let items: Vec<StreamMessage> = rx.try_iter().collect();
        assert_eq!(items.len(), 1);
        assert!(matches!(
            items[0],
            StreamMessage::Thinking { summary: None }
        ));
    }

    #[test]
    fn test_codex_reasoning_completed_sends_thinking_with_summary() {
        let (tx, rx) = mpsc::channel();
        let mut thread_id = None;
        let mut final_text = String::new();
        let started_at = std::time::Instant::now();

        let _ = handle_codex_json_line(
            r#"{"type":"item.completed","item":{"type":"reasoning","id":"rs_001","summary":[{"type":"summary_text","text":"Analyzing the code structure"}]}}"#,
            &tx,
            &mut thread_id,
            &mut final_text,
            started_at,
        )
        .unwrap();

        let items: Vec<StreamMessage> = rx.try_iter().collect();
        assert_eq!(items.len(), 1);
        match &items[0] {
            StreamMessage::Thinking { summary } => {
                assert_eq!(summary.as_deref(), Some("Analyzing the code structure"));
            }
            _ => panic!("Expected Thinking with summary"),
        }
    }

    #[test]
    fn test_tmux_followup_encoding_is_single_line() {
        let prompt = "line1\nline2\nline3";
        let encoded = format!(
            "{}{}",
            TMUX_PROMPT_B64_PREFIX,
            BASE64_STANDARD.encode(prompt.as_bytes())
        );

        assert!(!encoded.contains('\n'));
    }

    #[test]
    fn test_base_exec_args_includes_model_before_exec() {
        let args = base_exec_args(None, "- starts like option", Some("gpt-5-codex"));
        assert!(args.starts_with(&[
            "-c".to_string(),
            r#"model_reasoning_effort="high""#.to_string(),
            "-m".to_string(),
            "gpt-5-codex".to_string()
        ]));
        assert!(args.iter().any(|arg| arg == "exec"));
        let separator_index = args
            .iter()
            .position(|arg| arg == "--")
            .expect("prompt separator should be present");
        assert_eq!(
            args.get(separator_index + 1).map(String::as_str),
            Some("- starts like option")
        );
        assert_eq!(
            args.iter()
                .filter(|arg| arg.as_str() == "--skip-git-repo-check")
                .count(),
            1
        );
    }

    #[test]
    fn test_base_exec_args_includes_resume_before_flags() {
        let args = base_exec_args(Some("thread-123"), "hello", None);
        assert_eq!(
            args,
            vec![
                "exec".to_string(),
                "resume".to_string(),
                "thread-123".to_string(),
                "--skip-git-repo-check".to_string(),
                "--json".to_string(),
                "--dangerously-bypass-approvals-and-sandbox".to_string(),
                "--".to_string(),
                "hello".to_string(),
            ]
        );
    }

    // ========== FollowupResult tests ==========

    #[cfg(unix)]
    #[test]
    fn test_codex_followup_fifo_not_found_returns_recreate() {
        use super::send_followup_to_tmux;
        use crate::services::provider::FollowupResult;

        let (sender, _receiver) = mpsc::channel();
        let dir = std::env::temp_dir();
        let output_path = dir.join(format!(
            "agentdesk-test-codex-followup-{}.jsonl",
            std::process::id()
        ));
        let _ = std::fs::write(&output_path, "");

        let result = send_followup_to_tmux(
            "test prompt",
            output_path.to_str().unwrap(),
            "/tmp/agentdesk-test-codex-nonexistent-fifo",
            sender,
            None,
            "test-codex-followup",
        );

        let _ = std::fs::remove_file(&output_path);

        match result {
            Ok(FollowupResult::RecreateSession { error }) => {
                assert!(error.contains("Failed to open input FIFO"));
            }
            other => panic!("Expected Ok(RecreateSession), got {:?}", other),
        }
    }
}
