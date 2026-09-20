//! Provider-neutral process lifecycle for StreamJson CLIs.

use crate::services::process::stream_child::stream_queue;

use crate::services::process::stream_child::{EXIT_POLL, StreamChild, finish_reader, spawn_reader};

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::services::agent_protocol::StreamMessage;
use crate::services::platform::{BinaryResolution, apply_binary_resolution};
use crate::services::process::configure_child_process_group;
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
    pub env: Vec<(String, String)>,
    pub unset_env: Vec<String>,
    pub codec: Box<dyn StreamJsonCodec>,
}

pub fn run_prepared(
    prepared: PreparedCommand,
    sender: Sender<StreamMessage>,
    no_output_timeout: Duration,
    cancel: Option<std::sync::Arc<crate::services::provider::CancelToken>>,
) -> Result<(), String> {
    run_prepared_with_clock(prepared, sender, no_output_timeout, cancel, Instant::now)
}

fn run_prepared_with_clock(
    prepared: PreparedCommand,
    sender: Sender<StreamMessage>,
    no_output_timeout: Duration,
    cancel: Option<std::sync::Arc<crate::services::provider::CancelToken>>,
    mut now: impl FnMut() -> Instant,
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
    for key in &prepared.unset_env {
        command.env_remove(key);
    }
    for (key, value) in &prepared.env {
        command.env(key, value);
    }
    let mut child = command
        .args(&prepared.args)
        .current_dir(&prepared.current_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("Failed to start StreamJson CLI: {error}"))?;

    register_child_pid(cancel.as_deref(), child.id());
    let watchdog = spawn_cancel_watchdog(cancel.clone(), "stream-json-cli");
    let mut lifecycle = StreamChild::new(&child, cancel.clone(), watchdog);
    if cancel_requested(cancel.as_deref()) {
        lifecycle.terminate(&mut child);
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
    let (line_tx, line_rx) = stream_queue::channel::<Option<String>>();
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
    let stderr_handle = spawn_reader(move || collect_stderr(stderr));

    let mut codec = prepared.codec;
    let poll = EXIT_POLL;
    let startup = startup_output_timeout(no_output_timeout);
    let started_at = now();
    let mut saw_progress = false;
    let mut stdout_line_count = 0_u64;

    loop {
        if cancel_requested(cancel.as_deref()) {
            lifecycle.terminate(&mut child);
            let _ = child.wait();
            let _ = finish_reader(&stderr_handle);
            return Ok(());
        }
        lifecycle
            .observe_and_seal(&mut child, &line_rx)
            .map_err(|e| e.to_string())?;
        let now = now();
        if !saw_progress && now.duration_since(started_at) >= startup {
            lifecycle.terminate(&mut child);
            let _ = child.wait();
            let _ = finish_reader(&stderr_handle);
            return Err(format!(
                "[{NO_OUTPUT_ERROR_MARKER}] StreamJson CLI produced no output for {} seconds",
                startup.as_secs()
            ));
        }
        match line_rx.recv_timeout(poll) {
            Ok(Some(line)) => {
                stdout_line_count += 1;
                let messages = match codec.push_stdout_line(&line) {
                    Ok(messages) => messages,
                    Err(error) => {
                        lifecycle.terminate(&mut child);
                        let _ = child.wait();
                        let _ = finish_reader(&stderr_handle);
                        return Err(mark_no_output_error(saw_progress, error));
                    }
                };
                // Empty/whitespace lines are not protocol progress. In
                // particular, a noisy wrapper must not keep a poisoned resume
                // token alive forever by printing blank lines faster than the
                // polling interval.
                if !line.trim().is_empty() {
                    saw_progress = true;
                }
                for message in messages {
                    if sender.send(message).is_err() {
                        lifecycle.terminate(&mut child);
                        let _ = child.wait();
                        let _ = finish_reader(&stderr_handle);
                        return Ok(());
                    }
                }
            }
            Ok(None) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }

    let status = lifecycle
        .wait(&mut child)
        .map_err(|error| format!("Failed waiting for StreamJson CLI: {error}"))?;
    let stderr = finish_reader(&stderr_handle);
    if cancel_requested(cancel.as_deref()) {
        return Ok(());
    }
    let stderr_present = !stderr.trim().is_empty();
    tracing::debug!(
        exit_code = ?status.code(),
        stdout_line_count,
        stderr_len = stderr.len(),
        stderr_present,
        "stream_json_cli child finished"
    );
    if stderr_present && status.success() {
        tracing::warn!(
            exit_code = ?status.code(),
            stdout_line_count,
            stderr_len = stderr.len(),
            "stream_json_cli child exited successfully with stderr"
        );
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

/// Keep a bounded diagnostic prefix while draining the pipe to avoid blocking
/// the child. Decode after collection so arbitrary bytes and a UTF-8 character
/// crossing the cap cannot panic or discard the whole diagnostic.
fn collect_stderr(mut reader: impl Read) -> String {
    const MAX_BYTES: usize = 16 * 1024;
    let mut captured = Vec::with_capacity(MAX_BYTES);
    let mut chunk = [0_u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(count) => {
                let keep = count.min(MAX_BYTES - captured.len());
                captured.extend_from_slice(&chunk[..keep]);
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    String::from_utf8_lossy(&captured).into_owned()
}

fn startup_output_timeout(timeout: Duration) -> Duration {
    Duration::from_secs(if timeout.is_zero() { 90 } else { 60 })
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[test]
    fn accepted_stream_runs_past_virtual_day_until_real_eof() {
        struct Codec(usize);
        impl StreamJsonCodec for Codec {
            fn push_stdout_line(&mut self, _: &str) -> Result<Vec<StreamMessage>, String> {
                self.0 += 1;
                Ok(vec![])
            }
            fn finish(&mut self, code: Option<i32>, _: &str) -> Result<Vec<StreamMessage>, String> {
                assert_eq!(code, Some(0));
                assert_eq!(
                    self.0, 2,
                    "both accepted stream events must reach the codec"
                );
                Ok(vec![])
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let prepared = PreparedCommand {
            executable: "/bin/sh".into(),
            resolution: BinaryResolution {
                requested_binary: "sh".into(),
                resolved_path: Some("/bin/sh".into()),
                canonical_path: None,
                source: None,
                attempts: vec![],
                failure_kind: None,
                exec_path: None,
            },
            args: vec!["-c".into(), "printf 'accepted\nfinished\n'".into()],
            redacted_args: vec![],
            current_dir: dir.path().into(),
            env: vec![],
            unset_env: vec![],
            codec: Box::new(Codec(0)),
        };
        let (tx, _rx) = mpsc::channel();
        let start = Instant::now();
        let mut ticks = 0;
        let result = run_prepared_with_clock(prepared, tx, Duration::ZERO, None, || {
            ticks += 1;
            start + Duration::from_secs(if ticks > 2 { 24 * 3600 } else { 0 })
        });
        assert!(
            result.is_ok(),
            "accepted provider was terminated: {result:?}"
        );
        assert!(ticks >= 4);
    }

    use super::*;

    #[test]
    fn stderr_capture_is_bounded_and_drains_the_pipe() {
        let mut source = std::io::Cursor::new(vec![b'x'; 128 * 1024]);
        let captured = collect_stderr(&mut source);
        assert_eq!(captured.len(), 16 * 1024);
        assert_eq!(source.position(), 128 * 1024);
    }

    #[test]
    fn stderr_capture_preserves_invalid_utf8_and_partial_characters() {
        assert_eq!(collect_stderr(&b"denied: \xff"[..]), "denied: \u{fffd}");
        let mut source = vec![b'x'; 16 * 1024 - 1];
        source.extend_from_slice("한글".as_bytes());
        let captured = collect_stderr(source.as_slice());
        assert!(captured.starts_with(&"x".repeat(16 * 1024 - 1)));
        assert!(captured.ends_with('\u{fffd}'));
    }

    #[test]
    fn zero_timeout_preserves_startup_handshake_budget() {
        assert_eq!(
            startup_output_timeout(Duration::ZERO),
            Duration::from_secs(90)
        );
    }

    #[test]
    fn nonzero_timeout_preserves_startup_handshake_budget() {
        assert_eq!(
            startup_output_timeout(Duration::from_secs(1)),
            Duration::from_secs(60)
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

#[cfg(all(test, unix))]
mod child_exit_tests {
    use super::*;
    use crate::services::process::stream_child::test_fixture::{CASES, ProviderFixture};
    #[test]
    fn actual_provider_exit_drains_terminal_without_waiting_for_descendant_fds() {
        run_cases(&CASES);
    }

    #[test]
    fn published_terminal_survives_delayed_consumer_after_actual_exit() {
        run_cases(&["delayed_normal"]);
    }

    fn run_cases(cases: &[&str]) {
        for &mode in cases {
            let fixture = ProviderFixture::new("grok", mode);
            let normal = matches!(mode, "normal" | "quiet" | "delayed_normal");
            let (tx, rx) = mpsc::channel();
            let prepared = PreparedCommand {
                executable: fixture.cli.clone(),
                resolution: fixture.resolution(),
                args: vec![],
                redacted_args: vec![],
                current_dir: fixture.path().into(),
                env: vec![],
                unset_env: vec![],
                codec: Box::new(super::super::codec::MessagesJsonCodec::new()),
            };
            let completed = run_prepared(prepared, tx, Duration::ZERO, None);
            assert!(
                completed.is_ok(),
                "queued terminal must reach real codec: {completed:?}"
            );
            let messages: Vec<_> = rx.try_iter().collect();
            assert_eq!(
                messages
                    .iter()
                    .any(|m| matches!(m, StreamMessage::Done { .. })),
                normal,
                "{mode}: {messages:?}"
            );
            if !normal {
                assert!(messages.iter().any(|m| matches!(
                    m,
                    StreamMessage::Error {
                        exit_code: Some(7),
                        ..
                    }
                )));
            }
            fixture.verify_return(mode);
        }
    }

    #[test]
    fn actual_quiet_provider_observes_exact_token_manual_cancel() {
        let fixture = ProviderFixture::new("grok", "cancel");
        let token = std::sync::Arc::new(crate::services::provider::CancelToken::new());
        let cancel = fixture.cancel_after_accept(token.clone());
        let (tx, _rx) = mpsc::channel();
        let prepared = PreparedCommand {
            executable: fixture.cli.clone(),
            resolution: fixture.resolution(),
            args: vec![],
            redacted_args: vec![],
            current_dir: fixture.path().into(),
            env: vec![],
            unset_env: vec![],
            codec: Box::new(super::super::codec::MessagesJsonCodec::new()),
        };
        run_prepared(prepared, tx, Duration::ZERO, Some(token.clone())).unwrap();
        cancel.join().unwrap();
        assert_eq!(token.cancel_source().as_deref(), Some("manual_cancel"));
        assert_eq!(token.child_pid_value(), None);
        fixture.verify_return("cancel");
    }
}
