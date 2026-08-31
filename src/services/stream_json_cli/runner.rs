//! Provider-neutral process lifecycle for StreamJson CLIs.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::services::agent_protocol::StreamMessage;
use crate::services::platform::{BinaryResolution, apply_binary_resolution};
use crate::services::process::{configure_child_process_group, kill_child_tree};
use crate::services::provider::{cancel_requested, register_child_pid, spawn_cancel_watchdog};

use super::codec::StreamJsonCodec;

/// Stable marker for a stream that never became live (or stopped producing
/// output) before its CLI process exited.  Callers use this to discard a
/// persisted provider resume token: retrying that token would otherwise put
/// the next turn into the same silent state.
pub const NO_OUTPUT_ERROR_MARKER: &str = "stream-json-no-output";

pub struct PreparedCommand {
    pub executable: PathBuf,
    pub resolution: BinaryResolution,
    pub args: Vec<String>,
    pub redacted_args: Vec<String>,
    pub current_dir: PathBuf,
    pub codec: Box<dyn StreamJsonCodec>,
}

pub fn run_prepared(
    prepared: PreparedCommand,
    sender: Sender<StreamMessage>,
    no_output_timeout: Duration,
    cancel: Option<std::sync::Arc<crate::services::provider::CancelToken>>,
) -> Result<(), String> {
    tracing::info!(
        executable = %prepared.executable.display(),
        args = ?prepared.redacted_args,
        cwd = %prepared.current_dir.display(),
        "stream_json_cli spawn"
    );

    let mut command = Command::new(&prepared.executable);
    apply_binary_resolution(&mut command, &prepared.resolution);
    configure_child_process_group(&mut command);
    let mut child = command
        .args(&prepared.args)
        .current_dir(&prepared.current_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Failed to start StreamJson CLI: {error}"))?;

    register_child_pid(cancel.as_deref(), child.id());
    let _watchdog = spawn_cancel_watchdog(cancel.clone(), "stream-json-cli");
    if cancel_requested(cancel.as_deref()) {
        kill_child_tree(&mut child);
        return Ok(());
    }

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture StreamJson stdout".to_string())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "Failed to capture StreamJson stderr".to_string())?;
    let (line_tx, line_rx) = mpsc::channel::<Option<String>>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(value) => {
                    if line_tx.send(Some(value)).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = line_tx.send(None);
    });
    let stderr_handle = std::thread::spawn(move || {
        let mut buf = String::new();
        let mut reader = BufReader::new(stderr);
        let _ = reader.read_to_string(&mut buf);
        if buf.len() > 16 * 1024 {
            buf.truncate(16 * 1024);
        }
        buf
    });

    let mut codec = prepared.codec;
    let poll = Duration::from_secs(5);
    let (startup, idle) = no_output_watchdogs(no_output_timeout);
    let started_at = Instant::now();
    let mut last_progress_at = started_at;
    let mut saw_progress = false;

    loop {
        if cancel_requested(cancel.as_deref()) {
            kill_child_tree(&mut child);
            let _ = child.wait();
            let _ = stderr_handle.join();
            return Ok(());
        }
        let now = Instant::now();
        let (elapsed, limit) = if saw_progress {
            (now.duration_since(last_progress_at), idle)
        } else {
            (now.duration_since(started_at), startup)
        };
        if limit.is_some_and(|limit| elapsed >= limit) {
            kill_child_tree(&mut child);
            let _ = child.wait();
            let _ = stderr_handle.join();
            return Err(format!(
                "[{NO_OUTPUT_ERROR_MARKER}] StreamJson CLI produced no output for {} seconds",
                limit.expect("checked above").as_secs()
            ));
        }
        match line_rx.recv_timeout(poll) {
            Ok(Some(line)) => {
                let messages = match codec.push_stdout_line(&line) {
                    Ok(messages) => messages,
                    Err(error) => {
                        kill_child_tree(&mut child);
                        let _ = child.wait();
                        let _ = stderr_handle.join();
                        return Err(mark_no_output_error(saw_progress, error));
                    }
                };
                // Empty/whitespace lines are not protocol progress. In
                // particular, a noisy wrapper must not keep a poisoned resume
                // token alive forever by printing blank lines faster than the
                // polling interval.
                if !line.trim().is_empty() {
                    saw_progress = true;
                    last_progress_at = Instant::now();
                }
                for message in messages {
                    if sender.send(message).is_err() {
                        kill_child_tree(&mut child);
                        let _ = child.wait();
                        let _ = stderr_handle.join();
                        return Ok(());
                    }
                }
            }
            Ok(None) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }

    let status = child
        .wait()
        .map_err(|error| format!("Failed waiting for StreamJson CLI: {error}"))?;
    let stderr = stderr_handle.join().unwrap_or_default();
    if cancel_requested(cancel.as_deref()) {
        return Ok(());
    }
    let mut messages = codec
        .finish(status.code(), &stderr)
        .map_err(|error| mark_no_output_error(saw_progress, error))?;
    if !saw_progress {
        for message in &mut messages {
            if let StreamMessage::Error { message, .. } = message {
                *message = mark_no_output_error(false, std::mem::take(message));
            }
        }
    }
    for message in messages {
        let _ = sender.send(message);
    }
    Ok(())
}

fn mark_no_output_error(saw_progress: bool, error: String) -> String {
    if saw_progress || error.to_ascii_lowercase().contains(NO_OUTPUT_ERROR_MARKER) {
        error
    } else {
        format!("[{NO_OUTPUT_ERROR_MARKER}] {error}")
    }
}

/// `Duration::ZERO` means the caller permits an unbounded *turn* duration. It
/// must not disable stream liveness detection: an outputless process has made
/// no progress and cannot be distinguished from a poisoned `--resume` token.
///
/// The longer values for that mode allow genuinely long Grok turns while still
/// recovering a CLI that never emits its initial streaming event.
fn no_output_watchdogs(timeout: Duration) -> (Option<Duration>, Option<Duration>) {
    if timeout.is_zero() {
        (
            Some(Duration::from_secs(90)),
            Some(Duration::from_secs(300)),
        )
    } else {
        (
            Some(Duration::from_secs(60)),
            Some(Duration::from_secs(120)),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_timeout_keeps_liveness_watchdogs_for_unbounded_turns() {
        assert_eq!(
            no_output_watchdogs(Duration::ZERO),
            (
                Some(Duration::from_secs(90)),
                Some(Duration::from_secs(300))
            )
        );
    }

    #[test]
    fn nonzero_no_output_timeout_keeps_default_watchdogs() {
        assert_eq!(
            no_output_watchdogs(Duration::from_secs(1)),
            (
                Some(Duration::from_secs(60)),
                Some(Duration::from_secs(120))
            )
        );
    }

    #[test]
    fn no_output_error_has_machine_readable_marker() {
        let message = format!("[{NO_OUTPUT_ERROR_MARKER}] StreamJson CLI produced no output");
        assert!(message.contains(NO_OUTPUT_ERROR_MARKER));
    }

    #[test]
    fn exit_before_protocol_progress_gets_terminal_reset_marker() {
        assert_eq!(
            mark_no_output_error(false, "terminal success without a valid session id".into()),
            "[stream-json-no-output] terminal success without a valid session id"
        );
        assert_eq!(
            mark_no_output_error(true, "provider exited".into()),
            "provider exited"
        );
    }
}
