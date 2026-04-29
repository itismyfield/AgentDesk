use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};

use crate::services::tmux_wrapper::{InputMode, render_for_terminal};

const TMUX_PROMPT_B64_PREFIX: &str = "__AGENTDESK_B64__:";

#[derive(Debug, Default)]
struct PartialBlockState {
    kind: String,
    tool_name: Option<String>,
    input_json: String,
    thinking_emitted: bool,
}

#[derive(Debug, Default)]
struct TurnNormalizationState {
    partial_stream_seen: bool,
    meaningful_progress_seen: bool,
    current_model: Option<String>,
    last_session_id: Option<String>,
    init_emitted_for_session: Option<String>,
    partial_blocks: HashMap<usize, PartialBlockState>,
}

#[derive(Debug)]
enum TurnReadEvent {
    Line(String),
    ReadError(String),
    Eof,
}

#[derive(Debug)]
struct TurnFailure {
    message: String,
    retryable: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum TurnWatchdogOutcome {
    Continue,
    Break,
    Retry { message: String },
}

pub fn run(
    output_file: &str,
    input_fifo: &str,
    prompt_file: &str,
    working_dir: &str,
    qwen_bin: &str,
    qwen_model: Option<&str>,
    qwen_core_tools: &[String],
    resume_session_id: Option<&str>,
    input_mode: InputMode,
) {
    let mode_label = match input_mode {
        InputMode::Fifo => "tmux resume loop",
        InputMode::Pipe => "pipe-mode",
    };
    eprintln!("\x1b[90m═══════════════════════════════════════════════════════\x1b[0m");
    eprintln!("\x1b[90m  AgentDesk Qwen Session ({})\x1b[0m", mode_label);
    if input_mode == InputMode::Fifo {
        eprintln!("\x1b[90m  Type messages below when Qwen is ready.\x1b[0m");
        eprintln!("\x1b[90m  Ctrl-B, D to detach\x1b[0m");
    }
    eprintln!("\x1b[90m═══════════════════════════════════════════════════════\x1b[0m");
    eprintln!();

    let prompt = match std::fs::read_to_string(prompt_file) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("\x1b[31mError reading prompt file: {}\x1b[0m", e);
            std::process::exit(1);
        }
    };
    let _ = std::fs::remove_file(prompt_file);

    let expanded_dir = if working_dir.starts_with("~/") {
        if let Some(home) = dirs::home_dir() {
            home.join(&working_dir[2..]).to_string_lossy().to_string()
        } else {
            working_dir.to_string()
        }
    } else if working_dir == "~" {
        dirs::home_dir()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|| working_dir.to_string())
    } else {
        working_dir.to_string()
    };

    let (prompt_tx, prompt_rx) = mpsc::channel::<String>();

    if input_mode == InputMode::Fifo {
        let prompt_tx = prompt_tx.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let reader = BufReader::new(stdin.lock());
            for line in reader.lines() {
                let Ok(line) = line else {
                    break;
                };
                if line.trim().is_empty() {
                    continue;
                }
                let _ = prompt_tx.send(line);
            }
        });
    }

    {
        let prompt_tx = prompt_tx.clone();
        let input_fifo = input_fifo.to_string();
        std::thread::spawn(move || {
            let reader: BufReader<Box<dyn std::io::Read + Send>> = match input_mode {
                InputMode::Fifo => {
                    let fifo = match std::fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(&input_fifo)
                    {
                        Ok(f) => f,
                        Err(e) => {
                            eprintln!("\x1b[90m[input fifo error: {}]\x1b[0m", e);
                            return;
                        }
                    };
                    BufReader::new(Box::new(fifo))
                }
                InputMode::Pipe => BufReader::new(Box::new(std::io::stdin())),
            };

            for line in reader.lines() {
                let Ok(line) = line else {
                    break;
                };
                if line.trim().is_empty() {
                    continue;
                }
                eprintln!("\x1b[90m[external message received]\x1b[0m");
                match decode_external_prompt(&line) {
                    Ok(prompt) => {
                        if !prompt.trim().is_empty() {
                            let _ = prompt_tx.send(prompt);
                        }
                    }
                    Err(err) => eprintln!("\x1b[90m[input decode error: {}]\x1b[0m", err),
                }
            }
        });
    }

    let mut output = match std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(output_file)
    {
        Ok(file) => file,
        Err(e) => {
            eprintln!("\x1b[31mFailed to open output file: {}\x1b[0m", e);
            std::process::exit(1);
        }
    };

    let settings_override = match crate::services::qwen::create_system_settings_override(
        (!qwen_core_tools.is_empty()).then_some(qwen_core_tools),
    ) {
        Ok(override_file) => override_file,
        Err(err) => {
            emit_result_error(&mut output, &err);
            let exit_reason_path = format!("{}.exit_reason", output_file);
            let _ = std::fs::write(&exit_reason_path, format!("error:{err}"));
            eprintln!("\x1b[33m[preserving output files for post-mortem: {output_file}]\x1b[0m");
            std::process::exit(1);
        }
    };

    let mut session_id = resume_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let first_turn = run_turn(
        &mut output,
        qwen_bin,
        qwen_model,
        &expanded_dir,
        &prompt,
        &mut session_id,
        settings_override.as_ref(),
    );
    if let Err(err) = first_turn {
        emit_result_error(&mut output, &err);
        let exit_reason_path = format!("{}.exit_reason", output_file);
        let _ = std::fs::write(&exit_reason_path, format!("error:{err}"));
        eprintln!("\x1b[33m[preserving output files for post-mortem: {output_file}]\x1b[0m");
        std::process::exit(1);
    }

    let mut followup_error: Option<String> = None;
    while let Ok(next_prompt) = prompt_rx.recv() {
        if let Err(err) = run_turn(
            &mut output,
            qwen_bin,
            qwen_model,
            &expanded_dir,
            next_prompt.trim(),
            &mut session_id,
            settings_override.as_ref(),
        ) {
            emit_result_error(&mut output, &err);
            followup_error = Some(err);
            break;
        }
    }

    let exit_reason_path = format!("{}.exit_reason", output_file);
    let exit_reason = if let Some(ref err) = followup_error {
        let reason = format!("error:{err}");
        let _ = std::fs::write(&exit_reason_path, &reason);
        eprintln!("\x1b[33m[preserving output files for post-mortem: {output_file}]\x1b[0m");
        reason
    } else {
        let reason = "exit:0".to_string();
        let _ = std::fs::write(&exit_reason_path, &reason);
        cleanup(output_file, input_fifo);
        reason
    };
    eprintln!();
    eprintln!("\x1b[90m--- Session ended ({exit_reason}) ---\x1b[0m");
}

fn run_turn(
    output: &mut std::fs::File,
    qwen_bin: &str,
    qwen_model: Option<&str>,
    working_dir: &str,
    prompt: &str,
    session_id: &mut Option<String>,
    settings_override: Option<&crate::services::qwen::QwenSystemSettingsOverride>,
) -> Result<(), String> {
    let mut resume_strategy =
        crate::services::qwen::normalize_resume_strategy(session_id.as_deref(), working_dir)?;

    for attempt in 0..=crate::services::qwen::QWEN_MAX_SESSION_RETRIES {
        let output_checkpoint = output
            .metadata()
            .map_err(|err| format!("read Qwen output checkpoint: {}", err))?
            .len();
        match run_turn_once(
            output,
            qwen_bin,
            qwen_model,
            working_dir,
            prompt,
            session_id,
            settings_override,
            &resume_strategy,
        ) {
            Ok(()) => return Ok(()),
            Err(err)
                if err.retryable && attempt < crate::services::qwen::QWEN_MAX_SESSION_RETRIES =>
            {
                rewind_output_to_checkpoint(output, output_checkpoint)?;
                *session_id = None;
                resume_strategy = crate::services::qwen::QwenResumeStrategy::Fresh;
            }
            Err(err) => return Err(err.message),
        }
    }

    Ok(())
}

fn run_turn_once(
    output: &mut std::fs::File,
    qwen_bin: &str,
    qwen_model: Option<&str>,
    working_dir: &str,
    prompt: &str,
    session_id: &mut Option<String>,
    settings_override: Option<&crate::services::qwen::QwenSystemSettingsOverride>,
    resume_strategy: &crate::services::qwen::QwenResumeStrategy,
) -> Result<(), TurnFailure> {
    emit_status("[sending...]");

    let args = crate::services::qwen::build_stream_exec_args(prompt, qwen_model, resume_strategy);
    let mut command = Command::new(qwen_bin);
    crate::services::platform::augment_exec_path(&mut command, qwen_bin);
    command
        .args(&args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(settings_override) = settings_override {
        command.env(
            crate::services::qwen::QWEN_CODE_SYSTEM_SETTINGS_ENV,
            settings_override.path(),
        );
    }
    let mut child = command.spawn().map_err(|e| TurnFailure {
        message: format!("Failed to start Qwen: {}", e),
        retryable: false,
    })?;

    let child_pid = child.id();
    let stdout = child.stdout.take().ok_or_else(|| TurnFailure {
        message: "Failed to capture Qwen stdout".to_string(),
        retryable: false,
    })?;
    let stderr = child.stderr.take().ok_or_else(|| TurnFailure {
        message: "Failed to capture Qwen stderr".to_string(),
        retryable: false,
    })?;
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        let mut reader = BufReader::new(stderr);
        let _ = std::io::Read::read_to_string(&mut reader, &mut buf);
        buf
    });
    let stdout_events = spawn_turn_stream_reader(stdout);

    let mut state = TurnNormalizationState {
        last_session_id: session_id.clone(),
        init_emitted_for_session: session_id.clone(),
        ..TurnNormalizationState::default()
    };
    let mut saw_result = false;
    let mut watchdog = crate::services::qwen::QwenStreamWatchdog::default();

    loop {
        match stdout_events.recv_timeout(watchdog.poll_timeout()) {
            Ok(TurnReadEvent::Line(line)) => {
                watchdog.observe_line();
                for normalized in normalize_qwen_line(&line, &mut state) {
                    if is_meaningful_progress_event(&normalized) {
                        state.meaningful_progress_seen = true;
                    }
                    if let Some(id) = extract_init_session_id(&normalized) {
                        *session_id = Some(id);
                    }
                    if is_result_line(&normalized) {
                        saw_result = true;
                    }
                    emit_json_line(output, normalized).map_err(|err| TurnFailure {
                        message: err,
                        retryable: false,
                    })?;
                }
            }
            Ok(TurnReadEvent::ReadError(message)) => {
                crate::services::process::kill_pid_tree(child_pid);
                let _ = child.wait();
                let _ = stderr_handle.join().unwrap_or_default();
                return Err(TurnFailure {
                    message,
                    retryable: true,
                });
            }
            Ok(TurnReadEvent::Eof) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                match next_turn_watchdog_outcome(saw_result, &state, &mut watchdog) {
                    TurnWatchdogOutcome::Continue => {}
                    TurnWatchdogOutcome::Break => break,
                    TurnWatchdogOutcome::Retry { message } => {
                        crate::services::process::kill_pid_tree(child_pid);
                        let _ = child.wait();
                        let _ = stderr_handle.join().unwrap_or_default();
                        return Err(TurnFailure {
                            message,
                            retryable: true,
                        });
                    }
                }
            }
        }
    }

    crate::services::process::kill_pid_tree(child_pid);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let wait = child.wait_with_output().map_err(|e| TurnFailure {
        message: format!("Failed to wait for Qwen: {}", e),
        retryable: false,
    })?;
    let stderr = stderr_handle.join().unwrap_or_default();

    if !wait.status.success() && !saw_result {
        let message = derive_wrapper_error_message(&stderr, wait.status.code());
        return Err(TurnFailure {
            message,
            retryable: true,
        });
    }

    if !saw_result {
        let message = if stderr.trim().is_empty() {
            "Qwen stream ended without a terminal result".to_string()
        } else {
            stderr.trim().to_string()
        };
        return Err(TurnFailure {
            message,
            retryable: true,
        });
    }

    Ok(())
}

#[cfg(test)]
fn build_turn_args(
    prompt: &str,
    model: Option<&str>,
    session_id: Option<&str>,
    working_dir: &str,
) -> Result<Vec<String>, String> {
    let resume_strategy =
        crate::services::qwen::normalize_resume_strategy(session_id, working_dir)?;
    Ok(crate::services::qwen::build_stream_exec_args(
        prompt,
        model,
        &resume_strategy,
    ))
}

fn spawn_turn_stream_reader<R: std::io::Read + Send + 'static>(
    stdout: R,
) -> mpsc::Receiver<TurnReadEvent> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(TurnReadEvent::Line(line)).is_err() {
                        return;
                    }
                }
                Err(err) => {
                    let _ = tx.send(TurnReadEvent::ReadError(format!(
                        "Failed to read Qwen output: {}",
                        err
                    )));
                    return;
                }
            }
        }
        let _ = tx.send(TurnReadEvent::Eof);
    });
    rx
}

fn normalize_qwen_line(line: &str, state: &mut TurnNormalizationState) -> Vec<Value> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }

    let Ok(json) = serde_json::from_str::<Value>(trimmed) else {
        return Vec::new();
    };

    match json.get("type").and_then(|v| v.as_str()) {
        Some("system") => normalize_system_event(&json, state),
        Some("stream_event") => {
            state.partial_stream_seen = true;
            normalize_stream_event(&json, state)
        }
        Some("assistant") => normalize_assistant_message(&json, state),
        Some("user") => normalize_user_message(&json),
        Some("result") => normalize_result_message(&json, state),
        _ => Vec::new(),
    }
}

// After normalize_qwen_line expansion, LLM text/thinking/tool-use arrives as "assistant" events
// and tool results arrive as "user" events.  "system" (session bookkeeping) and "result" (terminal
// signal) represent protocol framing, not LLM progress, so they intentionally do not qualify.
// This is consistent with qwen.rs where mark_meaningful_progress is called only on content events.
fn is_meaningful_progress_event(event: &Value) -> bool {
    matches!(
        event.get("type").and_then(|value| value.as_str()),
        Some("assistant") | Some("user")
    )
}

fn next_turn_watchdog_outcome(
    saw_result: bool,
    state: &TurnNormalizationState,
    watchdog: &mut crate::services::qwen::QwenStreamWatchdog,
) -> TurnWatchdogOutcome {
    if saw_result {
        return TurnWatchdogOutcome::Break;
    }

    match watchdog.on_timeout(state.meaningful_progress_seen) {
        Some(message) => TurnWatchdogOutcome::Retry { message },
        None => TurnWatchdogOutcome::Continue,
    }
}

fn normalize_system_event(json: &Value, state: &mut TurnNormalizationState) -> Vec<Value> {
    if json.get("subtype").and_then(|v| v.as_str()) != Some("session_start") {
        return Vec::new();
    }

    state.current_model = json
        .get("model")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            json.get("data")
                .and_then(|v| v.get("model"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
        });

    let Some(session_id) = json
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return Vec::new();
    };

    state.last_session_id = Some(session_id.to_string());
    if state.init_emitted_for_session.as_deref() == Some(session_id) {
        return Vec::new();
    }
    state.init_emitted_for_session = Some(session_id.to_string());
    vec![json!({
        "type": "system",
        "subtype": "init",
        "session_id": session_id,
    })]
}

fn normalize_stream_event(json: &Value, state: &mut TurnNormalizationState) -> Vec<Value> {
    let Some(event) = json.get("event") else {
        return Vec::new();
    };

    match event.get("type").and_then(|v| v.as_str()) {
        Some("message_start") => {
            state.current_model = event
                .get("message")
                .and_then(|v| v.get("model"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .or_else(|| state.current_model.clone());
            Vec::new()
        }
        Some("content_block_start") => {
            let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let block = event.get("content_block").unwrap_or(&Value::Null);
            let block_type = block
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("text")
                .to_string();
            state.partial_blocks.insert(
                index,
                PartialBlockState {
                    kind: block_type.clone(),
                    tool_name: block
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    input_json: block
                        .get("input")
                        .map(render_qwen_value)
                        .filter(|value| value != "{}")
                        .unwrap_or_default(),
                    thinking_emitted: false,
                },
            );
            if block_type == "thinking" {
                // Thinking-start is meaningful progress even though this wrapper does not emit
                // a normalized output event until a later delta or stop arrives.
                state.meaningful_progress_seen = true;
            }
            Vec::new()
        }
        Some("content_block_delta") => {
            let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let Some(block) = state.partial_blocks.get_mut(&index) else {
                return Vec::new();
            };

            match event
                .get("delta")
                .and_then(|v| v.get("type"))
                .and_then(|v| v.as_str())
            {
                Some("text_delta") => {
                    let text = event
                        .get("delta")
                        .and_then(|v| v.get("text"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if text.is_empty() {
                        Vec::new()
                    } else {
                        vec![assistant_text_event(text, state.current_model.as_deref())]
                    }
                }
                Some("thinking_delta") => {
                    let thinking = event
                        .get("delta")
                        .and_then(|v| v.get("thinking"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if thinking.is_empty() || block.thinking_emitted {
                        Vec::new()
                    } else {
                        block.thinking_emitted = true;
                        vec![assistant_thinking_event(state.current_model.as_deref())]
                    }
                }
                Some("input_json_delta") => {
                    let partial_json = event
                        .get("delta")
                        .and_then(|v| v.get("partial_json"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("");
                    if !partial_json.is_empty() {
                        block.input_json.push_str(partial_json);
                    }
                    Vec::new()
                }
                _ => Vec::new(),
            }
        }
        Some("content_block_stop") => {
            let index = event.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize;
            let Some(block) = state.partial_blocks.remove(&index) else {
                return Vec::new();
            };
            if block.kind != "tool_use" {
                return Vec::new();
            }

            let input = parse_tool_input_value(&block.input_json);
            vec![assistant_tool_use_event(
                block.tool_name.as_deref().unwrap_or("tool"),
                input,
                state.current_model.as_deref(),
            )]
        }
        _ => Vec::new(),
    }
}

fn normalize_assistant_message(json: &Value, state: &mut TurnNormalizationState) -> Vec<Value> {
    if state.partial_stream_seen {
        return Vec::new();
    }

    state.current_model = json
        .get("message")
        .and_then(|v| v.get("model"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| state.current_model.clone());

    let Some(content) = json
        .get("message")
        .and_then(|v| v.get("content"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };

    let mut events = Vec::new();
    for block in content {
        match block.get("type").and_then(|v| v.as_str()) {
            Some("text") => {
                let text = block.get("text").and_then(|v| v.as_str()).unwrap_or("");
                if !text.is_empty() {
                    events.push(assistant_text_event(text, state.current_model.as_deref()));
                }
            }
            Some("thinking") => {
                events.push(assistant_thinking_event(state.current_model.as_deref()));
            }
            Some("tool_use") => {
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                events.push(assistant_tool_use_event(
                    block.get("name").and_then(|v| v.as_str()).unwrap_or("tool"),
                    input,
                    state.current_model.as_deref(),
                ));
            }
            _ => {}
        }
    }

    events
}

fn normalize_user_message(json: &Value) -> Vec<Value> {
    let Some(content) = json
        .get("message")
        .and_then(|v| v.get("content"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };

    let mut events = Vec::new();
    for block in content {
        if block.get("type").and_then(|v| v.as_str()) != Some("tool_result") {
            continue;
        }
        let content = block
            .get("content")
            .map(render_qwen_value)
            .or_else(|| block.get("error").map(render_qwen_value))
            .unwrap_or_default();
        let is_error = block
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        events.push(json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "content": content,
                    "is_error": is_error,
                }]
            }
        }));
    }
    events
}

fn normalize_result_message(json: &Value, state: &mut TurnNormalizationState) -> Vec<Value> {
    if let Some(session_id) = json
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        state.last_session_id = Some(session_id.to_string());
    }

    let is_error = json
        .get("is_error")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let result = if is_error {
        json.get("error")
            .and_then(|v| v.get("message"))
            .and_then(|v| v.as_str())
            .or_else(|| json.get("subtype").and_then(|v| v.as_str()))
            .unwrap_or("Unknown Qwen error")
            .to_string()
    } else {
        json.get("result")
            .map(render_qwen_value)
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_default()
    };

    vec![json!({
        "type": "result",
        "subtype": if is_error { "error_during_execution" } else { "success" },
        "is_error": is_error,
        "result": if is_error { Value::Null } else { Value::String(result.clone()) },
        "errors": if is_error { json!([result.clone()]) } else { Value::Null },
        "session_id": state.last_session_id.clone(),
        "usage": json.get("usage").cloned().unwrap_or(Value::Null),
        "duration_ms": json.get("duration_ms").cloned().unwrap_or(Value::Null),
        "num_turns": json.get("num_turns").cloned().unwrap_or(Value::Null),
    })]
}

fn assistant_text_event(text: &str, model: Option<&str>) -> Value {
    json!({
        "type": "assistant",
        "message": {
            "model": model,
            "content": [{
                "type": "text",
                "text": text,
            }]
        }
    })
}

fn assistant_thinking_event(model: Option<&str>) -> Value {
    json!({
        "type": "assistant",
        "message": {
            "model": model,
            "content": [{
                "type": "thinking",
            }]
        }
    })
}

fn assistant_tool_use_event(name: &str, input: Value, model: Option<&str>) -> Value {
    json!({
        "type": "assistant",
        "message": {
            "model": model,
            "content": [{
                "type": "tool_use",
                "name": name,
                "input": input,
            }]
        }
    })
}

fn parse_tool_input_value(raw: &str) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        json!({})
    } else {
        serde_json::from_str(trimmed).unwrap_or_else(|_| Value::String(trimmed.to_string()))
    }
}

fn render_qwen_value(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::Bool(v) => v.to_string(),
        Value::Number(v) => v.to_string(),
        Value::String(v) => v.clone(),
        _ => serde_json::to_string(value).unwrap_or_default(),
    }
}

fn decode_external_prompt(line: &str) -> Result<String, String> {
    if let Some(encoded) = line.strip_prefix(TMUX_PROMPT_B64_PREFIX) {
        let bytes = BASE64_STANDARD
            .decode(encoded)
            .map_err(|e| format!("invalid base64 payload: {}", e))?;
        return String::from_utf8(bytes).map_err(|e| format!("invalid utf-8 payload: {}", e));
    }
    Ok(line.to_string())
}

fn derive_wrapper_error_message(stderr: &str, exit_code: Option<i32>) -> String {
    if !stderr.trim().is_empty() {
        stderr.trim().to_string()
    } else {
        format!("Qwen exited with code {:?}", exit_code)
    }
}

fn rewind_output_to_checkpoint(output: &mut std::fs::File, checkpoint: u64) -> Result<(), String> {
    output
        .flush()
        .map_err(|err| format!("flush output before retry: {}", err))?;
    output
        .set_len(checkpoint)
        .map_err(|err| format!("truncate output before retry: {}", err))?;
    Ok(())
}

fn extract_init_session_id(value: &Value) -> Option<String> {
    if value.get("type").and_then(|v| v.as_str()) != Some("system") {
        return None;
    }
    if value.get("subtype").and_then(|v| v.as_str()) != Some("init") {
        return None;
    }
    value
        .get("session_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn is_result_line(value: &Value) -> bool {
    value.get("type").and_then(|v| v.as_str()) == Some("result")
}

fn cleanup(output_file: &str, input_fifo: &str) {
    let _ = std::fs::remove_file(output_file);
    let _ = std::fs::remove_file(input_fifo);
}

fn emit_status(message: &str) {
    eprintln!("\x1b[90m{}\x1b[0m", message);
}

fn emit_result_error(output: &mut std::fs::File, message: &str) {
    let _ = emit_json_line(
        output,
        json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "errors": [message],
        }),
    );
}

fn emit_json_line(output: &mut std::fs::File, value: Value) -> Result<(), String> {
    let line =
        serde_json::to_string(&value).map_err(|e| format!("serialize output line: {}", e))?;
    writeln!(output, "{}", line).map_err(|e| format!("write output line: {}", e))?;
    output
        .flush()
        .map_err(|e| format!("flush output line: {}", e))?;
    render_for_terminal(&line);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        TurnNormalizationState, TurnWatchdogOutcome, build_turn_args, decode_external_prompt,
        next_turn_watchdog_outcome, normalize_qwen_line,
    };
    use crate::services::qwen::qwen_project_cache_key;
    use std::fs;
    use std::io::Write;
    use std::time::Duration;
    use tempfile::TempDir;

    fn with_temp_qwen_home<F>(f: F)
    where
        F: FnOnce(&TempDir, &TempDir),
    {
        let _guard = crate::services::discord::runtime_store::lock_test_env();
        let temp_home = TempDir::new().unwrap();
        let temp_project = TempDir::new().unwrap();

        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");

        unsafe {
            std::env::set_var("HOME", temp_home.path());
            std::env::set_var("USERPROFILE", temp_home.path());
        }

        f(&temp_home, &temp_project);

        match prev_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match prev_userprofile {
            Some(value) => unsafe { std::env::set_var("USERPROFILE", value) },
            None => unsafe { std::env::remove_var("USERPROFILE") },
        }
    }

    fn create_prior_qwen_chat_cache(temp_home: &TempDir, working_dir: &TempDir) {
        let chats_dir = temp_home
            .path()
            .join(".qwen")
            .join("projects")
            .join(qwen_project_cache_key(working_dir.path().to_str().unwrap()))
            .join("chats");
        fs::create_dir_all(&chats_dir).unwrap();
        fs::write(chats_dir.join("turn-1.jsonl"), "{\"type\":\"result\"}\n").unwrap();
    }

    #[test]
    fn test_decode_external_prompt_keeps_plain_line() {
        assert_eq!(decode_external_prompt("hello").unwrap(), "hello");
    }

    #[test]
    fn test_decode_external_prompt_decodes_base64_payload() {
        let line = "__AGENTDESK_B64__:bGluZTEKbGluZTI=";
        assert_eq!(decode_external_prompt(line).unwrap(), "line1\nline2");
    }

    #[test]
    fn normalize_stream_event_emits_text_fragment() {
        let mut state = Default::default();
        let _ = normalize_qwen_line(
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"text"}}}"#,
            &mut state,
        );
        let events = normalize_qwen_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello"}}}"#,
            &mut state,
        );
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["type"], "assistant");
        assert_eq!(events[0]["message"]["content"][0]["text"], "hello");
    }

    #[test]
    fn normalize_stream_event_redacts_thinking_delta() {
        let mut state = Default::default();
        let _ = normalize_qwen_line(
            r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","signature":"pondering"}}}"#,
            &mut state,
        );
        let events = normalize_qwen_line(
            r#"{"type":"stream_event","event":{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"internal reasoning"}}}"#,
            &mut state,
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["message"]["content"][0]["type"], "thinking");
        assert!(events[0]["message"]["content"][0].get("thinking").is_none());
        assert!(!events[0].to_string().contains("internal reasoning"));
    }

    #[test]
    fn normalize_assistant_message_redacts_thinking_block() {
        let mut state = Default::default();
        let events = normalize_qwen_line(
            r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"internal reasoning"}]}}"#,
            &mut state,
        );

        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["message"]["content"][0]["type"], "thinking");
        assert!(events[0]["message"]["content"][0].get("thinking").is_none());
        assert!(!events[0].to_string().contains("internal reasoning"));
    }

    #[test]
    fn build_turn_args_uses_fresh_without_resume_token_or_prior_cache() {
        with_temp_qwen_home(|_temp_home, working_dir| {
            let args = build_turn_args(
                "hello",
                Some("qwen-max"),
                None,
                working_dir.path().to_str().unwrap(),
            )
            .unwrap();
            assert!(args.windows(2).any(|pair| pair == ["--model", "qwen-max"]));
            assert!(!args.iter().any(|arg| arg == "--continue"));
            assert!(!args.iter().any(|arg| arg == "--resume"));
        });
    }

    #[test]
    fn build_turn_args_uses_continue_with_prior_cache() {
        with_temp_qwen_home(|temp_home, working_dir| {
            create_prior_qwen_chat_cache(temp_home, working_dir);
            let args = build_turn_args(
                "hello",
                Some("qwen-max"),
                None,
                working_dir.path().to_str().unwrap(),
            )
            .unwrap();
            assert!(args.windows(2).any(|pair| pair == ["--model", "qwen-max"]));
            assert!(args.iter().any(|arg| arg == "--continue"));
            assert!(!args.iter().any(|arg| arg == "--resume"));
        });
    }

    #[test]
    fn build_turn_args_prefers_resume_token_when_present() {
        with_temp_qwen_home(|_temp_home, working_dir| {
            let args = build_turn_args(
                "hello",
                None,
                Some("session-123"),
                working_dir.path().to_str().unwrap(),
            )
            .unwrap();
            assert!(
                args.windows(2)
                    .any(|pair| pair == ["--resume", "session-123"])
            );
            assert!(!args.iter().any(|arg| arg == "--continue"));
        });
    }

    #[test]
    fn rewind_output_to_checkpoint_discards_retryable_partial_lines() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("turn.jsonl");
        let mut output = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .unwrap();

        writeln!(output, "{{\"type\":\"assistant\",\"message\":\"keep\"}}").unwrap();
        output.flush().unwrap();
        let checkpoint = output.metadata().unwrap().len();

        writeln!(output, "{{\"type\":\"assistant\",\"message\":\"drop\"}}").unwrap();
        output.flush().unwrap();

        super::rewind_output_to_checkpoint(&mut output, checkpoint).unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\"type\":\"assistant\",\"message\":\"keep\"}\n"
        );
    }

    #[test]
    fn qwen_tmux_watchdog_uses_startup_timeout_before_first_progress() {
        let state = TurnNormalizationState::default();
        let mut watchdog = crate::services::qwen::QwenStreamWatchdog::new(
            Duration::from_millis(200),
            Duration::from_secs(1),
            Duration::from_secs(2),
        );
        let expected_message = watchdog.startup_retry_message();

        loop {
            match next_turn_watchdog_outcome(false, &state, &mut watchdog) {
                TurnWatchdogOutcome::Continue => {}
                TurnWatchdogOutcome::Retry { message } => {
                    assert_eq!(message, expected_message);
                    break;
                }
                TurnWatchdogOutcome::Break => panic!("startup watchdog should not break"),
            }
        }
    }

    #[test]
    fn qwen_tmux_watchdog_uses_idle_timeout_after_progress() {
        let mut state = TurnNormalizationState::default();
        let assistant_line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"partial"}]}}"#;
        let normalized = normalize_qwen_line(assistant_line, &mut state);
        for event in normalized {
            if super::is_meaningful_progress_event(&event) {
                state.meaningful_progress_seen = true;
            }
        }
        assert!(state.meaningful_progress_seen);

        let mut watchdog = crate::services::qwen::QwenStreamWatchdog::new(
            Duration::from_millis(200),
            Duration::from_secs(1),
            Duration::from_secs(2),
        );
        watchdog.observe_line();
        let expected_message = watchdog.idle_retry_message();

        loop {
            match next_turn_watchdog_outcome(false, &state, &mut watchdog) {
                TurnWatchdogOutcome::Continue => {}
                TurnWatchdogOutcome::Retry { message } => {
                    assert_eq!(message, expected_message);
                    break;
                }
                TurnWatchdogOutcome::Break => panic!("idle watchdog should not break"),
            }
        }
    }

    #[test]
    fn qwen_tmux_watchdog_treats_thinking_start_as_progress() {
        let mut state = TurnNormalizationState::default();
        let thinking_start = r#"{"type":"stream_event","event":{"type":"content_block_start","index":0,"content_block":{"type":"thinking","signature":"pondering"}}}"#;

        let normalized = normalize_qwen_line(thinking_start, &mut state);

        assert!(normalized.is_empty());
        assert!(
            state.meaningful_progress_seen,
            "thinking start should count as progress even without a normalized output event"
        );

        let mut watchdog = crate::services::qwen::QwenStreamWatchdog::new(
            Duration::from_millis(200),
            Duration::from_secs(1),
            Duration::from_secs(2),
        );
        watchdog.observe_line();
        let expected_message = watchdog.idle_retry_message();

        loop {
            match next_turn_watchdog_outcome(false, &state, &mut watchdog) {
                TurnWatchdogOutcome::Continue => {}
                TurnWatchdogOutcome::Retry { message } => {
                    assert_eq!(message, expected_message);
                    break;
                }
                TurnWatchdogOutcome::Break => panic!("idle watchdog should not break"),
            }
        }
    }
}
