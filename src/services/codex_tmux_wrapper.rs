use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use crate::services::codex::{
    CODEX_BACKGROUND_TASK_NOTIFICATION_ID, CODEX_BACKGROUND_TASK_NOTIFICATION_STATUS,
    CodexLaunchOptions, build_codex_exec_args, codex_background_event_summary,
};
use crate::services::tmux_common::RotatingJsonlWriter;
use crate::services::tmux_wrapper::{InputMode, render_for_terminal};

const TMUX_PROMPT_B64_PREFIX: &str = "__AGENTDESK_B64__:";
const TMUX_PROMPT_B64_CHUNK_PREFIX: &str = "__AGENTDESK_B64_CHUNK__:";
const DEFAULT_CODEX_FIRST_EVENT_TIMEOUT_SECS: u64 = 120;

pub fn run(
    output_file: &str,
    input_fifo: &str,
    prompt_file: &str,
    working_dir: &str,
    codex_bin: &str,
    codex_model: Option<&str>,
    reasoning_effort: Option<&str>,
    developer_instructions: Option<&str>,
    resume_session_id: Option<&str>,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
    input_mode: InputMode,
    compact_token_limit: Option<u64>,
    add_dirs: &[String],
) {
    let mode_label = match input_mode {
        InputMode::Fifo => "tmux resume loop",
        InputMode::Pipe => "pipe-mode",
    };
    eprintln!("\x1b[90m═══════════════════════════════════════════════════════\x1b[0m");
    eprintln!("\x1b[90m  AgentDesk Codex Session ({})\x1b[0m", mode_label);
    if input_mode == InputMode::Fifo {
        eprintln!("\x1b[90m  Type messages below when Codex is ready.\x1b[0m");
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

    let expanded_dir = crate::runtime_layout::expand_user_path(working_dir)
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| working_dir.to_string());

    let (prompt_tx, prompt_rx) = mpsc::channel::<String>();

    // Terminal input — only in Fifo mode (interactive tmux session)
    if input_mode == InputMode::Fifo {
        let prompt_tx = prompt_tx.clone();
        std::thread::spawn(move || {
            loop {
                let reader = open_codex_terminal_input_reader();
                match read_codex_terminal_input_lines(reader, &prompt_tx) {
                    TerminalInputLoopOutcome::RetryReader => {
                        std::thread::sleep(std::time::Duration::from_millis(250));
                    }
                    TerminalInputLoopOutcome::Stop => break,
                }
            }
        });
    }

    // External input
    // Fifo mode: reads from named FIFO
    // Pipe mode: reads from process stdin (parent writes to child stdin pipe)
    {
        let prompt_tx = prompt_tx.clone();
        let input_fifo = input_fifo.to_string();
        std::thread::spawn(move || {
            let mut decoder = ExternalPromptDecoder::default();
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
                match decoder.decode_line(&line) {
                    Ok(Some(prompt)) => {
                        if !prompt.trim().is_empty() {
                            let _ = prompt_tx.send(prompt);
                        }
                    }
                    Ok(None) => {}
                    Err(err) => {
                        eprintln!("\x1b[90m[input decode error: {}]\x1b[0m", err);
                    }
                }
            }
        });
    }

    let mut output = match RotatingJsonlWriter::open(output_file) {
        Ok(file) => file,
        Err(e) => {
            eprintln!("\x1b[31mFailed to open output file: {}\x1b[0m", e);
            std::process::exit(1);
        }
    };

    let mut thread_id = normalize_resume_session_id(resume_session_id);
    let initial_turn = run_turn(
        &mut output,
        codex_bin,
        codex_model,
        reasoning_effort,
        developer_instructions,
        &expanded_dir,
        &prompt,
        &mut thread_id,
        fast_mode_enabled,
        goals_enabled,
        compact_token_limit,
        add_dirs,
    );
    if let Err(err) = initial_turn {
        emit_result_error(&mut output, &err);
        let exit_reason_path = format!("{}.exit_reason", output_file);
        let exit_str = format!("error:{err}");
        let _ = std::fs::write(&exit_reason_path, &exit_str);
        // #2442 (H2) — initial turn fail-fast path. Files are preserved
        // for post-mortem so the sentinel reaches the watcher.
        crate::services::tmux_common::emit_wrapper_sentinel(
            output_file,
            crate::services::tmux_common::WrapperSentinel::TerminalEnd { exit: &exit_str },
        );
        // Preserve output files for post-mortem on error
        eprintln!("\x1b[33m[preserving output files for post-mortem: {output_file}]\x1b[0m");
        std::process::exit(1);
    }

    // #2442 (H3) — initial turn succeeded; wrapper is now back to
    // waiting on `prompt_rx`. Emit ready_for_input so the tmux.rs probe
    // can short-circuit without waiting for the 2s tick.
    crate::services::tmux_common::emit_wrapper_sentinel(
        output_file,
        crate::services::tmux_common::WrapperSentinel::ReadyForInput { provider: "codex" },
    );

    let mut followup_error: Option<String> = None;
    while let Ok(next_prompt) = prompt_rx.recv() {
        if let Err(err) = run_turn(
            &mut output,
            codex_bin,
            codex_model,
            reasoning_effort,
            developer_instructions,
            &expanded_dir,
            next_prompt.trim(),
            &mut thread_id,
            fast_mode_enabled,
            goals_enabled,
            compact_token_limit,
            add_dirs,
        ) {
            emit_result_error(&mut output, &err);
            followup_error = Some(err);
            break;
        }
        // #2442 (H3) — same as above for follow-up turns.
        crate::services::tmux_common::emit_wrapper_sentinel(
            output_file,
            crate::services::tmux_common::WrapperSentinel::ReadyForInput { provider: "codex" },
        );
    }

    let exit_reason_path = format!("{}.exit_reason", output_file);
    let exit_reason = if let Some(ref err) = followup_error {
        // Follow-up turn failed — preserve files for post-mortem (same as initial turn)
        let reason = format!("error:{err}");
        let _ = std::fs::write(&exit_reason_path, &reason);
        eprintln!("\x1b[33m[preserving output files for post-mortem: {output_file}]\x1b[0m");
        reason
    } else {
        // Normal exit — prompt_rx closed, all turns succeeded.
        // #2442 (H2) — emit terminal_end BEFORE `cleanup()` removes the
        // JSONL so the sentinel actually reaches the watcher tail. The
        // failure branch (preserve for post-mortem) emits afterwards because
        // the file is preserved.
        let reason = "exit:0".to_string();
        let _ = std::fs::write(&exit_reason_path, &reason);
        crate::services::tmux_common::emit_wrapper_sentinel(
            output_file,
            crate::services::tmux_common::WrapperSentinel::TerminalEnd { exit: &reason },
        );
        cleanup(output_file, input_fifo);
        reason
    };
    if followup_error.is_some() {
        // Error branch: file preserved — emit the sentinel now so the
        // recovery_engine drain can short-circuit on this case too.
        crate::services::tmux_common::emit_wrapper_sentinel(
            output_file,
            crate::services::tmux_common::WrapperSentinel::TerminalEnd { exit: &exit_reason },
        );
    }
    eprintln!();
    eprintln!("\x1b[90m--- Session ended ({exit_reason}) ---\x1b[0m");
}

fn normalize_resume_session_id(resume_session_id: Option<&str>) -> Option<String> {
    resume_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

#[derive(Default)]
struct ExternalPromptDecoder {
    chunked: HashMap<String, ChunkedPrompt>,
}

struct ChunkedPrompt {
    chunks: Vec<Option<String>>,
    received: usize,
}

impl ExternalPromptDecoder {
    fn decode_line(&mut self, line: &str) -> Result<Option<String>, String> {
        if let Some(encoded) = line.strip_prefix(TMUX_PROMPT_B64_PREFIX) {
            return decode_base64_prompt(encoded).map(Some);
        }

        if let Some(chunk) = line.strip_prefix(TMUX_PROMPT_B64_CHUNK_PREFIX) {
            return self.decode_chunk(chunk);
        }

        Ok(Some(line.to_string()))
    }

    fn decode_chunk(&mut self, line: &str) -> Result<Option<String>, String> {
        let mut parts = line.splitn(4, ':');
        let message_id = parts
            .next()
            .filter(|value| !value.is_empty())
            .ok_or("missing chunk message id")?;
        let index = parts
            .next()
            .ok_or("missing chunk index")?
            .parse::<usize>()
            .map_err(|_| "invalid chunk index".to_string())?;
        let total = parts
            .next()
            .ok_or("missing chunk total")?
            .parse::<usize>()
            .map_err(|_| "invalid chunk total".to_string())?;
        let chunk = parts.next().ok_or("missing chunk payload")?;

        if total == 0 || total > 10_000 {
            return Err("invalid chunk total".to_string());
        }
        if index >= total {
            return Err("chunk index out of range".to_string());
        }

        let entry = self
            .chunked
            .entry(message_id.to_string())
            .or_insert_with(|| ChunkedPrompt {
                chunks: vec![None; total],
                received: 0,
            });
        if entry.chunks.len() != total {
            self.chunked.remove(message_id);
            return Err("chunk total changed for message id".to_string());
        }
        if entry.chunks[index].is_some() {
            self.chunked.remove(message_id);
            return Err("duplicate chunk index".to_string());
        }

        entry.chunks[index] = Some(chunk.to_string());
        entry.received += 1;
        if entry.received != total {
            return Ok(None);
        }

        let entry = self
            .chunked
            .remove(message_id)
            .ok_or("completed chunk state missing")?;
        let mut encoded = String::new();
        for chunk in entry.chunks {
            encoded.push_str(&chunk.ok_or("missing completed chunk")?);
        }
        decode_base64_prompt(&encoded).map(Some)
    }
}

fn decode_base64_prompt(encoded: &str) -> Result<String, String> {
    let bytes = BASE64_STANDARD
        .decode(encoded)
        .map_err(|e| format!("invalid base64 payload: {}", e))?;
    String::from_utf8(bytes).map_err(|e| format!("invalid utf-8 payload: {}", e))
}

fn decode_external_prompt(line: &str) -> Result<String, String> {
    if let Some(encoded) = line.strip_prefix(TMUX_PROMPT_B64_PREFIX) {
        return decode_base64_prompt(encoded);
    }
    Ok(line.to_string())
}

fn codex_first_event_timeout() -> std::time::Duration {
    let seconds = std::env::var("AGENTDESK_CODEX_FIRST_EVENT_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_CODEX_FIRST_EVENT_TIMEOUT_SECS);
    std::time::Duration::from_secs(seconds)
}

fn cleanup(output_file: &str, input_fifo: &str) {
    let _ = std::fs::remove_file(output_file);
    let _ = std::fs::remove_file(input_fifo);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalInputLoopOutcome {
    RetryReader,
    Stop,
}

fn open_codex_terminal_input_reader() -> Box<dyn BufRead> {
    match std::fs::OpenOptions::new().read(true).open("/dev/tty") {
        Ok(tty) => Box::new(BufReader::new(tty)),
        Err(err) => {
            eprintln!("\x1b[90m[terminal input tty open failed: {}]\x1b[0m", err);
            Box::new(BufReader::new(std::io::stdin()))
        }
    }
}

fn read_codex_terminal_input_lines<R: BufRead>(
    mut reader: R,
    prompt_tx: &mpsc::Sender<String>,
) -> TerminalInputLoopOutcome {
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return TerminalInputLoopOutcome::RetryReader,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                emit_status("[terminal message received]");
                if prompt_tx.send(trimmed.to_string()).is_err() {
                    return TerminalInputLoopOutcome::Stop;
                }
            }
            Err(err) => {
                eprintln!("\x1b[90m[terminal input read error: {}]\x1b[0m", err);
                return TerminalInputLoopOutcome::RetryReader;
            }
        }
    }
}

fn run_turn(
    output: &mut RotatingJsonlWriter,
    codex_bin: &str,
    codex_model: Option<&str>,
    reasoning_effort: Option<&str>,
    developer_instructions: Option<&str>,
    working_dir: &str,
    prompt: &str,
    thread_id: &mut Option<String>,
    fast_mode_enabled: Option<bool>,
    goals_enabled: Option<bool>,
    compact_token_limit: Option<u64>,
    add_dirs: &[String],
) -> Result<(), String> {
    emit_status("[sending...]");

    let default_reasoning_effort = codex_model
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|_| "high");
    let effective_reasoning_effort = reasoning_effort
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or(default_reasoning_effort);
    let add_dir_refs = add_dirs.iter().map(String::as_str).collect::<Vec<_>>();
    let args = build_codex_exec_args(
        &CodexLaunchOptions::new(prompt)
            .with_resume_session_id(thread_id.as_deref())
            .with_developer_instructions(developer_instructions)
            .with_model(codex_model)
            .with_reasoning_effort(effective_reasoning_effort)
            .with_compact_token_limit(compact_token_limit)
            .with_readonly_mode(false)
            .with_fast_mode_enabled(fast_mode_enabled)
            .with_goals_enabled(goals_enabled)
            .with_cwd(Some(working_dir))
            .with_add_dirs(&add_dir_refs),
    );

    let mut cmd = Command::new(codex_bin);
    crate::services::platform::augment_exec_path(&mut cmd, codex_bin);
    crate::services::process::configure_child_process_group(&mut cmd);
    let mut child = cmd
        .args(&args)
        .current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to start Codex: {}", e))?;

    let child_pid = child.id();

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture Codex stdout".to_string())?;
    let (stdout_tx, stdout_rx) = mpsc::channel::<Result<Option<String>, String>>();
    std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => {
                    let _ = stdout_tx.send(Ok(None));
                    break;
                }
                Ok(_) => {
                    if stdout_tx.send(Ok(Some(line))).is_err() {
                        break;
                    }
                }
                Err(err) => {
                    let _ = stdout_tx.send(Err(format!("Failed to read Codex output: {err}")));
                    break;
                }
            }
        }
    });

    let mut stdout_line = String::new();
    let mut state = CodexWrapperTurnState::default();
    let start = std::time::Instant::now();
    let mut saw_any_stdout = false;
    let first_event_timeout = codex_first_event_timeout();

    loop {
        let next_line = if saw_any_stdout {
            stdout_rx
                .recv()
                .map_err(|_| "Codex stdout reader disconnected".to_string())?
        } else {
            match stdout_rx.recv_timeout(first_event_timeout) {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    crate::services::process::kill_pid_tree(child_pid);
                    let _ = child.wait();
                    return Err(format!(
                        "Codex produced no JSON event within {}s after sending prompt",
                        first_event_timeout.as_secs()
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("Codex stdout reader disconnected".to_string());
                }
            }
        };

        let Some(line) = next_line? else {
            break;
        };

        saw_any_stdout = true;
        stdout_line.clear();
        stdout_line.push_str(&line);

        let trimmed = stdout_line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };

        handle_codex_wrapper_event(output, &json, thread_id, &mut state, start)?;
    }

    // Kill Codex process tree (including any cmd.exe / bash children) before waiting.
    // Without this, child processes spawned by Codex survive as orphan processes.
    crate::services::process::kill_pid_tree(child_pid);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let wait = child
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for Codex: {}", e))?;

    if !wait.status.success() && !state.saw_turn_completed {
        let stderr = String::from_utf8_lossy(&wait.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("Codex exited with code {:?}", wait.status.code())
        } else {
            stderr
        };
        emit_result_error(output, &message);
        return Err(message);
    }

    if !state.saw_turn_completed {
        emit_json_line(
            output,
            serde_json::json!({
                "type": "result",
                "subtype": "success",
                "result": state.final_text,
                "session_id": thread_id.as_deref(),
                "duration_ms": start.elapsed().as_millis() as u64,
            }),
        )?;
    }

    Ok(())
}

#[derive(Default)]
struct CodexWrapperTurnState {
    final_text: String,
    saw_turn_completed: bool,
    unknown_event_count: u64,
}

fn handle_codex_wrapper_event(
    output: &mut RotatingJsonlWriter,
    json: &serde_json::Value,
    thread_id: &mut Option<String>,
    state: &mut CodexWrapperTurnState,
    start: std::time::Instant,
) -> Result<(), String> {
    match json.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "thread.started" => {
            if let Some(id) = json.get("thread_id").and_then(|v| v.as_str()) {
                *thread_id = Some(id.to_string());
                emit_json_line(
                    output,
                    serde_json::json!({
                        "type": "system",
                        "subtype": "init",
                        "session_id": id,
                    }),
                )?;
            }
        }
        "item.started" => {
            if let Some(item) = json.get("item") {
                handle_item_started(output, item)?;
            }
        }
        "item.completed" => {
            if let Some(item) = json.get("item") {
                handle_item_completed(output, item, &mut state.final_text)?;
            }
        }
        "response_item" => {
            if let Some(payload) = json.get("payload") {
                handle_response_item(output, payload, &mut state.final_text)?;
            }
        }
        "background_event" => {
            handle_background_event(output, json)?;
        }
        "event_msg" => {
            handle_event_msg(output, json, thread_id, state, start)?;
        }
        "task_complete" => {
            emit_success_result(
                output,
                json.get("payload").unwrap_or(json),
                thread_id,
                state,
                start,
            )?;
        }
        "turn.completed" => {
            emit_success_result(output, json, thread_id, state, start)?;
        }
        "error" => {
            let message = json
                .get("message")
                .and_then(|v| v.as_str())
                .unwrap_or("Unknown Codex error");
            emit_result_error(output, message);
            state.saw_turn_completed = true;
        }
        event_type => {
            record_unknown_codex_event(output, state, event_type)?;
        }
    }

    Ok(())
}

fn handle_response_item(
    output: &mut RotatingJsonlWriter,
    payload: &serde_json::Value,
    final_text: &mut String,
) -> Result<(), String> {
    match payload.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "message" => handle_response_message(output, payload, final_text),
        "function_call" | "custom_tool_call" | "tool_search_call" => {
            handle_response_tool_call(output, payload)
        }
        "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
            handle_response_tool_output(output, payload)
        }
        "reasoning" => emit_json_line(output, assistant_redacted_thinking_event()),
        _ => Ok(()),
    }
}

fn handle_response_message(
    output: &mut RotatingJsonlWriter,
    payload: &serde_json::Value,
    final_text: &mut String,
) -> Result<(), String> {
    if payload.get("role").and_then(|v| v.as_str()) != Some("assistant") {
        return Ok(());
    }
    let Some(content) = payload.get("content").and_then(|v| v.as_array()) else {
        return Ok(());
    };
    let include_in_final_text = payload.get("phase").and_then(|v| v.as_str()) != Some("commentary");
    for item in content {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if item_type != "output_text" && item_type != "text" {
            continue;
        }
        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if text.is_empty() {
            continue;
        }
        if include_in_final_text {
            if !final_text.is_empty() {
                final_text.push_str("\n\n");
            }
            final_text.push_str(text);
        }
        emit_json_line(
            output,
            serde_json::json!({
                "type": "assistant",
                "message": {
                    "content": [{
                        "type": "text",
                        "text": text,
                    }]
                }
            }),
        )?;
    }
    Ok(())
}

fn handle_response_tool_call(
    output: &mut RotatingJsonlWriter,
    payload: &serde_json::Value,
) -> Result<(), String> {
    let Some(name) = payload
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        return Ok(());
    };
    let input = payload
        .get("arguments")
        .or_else(|| payload.get("input"))
        .or_else(|| payload.get("action"))
        .map(compact_json_or_string)
        .unwrap_or_else(|| "{}".to_string());
    emit_json_line(
        output,
        serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "name": name,
                    "input": input,
                }]
            }
        }),
    )
}

fn handle_response_tool_output(
    output: &mut RotatingJsonlWriter,
    payload: &serde_json::Value,
) -> Result<(), String> {
    let content = payload
        .get("output")
        .or_else(|| payload.get("content"))
        .map(compact_json_or_string)
        .unwrap_or_default();
    let is_error = payload
        .get("is_error")
        .or_else(|| payload.get("isError"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    emit_json_line(
        output,
        serde_json::json!({
            "type": "user",
            "message": {
                "content": [{
                    "type": "tool_result",
                    "content": content,
                    "is_error": is_error,
                }]
            }
        }),
    )
}

fn handle_event_msg(
    output: &mut RotatingJsonlWriter,
    json: &serde_json::Value,
    thread_id: &Option<String>,
    state: &mut CodexWrapperTurnState,
    start: std::time::Instant,
) -> Result<(), String> {
    let Some(payload) = json.get("payload") else {
        return Ok(());
    };
    match payload.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "task_complete" => emit_success_result(output, payload, thread_id, state, start),
        "agent_reasoning" => emit_json_line(output, assistant_redacted_thinking_event()),
        "token_count" | "composer_ready" => Ok(()),
        event_type => record_unknown_codex_event(output, state, event_type),
    }
}

fn emit_success_result(
    output: &mut RotatingJsonlWriter,
    json: &serde_json::Value,
    thread_id: &Option<String>,
    state: &mut CodexWrapperTurnState,
    start: std::time::Instant,
) -> Result<(), String> {
    if state.saw_turn_completed {
        return Ok(());
    }
    if let Some(text) = json
        .get("last_agent_message")
        .and_then(|v| v.as_str())
        .filter(|text| !text.is_empty())
    {
        state.final_text.clear();
        state.final_text.push_str(text);
    }
    let usage = json
        .get("usage")
        .or_else(|| {
            json.get("info")
                .and_then(|info| info.get("total_token_usage"))
        })
        .cloned()
        .unwrap_or_default();
    let input_tokens = usage
        .get("input_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let output_tokens = usage
        .get("output_tokens")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    emit_json_line(
        output,
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "result": state.final_text,
            "session_id": thread_id.as_deref(),
            "duration_ms": start.elapsed().as_millis() as u64,
            "input_tokens": input_tokens,
            "output_tokens": output_tokens,
        }),
    )?;
    state.saw_turn_completed = true;
    Ok(())
}

fn record_unknown_codex_event(
    output: &mut RotatingJsonlWriter,
    state: &mut CodexWrapperTurnState,
    event_type: &str,
) -> Result<(), String> {
    state.unknown_event_count = state.unknown_event_count.saturating_add(1);
    let label = if event_type.is_empty() {
        "<missing>"
    } else {
        event_type
    };
    eprintln!(
        "\x1b[90m[codex wrapper ignored unknown event type: {label}; count={}]\x1b[0m",
        state.unknown_event_count
    );
    emit_json_line(
        output,
        serde_json::json!({
            "type": "system",
            "subtype": "diagnostic",
            "source": "codex_tmux_wrapper",
            "diagnostic_kind": "unknown_event",
            "event_type": label,
            "count": state.unknown_event_count,
            "message": format!("Codex wrapper ignored unknown event type: {label}"),
        }),
    )
}

fn compact_json_or_string(value: &serde_json::Value) -> String {
    value
        .as_str()
        .map(str::to_string)
        .unwrap_or_else(|| value.to_string())
}

#[cfg(test)]
mod modern_event_tests {
    use super::{CodexWrapperTurnState, handle_codex_wrapper_event};
    use crate::services::codex::{
        CODEX_BACKGROUND_TASK_NOTIFICATION_ID, CODEX_BACKGROUND_TASK_NOTIFICATION_STATUS,
    };
    use crate::services::tmux_common::RotatingJsonlWriter;
    use serde_json::json;

    fn read_jsonl(path: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect()
    }

    #[test]
    fn modern_response_item_and_task_complete_emit_assistant_and_result() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-modern".to_string());
        let mut state = CodexWrapperTurnState::default();
        let start = std::time::Instant::now();

        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "modern final" }]
                }
            }),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();
        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "event_msg",
                "payload": {
                    "type": "task_complete",
                    "turn_id": "turn-1",
                    "last_agent_message": "modern final",
                    "duration_ms": 12
                }
            }),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();

        let lines = read_jsonl(&path);
        assert_eq!(
            lines[0],
            json!({
                "type": "assistant",
                "message": {
                    "content": [{
                        "type": "text",
                        "text": "modern final"
                    }]
                }
            })
        );
        assert_eq!(lines[1]["type"], "result");
        assert_eq!(lines[1]["subtype"], "success");
        assert_eq!(lines[1]["result"], "modern final");
        assert_eq!(lines[1]["session_id"], "thread-modern");
        assert!(state.saw_turn_completed);
    }

    #[test]
    fn top_level_task_complete_finalizes_with_last_agent_message_fallback() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = None;
        let mut state = CodexWrapperTurnState::default();

        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "task_complete",
                "last_agent_message": "fallback final"
            }),
            &mut thread_id,
            &mut state,
            std::time::Instant::now(),
        )
        .unwrap();

        let lines = read_jsonl(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "result");
        assert_eq!(lines[0]["subtype"], "success");
        assert_eq!(lines[0]["result"], "fallback final");
        assert!(state.saw_turn_completed);
    }

    #[test]
    fn task_complete_last_agent_message_replaces_partial_response_item_text() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-modern".to_string());
        let mut state = CodexWrapperTurnState::default();
        let start = std::time::Instant::now();

        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "partial" }]
                }
            }),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();
        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "event_msg",
                "payload": {
                    "type": "task_complete",
                    "last_agent_message": "full final"
                }
            }),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();

        let lines = read_jsonl(&path);
        assert_eq!(lines[0]["type"], "assistant");
        assert_eq!(lines[0]["message"]["content"][0]["text"], "partial");
        assert_eq!(lines[1]["type"], "result");
        assert_eq!(lines[1]["result"], "full final");
        assert_eq!(state.final_text, "full final");
        assert!(state.saw_turn_completed);
    }

    #[test]
    fn old_item_completed_and_turn_completed_still_emit_assistant_and_result() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-old".to_string());
        let mut state = CodexWrapperTurnState::default();
        let start = std::time::Instant::now();

        handle_codex_wrapper_event(
            &mut output,
            &json!({"type":"item.completed","item":{"type":"agent_message","text":"legacy final"}}),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();
        handle_codex_wrapper_event(
            &mut output,
            &json!({"type":"turn.completed","usage":{"input_tokens":10,"output_tokens":3}}),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();

        let lines = read_jsonl(&path);
        assert_eq!(lines[0]["type"], "assistant");
        assert_eq!(lines[0]["message"]["content"][0]["text"], "legacy final");
        assert_eq!(lines[1]["type"], "result");
        assert_eq!(lines[1]["result"], "legacy final");
        assert_eq!(lines[1]["input_tokens"], 10);
        assert_eq!(lines[1]["output_tokens"], 3);
    }

    #[test]
    fn unknown_future_events_are_counted_for_diagnostics() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = None;
        let mut state = CodexWrapperTurnState::default();

        handle_codex_wrapper_event(
            &mut output,
            &json!({"type":"future.event","payload":{"type":"nested"}}),
            &mut thread_id,
            &mut state,
            std::time::Instant::now(),
        )
        .unwrap();
        handle_codex_wrapper_event(
            &mut output,
            &json!({"type":"event_msg","payload":{"type":"future_event"}}),
            &mut thread_id,
            &mut state,
            std::time::Instant::now(),
        )
        .unwrap();

        assert_eq!(state.unknown_event_count, 2);
        assert!(!state.saw_turn_completed);
        let lines = read_jsonl(&path);
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["type"], "system");
        assert_eq!(lines[0]["subtype"], "diagnostic");
        assert_eq!(lines[0]["diagnostic_kind"], "unknown_event");
        assert_eq!(lines[0]["event_type"], "future.event");
        assert_eq!(lines[0]["count"], 1);
        assert_eq!(lines[1]["event_type"], "future_event");
        assert_eq!(lines[1]["count"], 2);
    }

    #[test]
    fn background_event_emits_task_notification_marker() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = None;
        let mut state = CodexWrapperTurnState::default();

        handle_codex_wrapper_event(
            &mut output,
            &json!({"type":"background_event","message":"CI green"}),
            &mut thread_id,
            &mut state,
            std::time::Instant::now(),
        )
        .unwrap();

        let lines = read_jsonl(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0],
            json!({
                "type": "system",
                "subtype": "task_notification",
                "task_id": CODEX_BACKGROUND_TASK_NOTIFICATION_ID,
                "status": CODEX_BACKGROUND_TASK_NOTIFICATION_STATUS,
                "summary": "CI green",
                "task_notification_kind": "background",
            })
        );
        assert!(!state.saw_turn_completed);
    }

    #[test]
    fn response_item_reasoning_emits_redacted_thinking() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = None;
        let mut state = CodexWrapperTurnState::default();

        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "reasoning",
                    "summary": [{ "type": "summary_text", "text": "internal reasoning" }]
                }
            }),
            &mut thread_id,
            &mut state,
            std::time::Instant::now(),
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("internal reasoning"));
        let lines = read_jsonl(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "assistant");
        assert_eq!(lines[0]["message"]["content"][0]["type"], "thinking");
        assert!(lines[0]["message"]["content"][0].get("thinking").is_none());
        assert!(!state.saw_turn_completed);
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{
        ExternalPromptDecoder, TerminalInputLoopOutcome, decode_external_prompt, emit_json_line,
        handle_background_event, handle_item_completed, normalize_resume_session_id,
        read_codex_terminal_input_lines,
    };
    use crate::services::codex::{
        CODEX_BACKGROUND_TASK_NOTIFICATION_ID, CODEX_BACKGROUND_TASK_NOTIFICATION_STATUS,
    };
    use crate::services::tmux_common::RotatingJsonlWriter;
    use serde_json::json;

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
    fn chunked_external_prompt_decoder_reassembles_base64_payload() {
        let mut decoder = ExternalPromptDecoder::default();

        assert_eq!(
            decoder
                .decode_line("__AGENTDESK_B64_CHUNK__:msg-1:0:2:bGluZTEK")
                .unwrap(),
            None
        );
        assert_eq!(
            decoder
                .decode_line("__AGENTDESK_B64_CHUNK__:msg-1:1:2:bGluZTI=")
                .unwrap(),
            Some("line1\nline2".to_string())
        );
    }

    #[test]
    fn chunked_external_prompt_decoder_drops_malformed_chunks() {
        let mut decoder = ExternalPromptDecoder::default();

        assert!(
            decoder
                .decode_line("__AGENTDESK_B64_CHUNK__:msg-1:2:2:abc")
                .is_err()
        );
    }

    #[test]
    fn test_normalize_resume_session_id_trims_blank_values() {
        assert_eq!(
            normalize_resume_session_id(Some("  thread-1  ")),
            Some("thread-1".to_string())
        );
        assert_eq!(normalize_resume_session_id(Some("   ")), None);
        assert_eq!(normalize_resume_session_id(None), None);
    }

    #[test]
    fn terminal_input_reader_retries_after_eof_instead_of_exiting() {
        let (tx, rx) = std::sync::mpsc::channel();
        let reader = std::io::Cursor::new(" direct prompt \n\n");

        let outcome = read_codex_terminal_input_lines(reader, &tx);

        assert_eq!(outcome, TerminalInputLoopOutcome::RetryReader);
        assert_eq!(rx.try_recv().unwrap(), "direct prompt");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn terminal_input_reader_stops_when_consumer_is_gone() {
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        let reader = std::io::Cursor::new("direct prompt\n");

        let outcome = read_codex_terminal_input_lines(reader, &tx);

        assert_eq!(outcome, TerminalInputLoopOutcome::Stop);
    }

    #[test]
    fn emit_json_line_reopens_after_rotation_replacement() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();

        emit_json_line(&mut output, json!({"type":"assistant","message":"before"})).unwrap();

        let replacement = path.with_extension("jsonl.truncate.tmp");
        std::fs::write(
            &replacement,
            "{\"type\":\"assistant\",\"message\":\"kept\"}\n",
        )
        .unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        emit_json_line(&mut output, json!({"type":"assistant","message":"after"})).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let lines = content
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert!(lines.contains(&json!({"type":"assistant","message":"kept"})));
        assert!(lines.contains(&json!({"type":"assistant","message":"after"})));
    }

    #[test]
    fn handle_background_event_emits_task_notification_marker() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();

        handle_background_event(
            &mut output,
            &json!({"type":"background_event","message":"CI green"}),
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(
            value,
            json!({
                "type": "system",
                "subtype": "task_notification",
                "task_id": CODEX_BACKGROUND_TASK_NOTIFICATION_ID,
                "status": CODEX_BACKGROUND_TASK_NOTIFICATION_STATUS,
                "summary": "CI green",
                "task_notification_kind": "background",
            })
        );
    }

    #[test]
    fn handle_item_completed_redacts_reasoning_payload() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut final_text = String::new();

        handle_item_completed(
            &mut output,
            &json!({
                "type": "reasoning",
                "summary": [{ "type": "summary_text", "text": "internal reasoning" }]
            }),
            &mut final_text,
        )
        .unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("internal reasoning"));
        let value: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
        assert_eq!(value["message"]["content"][0]["type"], "thinking");
        assert!(value["message"]["content"][0].get("thinking").is_none());
    }
}

fn handle_item_started(
    output: &mut RotatingJsonlWriter,
    item: &serde_json::Value,
) -> Result<(), String> {
    if item.get("type").and_then(|v| v.as_str()) != Some("command_execution") {
        return Ok(());
    }

    let command = item.get("command").and_then(|v| v.as_str()).unwrap_or("");
    if command.is_empty() {
        return Ok(());
    }

    emit_json_line(
        output,
        serde_json::json!({
            "type": "assistant",
            "message": {
                "content": [{
                    "type": "tool_use",
                    "name": "Bash",
                    "input": {
                        "command": command,
                    }
                }]
            }
        }),
    )
}

fn handle_item_completed(
    output: &mut RotatingJsonlWriter,
    item: &serde_json::Value,
    final_text: &mut String,
) -> Result<(), String> {
    match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "agent_message" => {
            let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
            if !text.is_empty() {
                if !final_text.is_empty() {
                    final_text.push_str("\n\n");
                }
                final_text.push_str(text);
                emit_json_line(
                    output,
                    serde_json::json!({
                        "type": "assistant",
                        "message": {
                            "content": [{
                                "type": "text",
                                "text": text,
                            }]
                        }
                    }),
                )?;
            }
        }
        "command_execution" => {
            let content = item
                .get("aggregated_output")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let is_error = item
                .get("exit_code")
                .and_then(|v| v.as_i64())
                .map(|code| code != 0)
                .unwrap_or(false);
            emit_json_line(
                output,
                serde_json::json!({
                    "type": "user",
                    "message": {
                        "content": [{
                            "type": "tool_result",
                            "content": content,
                            "is_error": is_error,
                        }]
                    }
                }),
            )?;
        }
        "reasoning" => {
            emit_json_line(output, assistant_redacted_thinking_event())?;
        }
        _ => {}
    }

    Ok(())
}

fn assistant_redacted_thinking_event() -> serde_json::Value {
    serde_json::json!({
        "type": "assistant",
        "message": {
            "content": [{
                "type": "thinking",
            }]
        }
    })
}

fn handle_background_event(
    output: &mut RotatingJsonlWriter,
    json: &serde_json::Value,
) -> Result<(), String> {
    let Some(summary) = codex_background_event_summary(json) else {
        return Ok(());
    };

    emit_json_line(
        output,
        serde_json::json!({
            "type": "system",
            "subtype": "task_notification",
            "task_id": CODEX_BACKGROUND_TASK_NOTIFICATION_ID,
            "status": CODEX_BACKGROUND_TASK_NOTIFICATION_STATUS,
            "summary": summary,
            "task_notification_kind": "background",
        }),
    )
}

fn emit_status(message: &str) {
    eprintln!("\x1b[90m{}\x1b[0m", message);
}

fn emit_result_error(output: &mut RotatingJsonlWriter, message: &str) {
    let _ = emit_json_line(
        output,
        serde_json::json!({
            "type": "result",
            "subtype": "error_during_execution",
            "is_error": true,
            "errors": [message],
        }),
    );
}

fn emit_json_line(
    output: &mut RotatingJsonlWriter,
    value: serde_json::Value,
) -> Result<(), String> {
    let line =
        serde_json::to_string(&value).map_err(|e| format!("serialize output line: {}", e))?;
    output
        .write_line(&line)
        .map_err(|e| format!("write output line: {}", e))?;
    render_for_terminal(&line);
    Ok(())
}
