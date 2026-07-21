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
/// #3557 (B): idle window after the first event. Once Codex has emitted at
/// least one event, the run loop previously blocked on `recv()` forever, so a
/// Codex process that hung mid-turn (tool/API hang) without emitting another
/// JSON event and without exiting kept the watcher believing the pane was busy
/// — the direct cause of the 13125s outlier. We now bound inter-event silence.
///
/// #3557 (B) Codex-review r2 fix: this wrapper runs `codex exec` as a direct
/// child process and reads its JSON event stream over an OS pipe — there is NO
/// tmux pane to `capture-pane`, so the "pane liveness" check is not available
/// on this path. The only liveness signal IS the JSON stream, which `recv` here
/// already measures. A single long SILENT tool run (e.g. a multi-minute
/// `cargo build` issued through a shell tool) emits the tool-call-start event,
/// then nothing until the tool returns — so the idle window must clear the
/// longest plausible single tool execution, not merely a typical reasoning gap.
/// We therefore raise the generous default to 3600s (was 1800s) so a normal
/// long-running tool run is never mistaken for an idle hang. The 4h hard ceiling
/// (`DEFAULT_CODEX_TURN_HARD_CEILING_SECS`) is the real backstop: idle-kill only
/// trips on a Codex that is BOTH silent on its event stream AND not exiting, and
/// even then the ceiling guarantees termination regardless of idle tuning.
const DEFAULT_CODEX_TURN_IDLE_RECV_SECS: u64 = 3600;
/// #3557 (B): absolute wall-clock ceiling for a single Codex `exec` turn,
/// measured from process spawn. A hung Codex that *does* keep dribbling events
/// would slip past the idle window, so this is the hard backstop. Default 4h
/// matches the per-turn Codex ceiling and clears any legitimate Codex turn.
const DEFAULT_CODEX_TURN_HARD_CEILING_SECS: u64 = 4 * 3600;

#[allow(clippy::too_many_arguments)]
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

fn codex_first_event_timeout() -> std::time::Duration {
    let seconds = std::env::var("AGENTDESK_CODEX_FIRST_EVENT_TIMEOUT_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_CODEX_FIRST_EVENT_TIMEOUT_SECS);
    std::time::Duration::from_secs(seconds)
}

/// #3557 (B): idle inter-event recv timeout after the first event. Override via
/// `AGENTDESK_CODEX_TURN_IDLE_RECV_SECS`.
fn codex_turn_idle_recv_timeout() -> std::time::Duration {
    let seconds = std::env::var("AGENTDESK_CODEX_TURN_IDLE_RECV_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_CODEX_TURN_IDLE_RECV_SECS);
    std::time::Duration::from_secs(seconds)
}

/// #3557 (B): absolute per-turn ceiling for a Codex `exec` run. Override via
/// `AGENTDESK_CODEX_TURN_HARD_CEILING_SECS` (shared with the orchestrator-side
/// auto-extend ceiling so a single knob bounds the Codex turn end to end).
fn codex_turn_hard_ceiling() -> std::time::Duration {
    let seconds = std::env::var("AGENTDESK_CODEX_TURN_HARD_CEILING_SECS")
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(DEFAULT_CODEX_TURN_HARD_CEILING_SECS);
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

#[allow(clippy::too_many_arguments)]
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
    // #3557 (B): bound a turn that hung after its first event (idle window) and a
    // turn that drips events forever (absolute ceiling). Both kill the Codex
    // process tree so the turn rejoins the normal error path instead of looking
    // "busy" to the watcher indefinitely.
    let idle_recv_timeout = codex_turn_idle_recv_timeout();
    let hard_ceiling = codex_turn_hard_ceiling();

    loop {
        // Absolute wall-clock ceiling, checked every iteration so even a Codex
        // that keeps emitting events past the limit is terminated.
        if start.elapsed() >= hard_ceiling {
            crate::services::process::kill_pid_tree(child_pid);
            let _ = child.wait();
            return Err(format!(
                "Codex turn exceeded hard ceiling of {}s",
                hard_ceiling.as_secs()
            ));
        }

        let next_line = if saw_any_stdout {
            // After the first event, bound inter-event silence: a hung Codex that
            // stops emitting without exiting used to block here forever.
            //
            // #3557 (B) Codex-review fix: cap the idle recv by the REMAINING
            // ceiling budget. Previously the loop entered `recv_timeout` with the
            // full idle window (1800s) regardless of how close the run was to the
            // hard ceiling, so an event at 3h59m followed by a hang killed Codex
            // only at 4h29m (ceiling + idle). Clamping the wait to the ceiling
            // remainder keeps the absolute ceiling honored even mid-recv; when no
            // budget remains we kill immediately on the next loop iteration via
            // the `start.elapsed() >= hard_ceiling` check at the top.
            let elapsed = start.elapsed();
            let ceiling_remaining = hard_ceiling.saturating_sub(elapsed);
            if ceiling_remaining.is_zero() {
                crate::services::process::kill_pid_tree(child_pid);
                let _ = child.wait();
                return Err(format!(
                    "Codex turn exceeded hard ceiling of {}s",
                    hard_ceiling.as_secs()
                ));
            }
            let wait = idle_recv_timeout.min(ceiling_remaining);
            match stdout_rx.recv_timeout(wait) {
                Ok(line) => line,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // A timeout here is either the idle window or the ceiling
                    // remainder elapsing; the top-of-loop ceiling check would
                    // also catch the latter, but kill now to avoid one more
                    // idle wait. Report the cause for diagnosis.
                    crate::services::process::kill_pid_tree(child_pid);
                    let _ = child.wait();
                    if wait < idle_recv_timeout {
                        return Err(format!(
                            "Codex turn exceeded hard ceiling of {}s (idle recv capped by ceiling remainder)",
                            hard_ceiling.as_secs()
                        ));
                    }
                    // NOTE: this path runs `codex exec` over a pipe (no tmux
                    // pane), so we cannot confirm pane activity before killing —
                    // the JSON event stream is the only liveness signal we have.
                    // A genuinely busy-but-silent tool run is covered by the
                    // generous idle window; the 4h hard ceiling is the backstop.
                    return Err(format!(
                        "Codex produced no JSON event for {}s (idle hang; no tmux pane to confirm activity — relied on JSON-stream liveness, hard ceiling is the backstop)",
                        idle_recv_timeout.as_secs()
                    ));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("Codex stdout reader disconnected".to_string());
                }
            }
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
        // The stream closed without a recognized terminal event. If schema drift
        // dropped the body, synthesizing a success here is the #3027 blackhole;
        // fail closed instead so the turn surfaces an error/retry.
        if let Some(reason) = schema_drift_reason(&state) {
            emit_schema_drift_result(output, &mut state, &reason);
        } else {
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
    }

    Ok(())
}

/// #3275: one Codex API call's token usage, parsed from `token_count`
/// (`info.last_token_usage`) or a terminal event's own per-turn `usage`.
#[derive(Clone, Copy)]
struct CodexCallTokenUsage {
    input_tokens: u64,
    cached_input_tokens: u64,
    output_tokens: u64,
}

impl CodexCallTokenUsage {
    fn from_value(value: &serde_json::Value) -> Option<Self> {
        // Guard: at least one known token field must actually be present. An
        // empty/unrelated object (e.g. a protocol variant sending
        // `last_token_usage: {}`) parses to None so it cannot clobber a
        // previously captured real usage with all zeros, and the result-frame
        // `or_else` fallback chain keeps working.
        const KNOWN_FIELDS: [&str; 3] = ["input_tokens", "cached_input_tokens", "output_tokens"];
        if !KNOWN_FIELDS.iter().any(|key| value.get(key).is_some()) {
            return None;
        }
        let field = |key: &str| value.get(key).and_then(|v| v.as_u64()).unwrap_or(0);
        Some(Self {
            input_tokens: field("input_tokens"),
            cached_input_tokens: field("cached_input_tokens"),
            output_tokens: field("output_tokens"),
        })
    }
}

#[derive(Default)]
struct CodexWrapperTurnState {
    final_text: String,
    saw_turn_completed: bool,
    // #3275: per-call context occupancy from the latest `token_count` event
    // (`info.last_token_usage` only — never the session-cumulative
    // `info.total_token_usage`), re-emitted as the result frame's nested
    // Claude-compatible `usage` so watcher/bridge token persistence fires.
    last_token_usage: Option<CodexCallTokenUsage>,
    // Diagnostic-only count of every unrecognized event (top-level / event_msg /
    // response_item), benign or not, used for the ignored-event log.
    unknown_event_count: u64,
    // #3027: set only when an UNRECOGNIZED event carried assistant-content shape
    // (role=assistant, or a non-empty text/message/content body). This — not the
    // broad `unknown_event_count` — is what signals schema drift dropped the
    // answer, so benign progress/lifecycle unknowns on a legitimately empty or
    // tool-only turn do not trip the fail-closed path.
    saw_dropped_assistant_content: bool,
}

/// True when an unrecognized Codex event has the *shape* of an assistant
/// message body (vs a benign lifecycle/progress/status event). If an event with
/// this shape was dropped, the answer was lost (#3027), so it must trip the
/// fail-closed path.
///
/// To avoid both misses and false positives we:
///  - unwrap the common `payload`/`item` envelopes, since drift may rename the
///    outer event type while keeping the assistant message nested inside; and
///  - require *assistant/message context* before trusting a text-bearing
///    `content` array — a bare `content`/`message`/`text` field on a tool or
///    status payload is NOT treated as a dropped answer.
fn value_carries_assistant_content(value: &serde_json::Value) -> bool {
    // Direct assistant message shape (role=assistant, possibly with content).
    if object_is_assistant_message(value) {
        return true;
    }
    // Drift may keep the assistant message nested under a known wrapper key while
    // only the outer event type changed. Inspect one envelope level down.
    for key in ["payload", "item", "message", "response_item"] {
        if let Some(inner) = value.get(key) {
            if object_is_assistant_message(inner) {
                return true;
            }
        }
    }
    false
}

/// True when `value` is an assistant message envelope carrying non-empty body
/// text: either `role == "assistant"`, or `type == "message"` with a
/// text-bearing `content` array. A bare `content` array without that context
/// (e.g. on a tool/status payload) does not qualify.
fn object_is_assistant_message(value: &serde_json::Value) -> bool {
    let role = value.get("role").and_then(|v| v.as_str());
    let is_message_type = value.get("type").and_then(|v| v.as_str()) == Some("message");
    if role != Some("assistant") && !is_message_type {
        return false;
    }
    // An explicit non-assistant role (e.g. a user echo `role: "user"` wrapped in
    // a renamed `type: "message"` envelope) is never dropped assistant output —
    // only assistant messages may trigger the fail-closed schema-drift path.
    if let Some(role) = role {
        return role == "assistant";
    }
    // type=message with text-bearing content.
    value
        .get("content")
        .and_then(|v| v.as_array())
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.is_object()
                    && item
                        .get("text")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.trim().is_empty())
            })
        })
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
                handle_response_item(output, payload, state)?;
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
            record_unknown_codex_event(output, state, event_type, json)?;
        }
    }

    Ok(())
}

fn handle_response_item(
    output: &mut RotatingJsonlWriter,
    payload: &serde_json::Value,
    state: &mut CodexWrapperTurnState,
) -> Result<(), String> {
    match payload.get("type").and_then(|v| v.as_str()).unwrap_or("") {
        "message" => handle_response_message(output, payload, state),
        "function_call" | "custom_tool_call" | "tool_search_call" => {
            handle_response_tool_call(output, payload)
        }
        "function_call_output" | "custom_tool_call_output" | "tool_search_output" => {
            handle_response_tool_output(output, payload)
        }
        "reasoning" => emit_json_line(output, assistant_redacted_thinking_event()),
        // #3027: an unrecognized response_item payload type (case c in the issue)
        // is fail-open the same way as an unknown top-level event. Route it through
        // the recorder so the offending type is logged and counted — otherwise a
        // dropped answer under a future response_item type finalizes as
        // schema_drift with unknown_event_count=0 and no diagnostic for the type.
        other => {
            let event_type = if other.is_empty() {
                "response_item.<missing-type>"
            } else {
                other
            };
            record_unknown_codex_event(output, state, event_type, payload)?;
            // The response_item envelope already establishes assistant-output
            // context, so a payload carrying body text is a drifted answer even
            // as a bare `{"type":"output_text","text":"…"}` without the
            // role/message wrapper the generic classifier requires. Mark it
            // dropped so a trailing bodyless task_complete fails closed.
            if response_item_payload_carries_text(payload) {
                state.saw_dropped_assistant_content = true;
            }
            Ok(())
        }
    }
}

/// Within a `response_item` envelope the surrounding context is already
/// assistant output, so any payload carrying body text is a drifted answer —
/// including a bare `{"type":"output_text","text":"…"}` that [`value_carries_assistant_content`]
/// rejects because it lacks a `role`/`type:"message"` wrapper.
fn response_item_payload_carries_text(payload: &serde_json::Value) -> bool {
    if value_carries_assistant_content(payload) {
        return true;
    }
    if payload
        .get("text")
        .and_then(|v| v.as_str())
        .is_some_and(|s| !s.trim().is_empty())
    {
        return true;
    }
    payload
        .get("content")
        .and_then(|v| v.as_array())
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.get("text")
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.trim().is_empty())
            })
        })
}

fn handle_response_message(
    output: &mut RotatingJsonlWriter,
    payload: &serde_json::Value,
    state: &mut CodexWrapperTurnState,
) -> Result<(), String> {
    if payload.get("role").and_then(|v| v.as_str()) != Some("assistant") {
        return Ok(());
    }
    let Some(content) = payload.get("content").and_then(|v| v.as_array()) else {
        return Ok(());
    };
    let include_in_final_text = payload.get("phase").and_then(|v| v.as_str()) != Some("commentary");
    // #3027 (case c): an assistant message envelope is recognized, but Codex may
    // rename the content item types we relay (`output_text`/`text`). Track
    // whether this message carried any non-empty text item we could NOT relay so
    // that, if nothing usable was emitted, finalization fails closed instead of
    // synthesizing an empty success.
    let mut relayed_text = false;
    let mut dropped_text = false;
    for item in content {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if item_type != "output_text" && item_type != "text" {
            // Unrecognized content item shape: if it still carries body text,
            // the answer was dropped by content-type drift.
            if !text.trim().is_empty() {
                dropped_text = true;
            }
            continue;
        }
        if text.is_empty() {
            continue;
        }
        relayed_text = true;
        if include_in_final_text {
            if !state.final_text.is_empty() {
                state.final_text.push_str("\n\n");
            }
            state.final_text.push_str(text);
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
    if dropped_text && !relayed_text {
        state.saw_dropped_assistant_content = true;
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
        "token_count" => {
            capture_last_token_usage(payload, state);
            Ok(())
        }
        "composer_ready" => Ok(()),
        event_type => record_unknown_codex_event(output, state, event_type, payload),
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
    // Fail-closed on schema drift: a terminal event that carries no body while
    // unknown events were observed means the assistant content was dropped by an
    // unrecognized schema. Emitting a success frame here reproduces the adk-cdx
    // blackhole (#3027). Surface it as an error so the turn retries instead of
    // silently completing with an empty answer.
    if let Some(reason) = schema_drift_reason(state) {
        emit_schema_drift_result(output, state, &reason);
        return Ok(());
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
    let mut result = serde_json::json!({
        "type": "result",
        "subtype": "success",
        "result": state.final_text,
        "session_id": thread_id.as_deref(),
        "duration_ms": start.elapsed().as_millis() as u64,
        "input_tokens": input_tokens,
        "output_tokens": output_tokens,
    });
    // #3275: the shared result-frame token parsers (watcher
    // `process_watcher_lines`, bridge/recovery `process_stream_line`) read only
    // a nested Claude-shaped `usage` object, so emitting tokens solely as the
    // top-level fields above left watcher-owned codex turns with zero persisted
    // telemetry and an idle recap of "context unknown". Re-emit the per-call
    // occupancy via the receipt.rs subset convention (`input - cached` +
    // `cache_read = cached`, so `context_occupancy_input_tokens()` reconstructs
    // the original codex input). Sources, in order: the captured
    // `token_count.info.last_token_usage`, then the terminal event's own
    // per-turn `usage` (codex exec protocol). The session-cumulative
    // `info.total_token_usage` is never used here, and with no usable source
    // the nested object is omitted entirely (fail-safe: recap stays Unknown
    // rather than fabricated). Top-level fields keep their legacy chain.
    let call_usage = state
        .last_token_usage
        .or_else(|| json.get("usage").and_then(CodexCallTokenUsage::from_value));
    if let Some(call) = call_usage.filter(|call| call.input_tokens > 0) {
        result["usage"] = serde_json::json!({
            "input_tokens": call.input_tokens.saturating_sub(call.cached_input_tokens),
            "cache_read_input_tokens": call.cached_input_tokens,
            "output_tokens": call.output_tokens,
        });
    }
    emit_json_line(output, result)?;
    state.saw_turn_completed = true;
    Ok(())
}

/// #3275: record the per-call context occupancy from a `token_count` event.
/// Only `info.last_token_usage` qualifies; the sibling `info.total_token_usage`
/// is the session-cumulative count (8.3M in the issue report) and persisting it
/// would render a wildly inflated context % in the idle recap. Absent payloads
/// leave prior state untouched.
fn capture_last_token_usage(payload: &serde_json::Value, state: &mut CodexWrapperTurnState) {
    if let Some(usage) = payload
        .get("info")
        .and_then(|info| info.get("last_token_usage"))
        .and_then(CodexCallTokenUsage::from_value)
    {
        state.last_token_usage = Some(usage);
    }
}

/// Decide whether a turn is finalizing in a fail-open schema-drift state.
///
/// Returns a human-readable reason when the turn has no assistant body to relay
/// (`final_text` empty) *and* an unrecognized event that looked like it carried
/// the assistant body was dropped (`saw_dropped_assistant_content`). That
/// combination means the answer was dropped by a Codex schema we no longer
/// parse, so a "success" frame would silently blackhole the turn (#3027).
///
/// Crucially this does NOT trip on the broad `unknown_event_count`: a legitimate
/// tool-only or intentionally empty turn can emit benign unknown progress or
/// lifecycle events without losing any answer, and those must still finalize as
/// success. When the body is present, or only benign drift was observed, returns
/// `None` and the caller proceeds with the normal success path.
fn schema_drift_reason(state: &CodexWrapperTurnState) -> Option<String> {
    if state.final_text.is_empty() && state.saw_dropped_assistant_content {
        Some(format!(
            "Codex turn produced no relayable body after dropping unrecognized \
             assistant-content event(s) (unknown_event_count={}); treating schema \
             drift as an error instead of an empty success",
            state.unknown_event_count
        ))
    } else {
        None
    }
}

/// Emit a fail-closed terminal frame for schema drift and mark the turn complete.
///
/// Idempotent: a no-op once `saw_turn_completed` is set, so it is safe to call
/// from multiple finalization paths.
fn emit_schema_drift_result(
    output: &mut RotatingJsonlWriter,
    state: &mut CodexWrapperTurnState,
    reason: &str,
) {
    if state.saw_turn_completed {
        return;
    }
    eprintln!(
        "\x1b[91m[codex wrapper schema drift: {reason}; unknown_event_count={}]\x1b[0m",
        state.unknown_event_count
    );
    let _ = emit_json_line(
        output,
        serde_json::json!({
            "type": "result",
            "subtype": "schema_drift",
            "is_error": true,
            "errors": [reason],
            "source": "codex_tmux_wrapper",
            "unknown_event_count": state.unknown_event_count,
        }),
    );
    state.saw_turn_completed = true;
}

fn record_unknown_codex_event(
    output: &mut RotatingJsonlWriter,
    state: &mut CodexWrapperTurnState,
    event_type: &str,
    value: &serde_json::Value,
) -> Result<(), String> {
    state.unknown_event_count = state.unknown_event_count.saturating_add(1);
    // #3027: only an unknown event that carried assistant body text proves the
    // answer was dropped. Benign unknown lifecycle/progress events are counted
    // for diagnostics but must not trip the fail-closed finalization path.
    if value_carries_assistant_content(value) {
        state.saw_dropped_assistant_content = true;
    }
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
    use super::{
        CodexWrapperTurnState, emit_schema_drift_result, handle_codex_wrapper_event,
        schema_drift_reason,
    };
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
    fn modern_update_plan_function_call_emits_tool_use_event() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-modern".to_string());
        let mut state = CodexWrapperTurnState::default();

        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "update_plan",
                    "arguments": "{\"plan\":[{\"step\":\"Render Codex plan\",\"status\":\"in_progress\"}]}",
                    "call_id": "call-plan"
                }
            }),
            &mut thread_id,
            &mut state,
            std::time::Instant::now(),
        )
        .unwrap();

        let lines = read_jsonl(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "assistant");
        assert_eq!(lines[0]["message"]["content"][0]["type"], "tool_use");
        assert_eq!(lines[0]["message"]["content"][0]["name"], "update_plan");
        assert_eq!(
            lines[0]["message"]["content"][0]["input"],
            "{\"plan\":[{\"step\":\"Render Codex plan\",\"status\":\"in_progress\"}]}"
        );
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

    // #3275: the result frame must carry a Claude-compatible nested `usage`
    // built from the per-call `token_count.info.last_token_usage` (subset
    // convention: input - cached / cache_read = cached), because the shared
    // result-frame token parsers ignore the top-level fields. Top-level fields
    // keep their legacy chain (no usage/info on task_complete → 0).
    #[test]
    fn token_count_last_usage_feeds_nested_result_usage() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-tokens".to_string());
        let mut state = CodexWrapperTurnState::default();
        let start = std::time::Instant::now();

        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 1000,
                            "cached_input_tokens": 600,
                            "output_tokens": 50
                        }
                    }
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
                    "last_agent_message": "tokens final"
                }
            }),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();

        let lines = read_jsonl(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "result");
        assert_eq!(lines[0]["subtype"], "success");
        assert_eq!(lines[0]["usage"]["input_tokens"], 400);
        assert_eq!(lines[0]["usage"]["cache_read_input_tokens"], 600);
        assert_eq!(lines[0]["usage"]["output_tokens"], 50);
        assert_eq!(lines[0]["input_tokens"], 0);
        assert_eq!(lines[0]["output_tokens"], 0);
    }

    // #3275: a protocol variant sending `token_count` with an empty
    // `info.last_token_usage: {}` must not clobber a previously captured real
    // per-call usage with all zeros — the empty object parses to None, the
    // earlier capture is preserved, and the nested result usage still emits.
    #[test]
    fn empty_last_token_usage_object_preserves_prior_capture() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-empty-usage".to_string());
        let mut state = CodexWrapperTurnState::default();
        let start = std::time::Instant::now();

        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 1000,
                            "cached_input_tokens": 600,
                            "output_tokens": 50
                        }
                    }
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
                    "type": "token_count",
                    "info": { "last_token_usage": {} }
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
                    "last_agent_message": "empty-variant final"
                }
            }),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();

        let lines = read_jsonl(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "result");
        assert_eq!(lines[0]["subtype"], "success");
        assert_eq!(lines[0]["usage"]["input_tokens"], 400);
        assert_eq!(lines[0]["usage"]["cache_read_input_tokens"], 600);
        assert_eq!(lines[0]["usage"]["output_tokens"], 50);
    }

    // #3275: when the terminal event also carries the session-cumulative
    // `info.total_token_usage` (8.3M in the issue), the nested usage must stay
    // per-call (last_token_usage) so the persisted context % is not inflated,
    // while the top-level fields keep the legacy total chain unchanged.
    #[test]
    fn nested_result_usage_prefers_last_token_usage_over_cumulative_total() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-total".to_string());
        let mut state = CodexWrapperTurnState::default();
        let start = std::time::Instant::now();

        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "last_token_usage": {
                            "input_tokens": 1000,
                            "cached_input_tokens": 600,
                            "output_tokens": 50
                        },
                        "total_token_usage": {
                            "input_tokens": 8_325_687_u64,
                            "cached_input_tokens": 8_000_000_u64,
                            "output_tokens": 41_600
                        }
                    }
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
                    "last_agent_message": "total final",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 8_325_687_u64,
                            "output_tokens": 41_600
                        }
                    }
                }
            }),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();

        let lines = read_jsonl(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "result");
        assert_eq!(lines[0]["usage"]["input_tokens"], 400);
        assert_eq!(lines[0]["usage"]["cache_read_input_tokens"], 600);
        assert_eq!(lines[0]["usage"]["output_tokens"], 50);
        assert_eq!(lines[0]["input_tokens"], 8_325_687_u64);
        assert_eq!(lines[0]["output_tokens"], 41_600);
    }

    // #3275: codex exec protocol — no token_count events, but `turn.completed`
    // carries a per-turn usage object. It must map through the same subset
    // convention while the top-level fields keep the legacy values.
    #[test]
    fn turn_completed_usage_maps_to_nested_result_usage() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-exec".to_string());
        let mut state = CodexWrapperTurnState::default();

        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "turn.completed",
                "last_agent_message": "exec final",
                "usage": {"input_tokens": 10, "cached_input_tokens": 4, "output_tokens": 3}
            }),
            &mut thread_id,
            &mut state,
            std::time::Instant::now(),
        )
        .unwrap();

        let lines = read_jsonl(&path);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["type"], "result");
        assert_eq!(lines[0]["usage"]["input_tokens"], 6);
        assert_eq!(lines[0]["usage"]["cache_read_input_tokens"], 4);
        assert_eq!(lines[0]["usage"]["output_tokens"], 3);
        assert_eq!(lines[0]["input_tokens"], 10);
        assert_eq!(lines[0]["output_tokens"], 3);
    }

    // #3275 fail-safe: with no token source at all the nested usage must be
    // omitted (recap stays Unknown rather than fabricating values).
    #[test]
    fn result_omits_nested_usage_without_any_token_source() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-none".to_string());
        let mut state = CodexWrapperTurnState::default();

        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "event_msg",
                "payload": {
                    "type": "task_complete",
                    "last_agent_message": "no tokens final"
                }
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
        assert!(lines[0].get("usage").is_none());
        assert_eq!(lines[0]["input_tokens"], 0);
        assert_eq!(lines[0]["output_tokens"], 0);
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

    // #3027: schema drift must fail CLOSED. A terminal event with no relayable
    // body, after unknown events were observed, used to emit a `result/success`
    // with an empty body (the adk-cdx blackhole). It must now emit an error.
    #[test]
    fn empty_terminal_after_drift_emits_schema_drift_error_not_empty_success() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-drift".to_string());
        let mut state = CodexWrapperTurnState::default();
        let start = std::time::Instant::now();

        // (1) The assistant body arrives inside an unrecognized event shape that
        //     still carries assistant-content text — dropped, drift flag set,
        //     final_text stays empty.
        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "future.message.event",
                "role": "assistant",
                "content": [{ "type": "future_text", "text": "the lost answer" }]
            }),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();
        assert!(state.saw_dropped_assistant_content);
        // (2) A terminal event with no last_agent_message closes the turn.
        handle_codex_wrapper_event(
            &mut output,
            &json!({"type":"event_msg","payload":{"type":"task_complete"}}),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();

        assert!(state.saw_turn_completed);
        let lines = read_jsonl(&path);
        let result = lines
            .iter()
            .find(|l| l["type"] == "result")
            .expect("a terminal result frame");
        assert_eq!(result["subtype"], "schema_drift");
        assert_eq!(result["is_error"], true);
        assert_eq!(result["unknown_event_count"], 1);
        // Anti-blackhole contract: the reason must ride in `errors` so the shared
        // relay tailer's extract_result_error_text() surfaces non-empty text and
        // terminates the turn as an error HardResult instead of an empty Done.
        let drift_errors = result["errors"]
            .as_array()
            .expect("schema_drift result carries an errors array");
        assert!(
            drift_errors
                .iter()
                .any(|e| e.as_str().is_some_and(|s| !s.trim().is_empty())),
            "drift reason must be non-empty so the relay surfaces an error, not a blackhole"
        );
        // No empty `success` frame may be emitted.
        assert!(
            !lines
                .iter()
                .any(|l| l["type"] == "result" && l["subtype"] == "success"),
            "schema drift must not be downgraded to success"
        );
    }

    // #3027 case (c): the `response_item`/`message` envelope is recognized but
    // the content item type is renamed, so the body is silently skipped. A
    // bodyless terminal event must then fail closed, not emit an empty success.
    #[test]
    fn renamed_message_content_item_then_empty_terminal_fails_closed() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-content-drift".to_string());
        let mut state = CodexWrapperTurnState::default();
        let start = std::time::Instant::now();

        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "future_text", "text": "the answer" }]
                }
            }),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();
        assert!(
            state.saw_dropped_assistant_content,
            "an assistant message whose only content item drifted must flag the drop"
        );
        handle_codex_wrapper_event(
            &mut output,
            &json!({"type":"event_msg","payload":{"type":"task_complete"}}),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();

        let lines = read_jsonl(&path);
        let result = lines
            .iter()
            .find(|l| l["type"] == "result")
            .expect("a terminal result frame");
        assert_eq!(result["subtype"], "schema_drift");
        assert!(!lines.iter().any(|l| l["subtype"] == "success"));
    }

    // #3027 round-3 Codex finding: a `response_item` whose payload carries the
    // assistant body DIRECTLY as a bare text item (no role / `type:"message"`
    // wrapper, e.g. a drifted `output_text` payload) must still flag the drop so
    // a trailing bodyless terminal fails closed — the generic role/message gate
    // would otherwise reject the bare text and re-open the blackhole.
    #[test]
    fn bare_text_response_item_then_empty_terminal_fails_closed() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-bare-text".to_string());
        let mut state = CodexWrapperTurnState::default();
        let start = std::time::Instant::now();

        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "response_item",
                "payload": { "type": "output_text", "text": "the lost answer" }
            }),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();
        assert!(
            state.saw_dropped_assistant_content,
            "a response_item carrying bare body text must flag the drop"
        );
        handle_codex_wrapper_event(
            &mut output,
            &json!({"type":"event_msg","payload":{"type":"task_complete"}}),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();

        let lines = read_jsonl(&path);
        let result = lines
            .iter()
            .find(|l| l["type"] == "result")
            .expect("a terminal result frame");
        assert_eq!(result["subtype"], "schema_drift");
        assert!(!lines.iter().any(|l| l["subtype"] == "success"));
    }

    // A mixed assistant message (one known text item + one drifted item) still
    // relays the known text and must NOT be flagged as drift.
    #[test]
    fn message_with_one_known_text_item_is_not_flagged_as_drift() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-mixed-content".to_string());
        let mut state = CodexWrapperTurnState::default();

        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        { "type": "output_text", "text": "relayed answer" },
                        { "type": "future_text", "text": "extra" }
                    ]
                }
            }),
            &mut thread_id,
            &mut state,
            std::time::Instant::now(),
        )
        .unwrap();

        assert!(!state.saw_dropped_assistant_content);
        assert_eq!(state.final_text, "relayed answer");
        assert!(schema_drift_reason(&state).is_none());
    }

    // Classification guard: only assistant/message-context shapes count as
    // dropped content; benign status/tool payloads do not (P2 false-positive),
    // and assistant messages nested under a wrapper still count (P1 miss).
    #[test]
    fn assistant_content_classification_requires_message_context() {
        use super::value_carries_assistant_content;
        // Benign status events with diagnostic strings — not assistant content.
        assert!(!value_carries_assistant_content(
            &json!({"type":"future_status","message":"running tool"})
        ));
        assert!(!value_carries_assistant_content(
            &json!({"type":"future_status","text":"step 3 of 5"})
        ));
        // A bare content array on an unknown tool/status payload (no assistant
        // or message context) must NOT be treated as a dropped answer.
        assert!(!value_carries_assistant_content(
            &json!({"type":"future_tool","content":[{"type":"text","text":"hi"}]})
        ));
        // Real assistant shapes trip it: role=assistant ...
        assert!(value_carries_assistant_content(
            &json!({"role":"assistant"})
        ));
        // ... or type=message with text content ...
        assert!(value_carries_assistant_content(
            &json!({"type":"message","content":[{"type":"text","text":"hi"}]})
        ));
        // ... or an assistant message nested under a renamed outer event.
        assert!(value_carries_assistant_content(&json!({
            "type": "future.unknown",
            "payload": { "type": "message", "role": "assistant",
                         "content": [{"type":"output_text","text":"answer"}] }
        })));
        assert!(value_carries_assistant_content(&json!({
            "type": "future.unknown",
            "item": { "role": "assistant" }
        })));
        // An explicit non-assistant role (user echo) wrapped in a renamed
        // message envelope is NOT dropped assistant content — even with text.
        assert!(!value_carries_assistant_content(&json!({
            "type":"message","role":"user",
            "content":[{"type":"text","text":"echoed prompt"}]
        })));
        assert!(!value_carries_assistant_content(&json!({
            "type":"future.unknown",
            "payload":{"type":"message","role":"user",
                       "content":[{"type":"text","text":"echoed prompt"}]}
        })));
    }

    // A real body present at finalization stays a success even if earlier
    // unknown events were observed — drift detection must not be over-eager.
    #[test]
    fn drift_with_real_body_still_emits_success() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-mixed".to_string());
        let mut state = CodexWrapperTurnState::default();
        let start = std::time::Instant::now();

        handle_codex_wrapper_event(
            &mut output,
            &json!({"type":"future.event","payload":{"type":"nested"}}),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();
        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": "real answer" }]
                }
            }),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();
        handle_codex_wrapper_event(
            &mut output,
            &json!({"type":"event_msg","payload":{"type":"task_complete"}}),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();

        let lines = read_jsonl(&path);
        let result = lines
            .iter()
            .find(|l| l["type"] == "result")
            .expect("a terminal result frame");
        assert_eq!(result["subtype"], "success");
        assert_eq!(result["result"], "real answer");
        assert!(
            !lines.iter().any(|l| l["subtype"] == "schema_drift"),
            "a turn with a real body must not be flagged as drift"
        );
    }

    // No drift observed but empty body (e.g. a tool-only turn) must remain a
    // success — fail-closed only triggers when an assistant-content event was
    // actually dropped, not on the broad unknown_event_count.
    #[test]
    fn empty_body_without_dropped_content_remains_success() {
        let mut state = CodexWrapperTurnState::default();
        assert!(schema_drift_reason(&state).is_none());
        // Benign unknown events were seen but none carried assistant content:
        // this is a legitimate empty/tool-only turn, NOT drift.
        state.unknown_event_count = 3;
        assert!(
            schema_drift_reason(&state).is_none(),
            "benign unknown events must not trip the fail-closed path"
        );
        // An unknown event that dropped assistant content + empty body => drift.
        state.saw_dropped_assistant_content = true;
        assert!(schema_drift_reason(&state).is_some());
        // Body present => not flagged even when content was dropped.
        state.final_text.push_str("answer");
        assert!(schema_drift_reason(&state).is_none());
    }

    // #3027 P2 regression: a tool-only turn that emits a benign unknown
    // lifecycle event and a bodyless task_complete must finalize as SUCCESS,
    // not be misclassified as schema drift.
    #[test]
    fn tool_only_turn_with_benign_unknown_event_finalizes_success() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut thread_id = Some("thread-tool".to_string());
        let mut state = CodexWrapperTurnState::default();
        let start = std::time::Instant::now();

        // A recognized tool call (relayable activity, but no final_text).
        handle_codex_wrapper_event(
            &mut output,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "name": "update_plan",
                    "arguments": "{}",
                    "call_id": "c1"
                }
            }),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();
        // A benign unknown lifecycle event with no assistant body.
        handle_codex_wrapper_event(
            &mut output,
            &json!({"type":"event_msg","payload":{"type":"future_progress_tick","step":3}}),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();
        assert!(!state.saw_dropped_assistant_content);
        // Bodyless terminal event closes the turn.
        handle_codex_wrapper_event(
            &mut output,
            &json!({"type":"event_msg","payload":{"type":"task_complete"}}),
            &mut thread_id,
            &mut state,
            start,
        )
        .unwrap();

        let lines = read_jsonl(&path);
        let result = lines
            .iter()
            .find(|l| l["type"] == "result")
            .expect("a terminal result frame");
        assert_eq!(
            result["subtype"], "success",
            "benign unknown events on a tool-only turn must not fail closed"
        );
        assert!(!lines.iter().any(|l| l["subtype"] == "schema_drift"));
    }

    // emit_schema_drift_result is idempotent: a no-op once the turn is closed.
    #[test]
    fn schema_drift_emit_is_idempotent() {
        let tdir = tempfile::tempdir().unwrap();
        let path = tdir.path().join("codex.jsonl");
        let mut output = RotatingJsonlWriter::open(&path).unwrap();
        let mut state = CodexWrapperTurnState {
            unknown_event_count: 2,
            ..Default::default()
        };

        emit_schema_drift_result(&mut output, &mut state, "drift reason");
        emit_schema_drift_result(&mut output, &mut state, "drift reason");

        assert!(state.saw_turn_completed);
        let lines = read_jsonl(&path);
        assert_eq!(
            lines.iter().filter(|l| l["type"] == "result").count(),
            1,
            "schema drift result must be emitted at most once"
        );
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

#[cfg(test)]
mod turn_timeout_tests {
    use super::{
        DEFAULT_CODEX_TURN_HARD_CEILING_SECS, DEFAULT_CODEX_TURN_IDLE_RECV_SECS,
        codex_turn_hard_ceiling, codex_turn_idle_recv_timeout,
    };
    use std::sync::mpsc;
    use std::time::Duration;

    /// #3557 (B): after the first event the run loop must bound inter-event
    /// silence. This mirrors the post-`saw_any_stdout` branch: a channel that
    /// never produces another line must surface a Timeout (the kill+Err path),
    /// not block forever as the old unconditional `recv()` did.
    #[test]
    fn idle_recv_timeout_fires_when_no_further_events() {
        let (_tx, rx) = mpsc::channel::<Result<Option<String>, String>>();
        // Keep _tx alive (so it's not Disconnected) and never send.
        let result = rx.recv_timeout(Duration::from_millis(50));
        assert!(matches!(result, Err(mpsc::RecvTimeoutError::Timeout)));
    }

    /// A live event stream still passes through `recv_timeout` cleanly.
    #[test]
    fn idle_recv_timeout_passes_through_live_event() {
        let (tx, rx) = mpsc::channel::<Result<Option<String>, String>>();
        tx.send(Ok(Some("{\"type\":\"event\"}\n".to_string())))
            .unwrap();
        let received = rx.recv_timeout(Duration::from_secs(1));
        assert!(matches!(received, Ok(Ok(Some(_)))));
    }

    #[test]
    fn idle_recv_timeout_defaults_to_generous_window() {
        // Only assert the default when the env override is not set, so this is
        // robust under a polluted shell.
        if std::env::var("AGENTDESK_CODEX_TURN_IDLE_RECV_SECS").is_err() {
            assert_eq!(
                codex_turn_idle_recv_timeout(),
                Duration::from_secs(DEFAULT_CODEX_TURN_IDLE_RECV_SECS)
            );
        }
    }

    /// #3557 (B) Codex-review r2 fix: this wrapper runs `codex exec` over a pipe
    /// (no tmux pane), so a long SILENT tool run cannot be distinguished from an
    /// idle hang via pane activity — the JSON stream is the only liveness
    /// signal. To avoid killing a normal long-running tool (e.g. a big build),
    /// the idle window must be generous and the 4h hard ceiling must stay the
    /// real backstop. Lock both: the default idle window is now >= 1h AND is
    /// strictly smaller than the hard ceiling (otherwise the idle window would
    /// be dead code that the ceiling always preempts).
    #[test]
    fn idle_window_is_generous_and_below_hard_ceiling() {
        assert_eq!(DEFAULT_CODEX_TURN_IDLE_RECV_SECS, 3600);
        assert!(
            DEFAULT_CODEX_TURN_IDLE_RECV_SECS >= 3600,
            "idle window must clear the longest plausible single silent tool run"
        );
        assert!(
            DEFAULT_CODEX_TURN_IDLE_RECV_SECS < DEFAULT_CODEX_TURN_HARD_CEILING_SECS,
            "the hard ceiling must remain the real backstop, above the idle window"
        );
    }

    /// A live event arriving before the (generous) idle window elapses must NOT
    /// be treated as a hang: a long-running tool that emits its completion event
    /// within the window passes through cleanly. Models the post-first-event
    /// branch: a delayed-but-present event resolves to `Ok`, not a Timeout kill.
    #[test]
    fn delayed_event_within_idle_window_is_not_killed() {
        let (tx, rx) = mpsc::channel::<Result<Option<String>, String>>();
        // Simulate a silent tool run that finishes and emits a completion event
        // shortly before the idle window would expire.
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            let _ = tx.send(Ok(Some("{\"type\":\"item.completed\"}\n".to_string())));
        });
        // Generous window relative to the simulated tool latency: the event wins.
        let received = rx.recv_timeout(Duration::from_millis(500));
        assert!(
            matches!(received, Ok(Ok(Some(_)))),
            "a tool event arriving within the idle window must not trip the idle-kill path"
        );
    }

    #[test]
    fn hard_ceiling_defaults_to_four_hours() {
        if std::env::var("AGENTDESK_CODEX_TURN_HARD_CEILING_SECS").is_err() {
            assert_eq!(
                codex_turn_hard_ceiling(),
                Duration::from_secs(DEFAULT_CODEX_TURN_HARD_CEILING_SECS)
            );
            assert_eq!(DEFAULT_CODEX_TURN_HARD_CEILING_SECS, 4 * 3600);
        }
    }

    /// The ceiling check is a pure elapsed comparison; verify the predicate the
    /// run loop uses (`start.elapsed() >= hard_ceiling`).
    #[test]
    fn ceiling_predicate_triggers_when_elapsed_exceeds_limit() {
        let ceiling = Duration::from_millis(10);
        let start = std::time::Instant::now();
        std::thread::sleep(Duration::from_millis(20));
        assert!(start.elapsed() >= ceiling);
    }

    /// #3557 (B) Codex-review fix: the idle recv must be capped by the REMAINING
    /// ceiling budget so a post-first-event hang can never run past the ceiling
    /// by a full idle window. This mirrors `idle_recv_timeout.min(remaining)`.
    /// Mid-run, with budget left, the cap wins only when it is the smaller of
    /// the two.
    #[test]
    fn idle_recv_is_capped_by_ceiling_remainder() {
        let idle = Duration::from_secs(1800);
        // Plenty of budget left: idle window governs (cap does not bite).
        let remaining_lots = Duration::from_secs(3600);
        assert_eq!(idle.min(remaining_lots), idle);
        // Near the ceiling: the remainder governs, so a hang is killed at the
        // ceiling, not ceiling+idle.
        let remaining_little = Duration::from_secs(60);
        assert_eq!(idle.min(remaining_little), remaining_little);
        assert!(idle.min(remaining_little) < idle);
    }

    /// At/over the ceiling the remainder is zero, which the run loop treats as
    /// "kill now" rather than entering another recv. Verify the saturating
    /// remainder is zero exactly when elapsed has reached the ceiling.
    #[test]
    fn ceiling_remainder_is_zero_at_or_past_ceiling() {
        let ceiling = Duration::from_secs(4 * 3600);
        // elapsed == ceiling => zero remainder => immediate kill branch.
        assert!(ceiling.saturating_sub(ceiling).is_zero());
        // elapsed > ceiling => still zero (saturating), never a huge wait.
        let past = ceiling + Duration::from_secs(120);
        assert!(ceiling.saturating_sub(past).is_zero());
        // elapsed < ceiling => positive remainder used as the recv cap.
        let before = ceiling - Duration::from_secs(120);
        let remaining = ceiling.saturating_sub(before);
        assert!(!remaining.is_zero());
        assert_eq!(remaining, Duration::from_secs(120));
    }
}
