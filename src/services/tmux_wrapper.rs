//! Wrapper subprocess module (bidirectional).
//!
//! When invoked via `agentdesk tmux-wrapper`, this module manages a Claude session.
//! It spawns Claude with `--input-format stream-json` and keeps stdin open for multi-turn.
//!
//! Supports two input modes:
//! - **Fifo** (default): Runs inside tmux. Reads external input from a named FIFO.
//!   Terminal input thread also active for interactive use via tmux attach.
//! - **Pipe**: Runs as a direct child process. Reads external input from stdin pipe.
//!   No terminal input thread (headless mode for ProcessBackend).
//!
//! Concurrent activities (fifo mode):
//! 1. **Output thread**: Reads Claude stdout → appends to output file + renders to terminal
//! 2. **Terminal input thread**: Reads user keyboard input → formats as stream-json → Claude stdin
//! 3. **External input thread**: Reads from input FIFO → writes to Claude stdin (pre-formatted)
//!
//! Concurrent activities (pipe mode):
//! 1. **Output thread**: Same as fifo mode
//! 2. **Pipe input thread**: Reads from process stdin → writes to Claude stdin (pre-formatted)

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::utils::format::safe_prefix;

/// Input mode for the wrapper subprocess.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum InputMode {
    /// Read external input from a named FIFO (default, used inside tmux)
    Fifo,
    /// Read external input from process stdin pipe (headless, for ProcessBackend)
    Pipe,
}

/// Entry point for the wrapper subprocess.
pub fn run(
    output_file: &str,
    input_fifo: &str,
    prompt_file: &str,
    working_dir: &str,
    claude_cmd: &[String],
    input_mode: InputMode,
) {
    // Banner
    let mode_label = match input_mode {
        InputMode::Fifo => "bidirectional",
        InputMode::Pipe => "pipe-mode",
    };
    eprintln!("\x1b[90m═══════════════════════════════════════════════════════\x1b[0m");
    eprintln!("\x1b[90m  AgentDesk Claude Session ({})\x1b[0m", mode_label);
    if input_mode == InputMode::Fifo {
        eprintln!("\x1b[90m  Type messages below when Claude is ready.\x1b[0m");
        eprintln!("\x1b[90m  Ctrl-B, D to detach\x1b[0m");
    }
    eprintln!("\x1b[90m═══════════════════════════════════════════════════════\x1b[0m");
    eprintln!();

    // Read initial prompt
    let prompt = match std::fs::read_to_string(prompt_file) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("\x1b[31mError reading prompt file: {}\x1b[0m", e);
            std::process::exit(1);
        }
    };
    // Clean up prompt file immediately
    let _ = std::fs::remove_file(prompt_file);

    if claude_cmd.is_empty() {
        eprintln!("\x1b[31mNo claude command specified\x1b[0m");
        std::process::exit(1);
    }

    let claude_bin = &claude_cmd[0];
    let claude_args = &claude_cmd[1..];

    // Expand ~ in working_dir (Rust's current_dir doesn't handle tilde)
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

    // Spawn Claude with piped stdin (kept open for multi-turn)
    let mut claude_command = Command::new(claude_bin);
    crate::services::platform::augment_exec_path(&mut claude_command, claude_bin);
    let mut child = match claude_command
        .args(claude_args)
        .current_dir(&expanded_dir)
        .env("CLAUDE_CODE_MAX_OUTPUT_TOKENS", "64000")
        .env("BASH_DEFAULT_TIMEOUT_MS", "86400000")
        .env("BASH_MAX_TIMEOUT_MS", "86400000")
        .env_remove("CLAUDECODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("\x1b[31mFailed to start Claude: {}\x1b[0m", e);
            std::process::exit(1);
        }
    };

    // Take stdin — keep it open for multi-turn via stream-json
    let claude_stdin = match child.stdin.take() {
        Some(s) => Arc::new(Mutex::new(s)),
        None => {
            eprintln!("\x1b[31mFailed to capture Claude stdin\x1b[0m");
            std::process::exit(1);
        }
    };

    // Send initial prompt as stream-json
    {
        let msg = serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": prompt
            }
        });
        let mut stdin = claude_stdin.lock().unwrap();
        if writeln!(stdin, "{}", msg).is_err() || stdin.flush().is_err() {
            eprintln!("\x1b[31mFailed to send initial prompt\x1b[0m");
            std::process::exit(1);
        }
    }
    eprintln!("\x1b[90m[prompt sent]\x1b[0m");

    // Take stdout
    let stdout = match child.stdout.take() {
        Some(s) => s,
        None => {
            eprintln!("\x1b[31mFailed to capture stdout\x1b[0m");
            std::process::exit(1);
        }
    };

    // Shared state
    let claude_exited = Arc::new(AtomicBool::new(false));
    let ready_for_input = Arc::new(AtomicBool::new(false));

    // === Thread 1: Output — read Claude stdout → output file + terminal ===
    let output_file_path = output_file.to_string();
    let exited_t1 = claude_exited.clone();
    let ready_t1 = ready_for_input.clone();
    let output_thread = std::thread::spawn(move || {
        let mut out_file = match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&output_file_path)
        {
            Ok(f) => f,
            Err(e) => {
                eprintln!("\x1b[31mFailed to open output file: {}\x1b[0m", e);
                return;
            }
        };

        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };

            if line.trim().is_empty() {
                continue;
            }

            // Append to output file
            if writeln!(out_file, "{}", line).is_err() {
                break;
            }
            let _ = out_file.flush();

            // Check if this is a "result" event (turn complete)
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&line) {
                if json.get("type").and_then(|v| v.as_str()) == Some("result") {
                    ready_t1.store(true, Ordering::Relaxed);
                    // Detect fatal startup errors (e.g. auth failure).
                    // If Claude reports is_error with zero cost, it failed before
                    // making any API call — no point keeping the session alive.
                    let is_error = json
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let cost = json
                        .get("total_cost_usd")
                        .and_then(|v| v.as_f64())
                        .unwrap_or(1.0);
                    if is_error && cost == 0.0 {
                        eprintln!("\x1b[31m[fatal startup error — session will exit]\x1b[0m");
                        break;
                    }
                }
            }

            // Render to terminal
            render_for_terminal(&line);
        }

        exited_t1.store(true, Ordering::Relaxed);
    });

    // === Thread 1b: Stderr monitor — detect auth errors and write synthetic result ===
    let stderr = child.stderr.take();
    let output_file_for_stderr = output_file.to_string();
    let exited_stderr = claude_exited.clone();
    let _stderr_thread = std::thread::spawn(move || {
        let Some(stderr) = stderr else { return };
        let reader = BufReader::new(stderr);
        let mut collected = String::new();
        for line in reader.lines() {
            if exited_stderr.load(Ordering::Relaxed) {
                break;
            }
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            eprintln!("\x1b[90m[stderr] {}\x1b[0m", line);
            collected.push_str(&line);
            collected.push('\n');

            let lower = line.to_lowercase();
            if lower.contains("not logged in")
                || lower.contains("please run /login")
                || lower.contains("unauthorized")
                || lower.contains("authentication")
                || lower.contains("oauth")
                || lower.contains("token expired")
                || lower.contains("invalid api key")
                || lower.contains("api key")
                    && (lower.contains("missing")
                        || lower.contains("invalid")
                        || lower.contains("expired"))
            {
                // Write a synthetic error result to the output file so the watcher
                // can detect it and stop the spinner.
                let err_event = serde_json::json!({
                    "type": "result",
                    "is_error": true,
                    "result": format!("Authentication error: {}", line.trim()),
                    "total_cost_usd": 0.0,
                });
                if let Ok(mut f) = std::fs::OpenOptions::new()
                    .append(true)
                    .open(&output_file_for_stderr)
                {
                    let _ = writeln!(f, "{}", err_event);
                    let _ = f.flush();
                }
                eprintln!("\x1b[31m[auth error detected — wrote synthetic result]\x1b[0m");
                break;
            }
        }
    });

    // === Thread 2: Terminal input — read user typing → Claude stdin ===
    // Only active in Fifo mode (interactive tmux session).
    // In Pipe mode, stdin is used for external input (Thread 3), so this thread is skipped.
    if input_mode == InputMode::Fifo {
        let stdin_t2 = claude_stdin.clone();
        let exited_t2 = claude_exited.clone();
        let ready_t2 = ready_for_input.clone();
        let _terminal_thread = std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let reader = BufReader::new(stdin.lock());

            for line in reader.lines() {
                if exited_t2.load(Ordering::Relaxed) {
                    break;
                }

                let line = match line {
                    Ok(l) => l,
                    Err(_) => break,
                };

                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }

                // Mark as not ready (new turn starting)
                ready_t2.store(false, Ordering::Relaxed);

                // Format as stream-json
                let msg = serde_json::json!({
                    "type": "user",
                    "message": {
                        "role": "user",
                        "content": trimmed
                    }
                });

                eprintln!("\x1b[90m[sending...]\x1b[0m");

                let mut stdin = stdin_t2.lock().unwrap();
                if writeln!(stdin, "{}", msg).is_err() || stdin.flush().is_err() {
                    break;
                }
            }
        });
    }

    // === Thread 3: External input → Claude stdin ===
    // Fifo mode: reads from named FIFO (Discord writes to FIFO)
    // Pipe mode: reads from process stdin (parent writes to child stdin pipe)
    let stdin_t3 = claude_stdin.clone();
    let exited_t3 = claude_exited.clone();
    let ready_t3 = ready_for_input.clone();
    let input_fifo_path = input_fifo.to_string();
    let _external_thread = std::thread::spawn(move || {
        let reader: BufReader<Box<dyn std::io::Read + Send>> = match input_mode {
            InputMode::Fifo => {
                // Open FIFO with O_RDWR to prevent blocking on open and avoid EOF when no writer
                let fifo = match std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(&input_fifo_path)
                {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("\x1b[90m[input fifo error: {}]\x1b[0m", e);
                        return;
                    }
                };
                BufReader::new(Box::new(fifo))
            }
            InputMode::Pipe => {
                // Read from process stdin (parent writes follow-up messages here)
                BufReader::new(Box::new(std::io::stdin()))
            }
        };

        for line in reader.lines() {
            if exited_t3.load(Ordering::Relaxed) {
                break;
            }

            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };

            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            // Mark as not ready (new turn starting)
            ready_t3.store(false, Ordering::Relaxed);

            // Write directly to Claude stdin (already stream-json formatted from parent)
            eprintln!("\x1b[90m[external message received]\x1b[0m");

            let mut stdin = stdin_t3.lock().unwrap();
            if writeln!(stdin, "{}", trimmed).is_err() || stdin.flush().is_err() {
                break;
            }
        }
    });

    // Wait for output thread (which blocks until Claude exits or detects fatal error)
    let _ = output_thread.join();

    // Collect exit status before kill — if child already exited, this captures the real reason
    let exit_reason = match child.try_wait() {
        Ok(Some(status)) => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(sig) = status.signal() {
                    format!("signal:{sig}")
                } else {
                    format!("exit:{}", status.code().unwrap_or(-1))
                }
            }
            #[cfg(not(unix))]
            {
                format!("exit:{}", status.code().unwrap_or(-1))
            }
        }
        Ok(None) => "still_running".to_string(),
        Err(e) => format!("wait_error:{e}"),
    };

    // Kill Claude AND all its descendants (cmd.exe, bash, etc.).
    // Using kill_child_tree() instead of child.kill() ensures that child processes
    // spawned by Claude (e.g. cmd.exe on Windows, bash on Unix) are also terminated.
    // Without this, those descendants survive as orphan processes.
    crate::services::process::kill_child_tree(&mut child);

    // Write exit reason file for recovery diagnostics
    let exit_reason_path = format!("{}.exit_reason", output_file);
    let _ = std::fs::write(&exit_reason_path, &exit_reason);
    eprintln!("\x1b[90m[exit reason: {exit_reason}]\x1b[0m");

    // Only clean up output/FIFO if exit was normal (exit:0).
    // Abnormal exits preserve files for post-mortem analysis by dcserver recovery.
    if exit_reason == "exit:0" {
        let _ = std::fs::remove_file(output_file);
        let _ = std::fs::remove_file(input_fifo);
    } else {
        eprintln!("\x1b[33m[preserving output files for post-mortem: {output_file}]\x1b[0m");
    }

    eprintln!();
    eprintln!("\x1b[90m--- Session ended ({exit_reason}) ---\x1b[0m");
}

/// Extract a short human-readable detail from a tool_use content block.
fn format_tool_detail(name: &str, item: &serde_json::Value) -> String {
    let input = match item.get("input") {
        Some(v) => v,
        None => return String::new(),
    };
    match name {
        "Bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let desc = input
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !desc.is_empty() {
                let truncated = safe_prefix(cmd, 120);
                format!("{}: `{}`", desc, truncated)
            } else if !cmd.is_empty() {
                let truncated = safe_prefix(cmd, 150);
                format!("`{}`", truncated)
            } else {
                String::new()
            }
        }
        "Read" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Write" => {
            let fp = input
                .get("file_path")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let lines = input
                .get("content")
                .and_then(|v| v.as_str())
                .map(|c| c.lines().count())
                .unwrap_or(0);
            if lines > 0 {
                format!("{} ({} lines)", fp, lines)
            } else {
                fp.to_string()
            }
        }
        "Edit" => input
            .get("file_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Glob" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if !path.is_empty() {
                format!("{} in {}", pattern, path)
            } else {
                pattern.to_string()
            }
        }
        "Grep" => {
            let pattern = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("");
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if !path.is_empty() {
                format!("\"{}\" in {}", pattern, path)
            } else {
                format!("\"{}\"", pattern)
            }
        }
        "WebSearch" => input
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "WebFetch" => input
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        "Agent" => {
            let desc = input
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let agent_type = input
                .get("subagent_type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !agent_type.is_empty() {
                format!("[{}] {}", agent_type, desc)
            } else {
                desc.to_string()
            }
        }
        "Skill" => input
            .get("skill")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// Render a stream-json line as human-readable terminal output.
pub(crate) fn render_for_terminal(json_line: &str) {
    let json: serde_json::Value = match serde_json::from_str(json_line) {
        Ok(v) => v,
        Err(_) => return,
    };

    let msg_type = json.get("type").and_then(|v| v.as_str()).unwrap_or("");

    match msg_type {
        "system" => {
            let subtype = json.get("subtype").and_then(|v| v.as_str()).unwrap_or("");
            if subtype == "init" {
                if let Some(sid) = json.get("session_id").and_then(|v| v.as_str()) {
                    eprintln!("\x1b[90m[session: {}]\x1b[0m", sid);
                }
            }
        }
        "assistant" => {
            if let Some(content) = json
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for item in content {
                    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    match item_type {
                        "text" => {
                            if let Some(text) = item.get("text").and_then(|v| v.as_str()) {
                                println!("{}", text);
                            }
                        }
                        "tool_use" => {
                            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                            let detail = format_tool_detail(name, item);
                            if detail.is_empty() {
                                eprintln!("\x1b[36m[{}]\x1b[0m", name);
                            } else {
                                eprintln!("\x1b[36m[{}]\x1b[0m \x1b[90m{}\x1b[0m", name, detail);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        "user" => {
            if let Some(content) = json
                .get("message")
                .and_then(|m| m.get("content"))
                .and_then(|c| c.as_array())
            {
                for item in content {
                    if item.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                        let is_error = item
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        if is_error {
                            eprintln!("\x1b[31m[tool error]\x1b[0m");
                        }
                    }
                }
            }
        }
        "result" => {
            let is_error = json
                .get("is_error")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if is_error {
                if let Some(errors) = json.get("errors").and_then(|v| v.as_array()) {
                    for e in errors {
                        if let Some(s) = e.as_str() {
                            eprintln!("\x1b[31m{}\x1b[0m", s);
                        }
                    }
                }
            } else {
                let cost = json.get("total_cost_usd").and_then(|v| v.as_f64());
                let duration = json.get("duration_ms").and_then(|v| v.as_u64());
                if let (Some(c), Some(d)) = (cost, duration) {
                    eprintln!(
                        "\x1b[90m[cost: ${:.4}, duration: {:.1}s]\x1b[0m",
                        c,
                        d as f64 / 1000.0
                    );
                }
                // Prompt indicator for user
                eprintln!();
                eprintln!("\x1b[32m▶\x1b[0m \x1b[90mReady for input (type message + Enter)\x1b[0m");
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::format_tool_detail;
    use crate::utils::format::safe_prefix;
    use serde_json::json;

    #[test]
    fn format_tool_detail_truncates_bash_with_description_on_char_boundary() {
        let cmd = format!("echo {}", "한".repeat(50));
        let item = json!({
            "input": {
                "command": cmd,
                "description": "한글 설명"
            }
        });

        let detail = format_tool_detail("Bash", &item);
        let expected = safe_prefix(item["input"]["command"].as_str().unwrap(), 120);

        assert_eq!(detail, format!("한글 설명: `{}`", expected));
    }

    #[test]
    fn format_tool_detail_truncates_bash_without_description_on_char_boundary() {
        let cmd = format!("printf {}", "한".repeat(80));
        let item = json!({
            "input": {
                "command": cmd
            }
        });

        let detail = format_tool_detail("Bash", &item);
        let expected = safe_prefix(item["input"]["command"].as_str().unwrap(), 150);

        assert_eq!(detail, format!("`{}`", expected));
    }
}
