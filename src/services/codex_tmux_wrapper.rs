use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc;

use crate::services::codex::{
    CODEX_BACKGROUND_TASK_NOTIFICATION_ID, CODEX_BACKGROUND_TASK_NOTIFICATION_STATUS,
    codex_background_event_summary,
};
use crate::services::tmux_common::RotatingJsonlWriter;
use crate::services::tmux_wrapper::{InputMode, render_for_terminal};

const TMUX_PROMPT_B64_PREFIX: &str = "__AGENTDESK_B64__:";

pub fn run(
    output_file: &str,
    input_fifo: &str,
    prompt_file: &str,
    working_dir: &str,
    codex_bin: &str,
    codex_model: Option<&str>,
    reasoning_effort: Option<&str>,
    resume_session_id: Option<&str>,
    fast_mode_enabled: Option<bool>,
    input_mode: InputMode,
    compact_token_limit: Option<u64>,
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

    // Terminal input — only in Fifo mode (interactive tmux session)
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

    // External input
    // Fifo mode: reads from named FIFO
    // Pipe mode: reads from process stdin (parent writes to child stdin pipe)
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
    if let Err(err) = run_turn(
        &mut output,
        codex_bin,
        codex_model,
        reasoning_effort,
        &expanded_dir,
        &prompt,
        &mut thread_id,
        fast_mode_enabled,
        compact_token_limit,
    ) {
        emit_result_error(&mut output, &err);
        let exit_reason_path = format!("{}.exit_reason", output_file);
        let _ = std::fs::write(&exit_reason_path, format!("error:{err}"));
        // Preserve output files for post-mortem on error
        eprintln!("\x1b[33m[preserving output files for post-mortem: {output_file}]\x1b[0m");
        std::process::exit(1);
    }

    let mut followup_error: Option<String> = None;
    while let Ok(next_prompt) = prompt_rx.recv() {
        if let Err(err) = run_turn(
            &mut output,
            codex_bin,
            codex_model,
            reasoning_effort,
            &expanded_dir,
            next_prompt.trim(),
            &mut thread_id,
            fast_mode_enabled,
            compact_token_limit,
        ) {
            emit_result_error(&mut output, &err);
            followup_error = Some(err);
            break;
        }
    }

    let exit_reason_path = format!("{}.exit_reason", output_file);
    let exit_reason = if let Some(ref err) = followup_error {
        // Follow-up turn failed — preserve files for post-mortem (same as initial turn)
        let reason = format!("error:{err}");
        let _ = std::fs::write(&exit_reason_path, &reason);
        eprintln!("\x1b[33m[preserving output files for post-mortem: {output_file}]\x1b[0m");
        reason
    } else {
        // Normal exit — prompt_rx closed, all turns succeeded
        let reason = "exit:0".to_string();
        let _ = std::fs::write(&exit_reason_path, &reason);
        cleanup(output_file, input_fifo);
        reason
    };
    eprintln!();
    eprintln!("\x1b[90m--- Session ended ({exit_reason}) ---\x1b[0m");
}

fn normalize_resume_session_id(resume_session_id: Option<&str>) -> Option<String> {
    resume_session_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
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

fn cleanup(output_file: &str, input_fifo: &str) {
    let _ = std::fs::remove_file(output_file);
    let _ = std::fs::remove_file(input_fifo);
}

fn run_turn(
    output: &mut RotatingJsonlWriter,
    codex_bin: &str,
    codex_model: Option<&str>,
    reasoning_effort: Option<&str>,
    working_dir: &str,
    prompt: &str,
    thread_id: &mut Option<String>,
    fast_mode_enabled: Option<bool>,
    compact_token_limit: Option<u64>,
) -> Result<(), String> {
    emit_status("[sending...]");

    let mut args = Vec::new();
    if let Some(model) = codex_model.map(str::trim).filter(|value| !value.is_empty()) {
        let effort = reasoning_effort
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .unwrap_or("high");
        args.push("-c".to_string());
        args.push(format!(r#"model_reasoning_effort="{}""#, effort));
        args.push("-m".to_string());
        args.push(model.to_string());
    }
    if let Some(limit) = compact_token_limit.filter(|&l| l > 0) {
        args.push("-c".to_string());
        args.push(format!("model_auto_compact_token_limit={}", limit));
    }
    if let Some(enabled) = fast_mode_enabled {
        args.push(if enabled {
            "--enable".to_string()
        } else {
            "--disable".to_string()
        });
        args.push("fast_mode".to_string());
    }
    args.push("exec".to_string());
    if let Some(existing_thread_id) = thread_id.as_deref() {
        args.push("resume".to_string());
        args.push(existing_thread_id.to_string());
    }
    args.extend([
        "--skip-git-repo-check".to_string(),
        "--json".to_string(),
        "--dangerously-bypass-approvals-and-sandbox".to_string(),
        prompt.to_string(),
    ]);

    let mut cmd = Command::new(codex_bin);
    crate::services::platform::augment_exec_path(&mut cmd, codex_bin);
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
    let mut reader = BufReader::new(stdout);
    let mut stdout_line = String::new();
    let mut final_text = String::new();
    let start = std::time::Instant::now();
    let mut saw_turn_completed = false;

    loop {
        stdout_line.clear();
        let read = reader
            .read_line(&mut stdout_line)
            .map_err(|e| format!("Failed to read Codex output: {}", e))?;
        if read == 0 {
            break;
        }

        let trimmed = stdout_line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) else {
            continue;
        };

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
                    handle_item_completed(output, item, &mut final_text)?;
                }
            }
            "background_event" => {
                handle_background_event(output, &json)?;
            }
            "turn.completed" => {
                let usage = json.get("usage").cloned().unwrap_or_default();
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
                        "result": final_text,
                        "session_id": thread_id.as_deref(),
                        "duration_ms": start.elapsed().as_millis() as u64,
                        "input_tokens": input_tokens,
                        "output_tokens": output_tokens,
                    }),
                )?;
                saw_turn_completed = true;
            }
            "error" => {
                let message = json
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown Codex error");
                emit_result_error(output, message);
                saw_turn_completed = true;
            }
            _ => {}
        }
    }

    // Kill Codex process tree (including any cmd.exe / bash children) before waiting.
    // Without this, child processes spawned by Codex survive as orphan processes.
    crate::services::process::kill_pid_tree(child_pid);
    std::thread::sleep(std::time::Duration::from_millis(200));

    let wait = child
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for Codex: {}", e))?;

    if !wait.status.success() && !saw_turn_completed {
        let stderr = String::from_utf8_lossy(&wait.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("Codex exited with code {:?}", wait.status.code())
        } else {
            stderr
        };
        emit_result_error(output, &message);
        return Err(message);
    }

    if !saw_turn_completed {
        emit_json_line(
            output,
            serde_json::json!({
                "type": "result",
                "subtype": "success",
                "result": final_text,
                "session_id": thread_id.as_deref(),
                "duration_ms": start.elapsed().as_millis() as u64,
            }),
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        decode_external_prompt, emit_json_line, handle_background_event, handle_item_completed,
        normalize_resume_session_id,
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
    fn test_normalize_resume_session_id_trims_blank_values() {
        assert_eq!(
            normalize_resume_session_id(Some("  thread-1  ")),
            Some("thread-1".to_string())
        );
        assert_eq!(normalize_resume_session_id(Some("   ")), None);
        assert_eq!(normalize_resume_session_id(None), None);
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
