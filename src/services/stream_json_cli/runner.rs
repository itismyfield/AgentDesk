//! Provider-neutral process lifecycle for StreamJson CLIs.

use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::Duration;

use crate::services::agent_protocol::StreamMessage;
use crate::services::platform::{BinaryResolution, apply_binary_resolution};
use crate::services::process::{configure_child_process_group, kill_child_tree};
use crate::services::provider::{cancel_requested, register_child_pid, spawn_cancel_watchdog};

use super::codec::StreamJsonCodec;

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
    let idle = Duration::from_secs(120);
    let startup = Duration::from_secs(60);
    let mut silent = Duration::ZERO;
    let mut startup_silent = Duration::ZERO;
    let mut saw_progress = false;

    loop {
        if cancel_requested(cancel.as_deref()) {
            kill_child_tree(&mut child);
            let _ = child.wait();
            let _ = stderr_handle.join();
            return Ok(());
        }
        match line_rx.recv_timeout(poll) {
            Ok(Some(line)) => {
                silent = Duration::ZERO;
                startup_silent = Duration::ZERO;
                saw_progress = true;
                for message in codec.push_stdout_line(&line)? {
                    if sender.send(message).is_err() {
                        kill_child_tree(&mut child);
                        let _ = child.wait();
                        let _ = stderr_handle.join();
                        return Ok(());
                    }
                }
            }
            Ok(None) | Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {
                if !saw_progress {
                    startup_silent += poll;
                    if startup_silent >= startup {
                        kill_child_tree(&mut child);
                        let _ = child.wait();
                        let _ = stderr_handle.join();
                        return Err(format!(
                            "StreamJson CLI produced no output for {} seconds",
                            startup.as_secs()
                        ));
                    }
                } else {
                    silent += poll;
                    if silent >= idle {
                        kill_child_tree(&mut child);
                        let _ = child.wait();
                        let _ = stderr_handle.join();
                        return Err(format!(
                            "StreamJson CLI produced no output for {} seconds",
                            idle.as_secs()
                        ));
                    }
                }
            }
        }
    }

    let status = child
        .wait()
        .map_err(|error| format!("Failed waiting for StreamJson CLI: {error}"))?;
    let stderr = stderr_handle.join().unwrap_or_default();
    if cancel_requested(cancel.as_deref()) {
        return Ok(());
    }
    for message in codec.finish(status.code(), &stderr)? {
        let _ = sender.send(message);
    }
    Ok(())
}
