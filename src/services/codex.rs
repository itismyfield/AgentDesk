use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::mpsc::Sender;

use crate::services::claude::{
    self, CancelToken, ReadOutputResult, SessionProbe, StreamLineState, StreamMessage,
    process_stream_line, read_output_file_until_result, shell_escape,
};
use crate::services::discord::restart_report::{
    RESTART_REPORT_CHANNEL_ENV, RESTART_REPORT_PROVIDER_ENV,
};
use crate::services::provider::ProviderKind;
use crate::services::remote::RemoteProfile;
#[cfg(unix)]
use crate::services::tmux_diagnostics::{
    record_tmux_exit_reason, tmux_session_exists, tmux_session_has_live_pane,
};

static CODEX_PATH: OnceLock<Option<String>> = OnceLock::new();
const TMUX_PROMPT_B64_PREFIX: &str = "__AGENTDESK_B64__:";

/// Public so onboarding/health-check can use the exact same resolution contract.
pub fn resolve_codex_path() -> Option<String> {
    crate::services::platform::resolve_binary_with_login_shell("codex")
}

fn get_codex_path() -> Option<&'static str> {
    CODEX_PATH.get_or_init(resolve_codex_path).as_deref()
}

#[cfg(unix)]
use crate::services::tmux_common::{tmux_exact_target, tmux_owner_path, write_tmux_owner_marker};

pub fn execute_command_simple(prompt: &str) -> Result<String, String> {
    let codex_bin = get_codex_path().ok_or_else(|| "Codex CLI not found".to_string())?;
    let args = base_exec_args(None, prompt);
    let working_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));

    let output = Command::new(codex_bin)
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
                working_dir,
                sender,
                cancel_token,
                tmux_name,
                report_channel_id,
                report_provider,
            );
        }
        // ProcessBackend fallback for Codex (no tmux or non-unix)
        return execute_streaming_local_process_codex(
            &prompt,
            working_dir,
            sender,
            cancel_token,
            tmux_name,
        );
    }

    execute_streaming_direct(
        &prompt,
        session_id,
        working_dir,
        sender,
        cancel_token,
        report_channel_id,
        report_provider,
    )
}

fn compose_codex_prompt(
    prompt: &str,
    system_prompt: Option<&str>,
    allowed_tools: Option<&[String]>,
) -> String {
    let mut sections = Vec::new();

    if let Some(system_prompt) = system_prompt
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        sections.push(format!(
            "[Authoritative Instructions]\n{}\n\nThese instructions are authoritative for this turn. \
Follow them over any generic assistant persona unless the user explicitly asks to inspect or compare them.",
            system_prompt
        ));
    }

    if let Some(allowed_tools) = allowed_tools.filter(|tools| !tools.is_empty()) {
        sections.push(format!(
            "[Tool Policy]\nIf tools are needed, stay within this allowlist unless the user explicitly asks to change it: {}",
            allowed_tools.join(", ")
        ));
    }

    if sections.is_empty() {
        return prompt.to_string();
    }

    sections.push(format!("[User Request]\n{}", prompt));
    sections.join("\n\n")
}

fn execute_streaming_direct(
    prompt: &str,
    session_id: Option<&str>,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
) -> Result<(), String> {
    let codex_bin = get_codex_path().ok_or_else(|| "Codex CLI not found".to_string())?;
    let args = base_exec_args(session_id, prompt);

    let mut command = Command::new(codex_bin);
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

    if let Some(ref token) = cancel_token {
        *token.child_pid.lock().unwrap() = Some(child.id());
        // Race condition fix: if /stop arrived before PID was stored, kill now
        if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            claude::kill_child_tree(&mut child);
            let _ = child.wait();
            return Ok(());
        }
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
        if let Some(ref token) = cancel_token {
            if token.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
                claude::kill_child_tree(&mut child);
                return Ok(());
            }
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
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    tmux_session_name: &str,
    report_channel_id: Option<u64>,
    report_provider: Option<ProviderKind>,
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
        return send_followup_to_tmux(
            prompt,
            &output_path,
            &input_fifo_path,
            sender,
            cancel_token,
            tmux_session_name,
        );
    }

    if session_exists {
        record_tmux_exit_reason(
            tmux_session_name,
            "stale local session cleanup before recreate",
        );
        let exact_target = tmux_exact_target(tmux_session_name);
        let _ = Command::new("tmux")
            .args(["kill-session", "-t", &exact_target])
            .status();
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
    let codex_bin = get_codex_path().ok_or_else(|| "Codex CLI not found".to_string())?;

    // Write launch script to file to avoid tmux "command too long" errors
    let script_path = crate::services::tmux_common::session_temp_path(tmux_session_name, "sh");

    let mut env_lines = String::from("unset CLAUDECODE\n");
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
        exec {exe} codex-tmux-wrapper \\\n  \
        --output-file {output} \\\n  \
        --input-fifo {input_fifo} \\\n  \
        --prompt-file {prompt} \\\n  \
        --cwd {wd} \\\n  \
        --codex-bin {codex_bin}\n",
        env = env_lines,
        exe = shell_escape(&exe.display().to_string()),
        output = shell_escape(&output_path),
        input_fifo = shell_escape(&input_fifo_path),
        prompt = shell_escape(&prompt_path),
        wd = shell_escape(working_dir),
        codex_bin = shell_escape(codex_bin),
    );

    std::fs::write(&script_path, &script_content)
        .map_err(|e| format!("Failed to write launch script: {}", e))?;

    let tmux_result = Command::new("tmux")
        .args([
            "new-session",
            "-d",
            "-s",
            tmux_session_name,
            "-c",
            working_dir,
            &format!("bash {}", shell_escape(&script_path)),
        ])
        .env_remove("CLAUDECODE")
        .output()
        .map_err(|e| format!("Failed to create tmux session: {}", e))?;

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
    let exact_target = tmux_exact_target(tmux_session_name);
    let _ = Command::new("tmux")
        .args([
            "set-option",
            "-t",
            &exact_target,
            "remain-on-exit",
            "on",
        ])
        .output();

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

    match read_result {
        ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path,
                input_fifo_path,
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
        }
        ReadOutputResult::SessionDied { .. } => {
            let _ = sender.send(StreamMessage::Done {
                result: "⚠ 세션이 종료되었습니다. 새 메시지를 보내면 새 세션이 시작됩니다."
                    .to_string(),
                session_id: None,
            });
        }
    }

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
) -> Result<(), String> {
    let start_offset = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    let mut fifo = std::fs::OpenOptions::new()
        .write(true)
        .open(input_fifo_path)
        .map_err(|e| format!("Failed to open input FIFO: {}", e))?;
    let encoded = format!(
        "{}{}",
        TMUX_PROMPT_B64_PREFIX,
        BASE64_STANDARD.encode(prompt.as_bytes())
    );
    writeln!(fifo, "{}", encoded).map_err(|e| format!("Failed to write to input FIFO: {}", e))?;
    fifo.flush()
        .map_err(|e| format!("Failed to flush input FIFO: {}", e))?;
    drop(fifo);

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

    match read_result {
        ReadOutputResult::Completed { offset } | ReadOutputResult::Cancelled { offset } => {
            let _ = sender.send(StreamMessage::TmuxReady {
                output_path: output_path.to_string(),
                input_fifo_path: input_fifo_path.to_string(),
                tmux_session_name: tmux_session_name.to_string(),
                last_offset: offset,
            });
        }
        ReadOutputResult::SessionDied { .. } => {
            let _ = sender.send(StreamMessage::Done {
                result: "⚠ 세션이 종료되었습니다. 새 메시지를 보내면 새 세션이 시작됩니다."
                    .to_string(),
                session_id: None,
            });
        }
    }

    Ok(())
}

/// Execute Codex via ProcessBackend (direct child process, no tmux).
fn execute_streaming_local_process_codex(
    prompt: &str,
    working_dir: &str,
    sender: Sender<StreamMessage>,
    cancel_token: Option<std::sync::Arc<CancelToken>>,
    session_name: &str,
) -> Result<(), String> {
    use crate::services::session_backend::{
        ProcessBackend, SessionBackend, SessionConfig, SessionHandle,
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

    // Check for existing process session
    {
        let handles = claude::PROCESS_HANDLES.lock().unwrap();
        if let Some(handle) = handles.get(session_name) {
            let backend = ProcessBackend::new();
            if backend.is_alive(handle) {
                drop(handles);
                // Snapshot file length BEFORE sending input to avoid race:
                // Codex wrapper appends JSONL immediately on stdin, so a fast
                // response could be written before we read the offset.
                let start_offset = std::fs::metadata(&output_path)
                    .map(|m| m.len())
                    .unwrap_or(0);

                // For codex follow-up, encode prompt in the expected format
                let encoded = format!(
                    "{}{}",
                    TMUX_PROMPT_B64_PREFIX,
                    BASE64_STANDARD.encode(prompt.as_bytes())
                );
                let handles2 = claude::PROCESS_HANDLES.lock().unwrap();
                if let Some(handle) = handles2.get(session_name) {
                    backend.send_input(handle, &encoded)?;
                }
                drop(handles2);
                let read_result = claude::read_output_file_until_result(
                    &output_path,
                    start_offset,
                    sender.clone(),
                    cancel_token,
                    claude::SessionProbe::process(session_name.to_string()),
                )?;

                match read_result {
                    ReadOutputResult::Completed { offset }
                    | ReadOutputResult::Cancelled { offset } => {
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
                        claude::PROCESS_HANDLES.lock().unwrap().remove(session_name);
                    }
                }
                return Ok(());
            }
        }
    }

    // Clean up and create new session
    let _ = std::fs::remove_file(&output_path);
    let _ = std::fs::remove_file(&prompt_path);
    std::fs::write(&prompt_path, prompt)
        .map_err(|e| format!("Failed to write prompt file: {}", e))?;

    let codex_bin = get_codex_path().ok_or_else(|| "Codex CLI not found".to_string())?;
    let exe =
        std::env::current_exe().map_err(|e| format!("Failed to get executable path: {}", e))?;

    let config = SessionConfig {
        session_name: session_name.to_string(),
        working_dir: working_dir.to_string(),
        agentdesk_exe: exe.display().to_string(),
        output_path: output_path.clone(),
        prompt_path: prompt_path.clone(),
        wrapper_args: vec!["--codex-bin".to_string(), codex_bin.to_string()],
        is_codex: true,
        env_vars: vec![],
    };

    let backend = ProcessBackend::new();
    let handle = backend.create_session(&config)?;

    let SessionHandle::Process { pid, .. } = &handle;
    if let Some(ref token) = cancel_token {
        *token.child_pid.lock().unwrap() = Some(*pid);
    }

    claude::PROCESS_HANDLES
        .lock()
        .unwrap()
        .insert(session_name.to_string(), handle);

    let read_result = claude::read_output_file_until_result(
        &output_path,
        0,
        sender.clone(),
        cancel_token,
        claude::SessionProbe::process(session_name.to_string()),
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
            claude::PROCESS_HANDLES.lock().unwrap().remove(session_name);
        }
    }

    Ok(())
}

fn base_exec_args(session_id: Option<&str>, prompt: &str) -> Vec<String> {
    let mut args = vec!["exec".to_string()];
    if let Some(existing_thread_id) = session_id {
        args.push("resume".to_string());
        args.push(existing_thread_id.to_string());
    }
    args.extend([
        "--skip-git-repo-check".to_string(),
        "--json".to_string(),
        "--dangerously-bypass-approvals-and-sandbox".to_string(),
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

    use super::{TMUX_PROMPT_B64_PREFIX, compose_codex_prompt, handle_codex_json_line};
    use crate::services::claude::StreamMessage;
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};

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
}
