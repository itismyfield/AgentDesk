//! The owned direct child is observed before its messages enter the bridge.

use super::handle_codex_json_line;
use crate::services::agent_protocol::StreamMessage;
use crate::services::discord::replay_policy::live::{RawTap, Seen};
use std::io::{BufRead, BufReader};
use std::process::Child;
use std::sync::mpsc::{self, Sender};

pub(super) fn read(
    mut child: Child,
    sender: Sender<StreamMessage>,
    session_id: Option<&str>,
    tap: RawTap,
    cancel: impl Fn(&mut Child) -> bool,
) -> Result<(), String> {
    let pending = tap.start_stderr(&mut child);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "Failed to capture Codex stdout".to_string())?;
    let mut current_thread_id = session_id.map(str::to_string);
    let mut final_text = String::new();
    let mut saw_done = false;
    let started_at = std::time::Instant::now();
    for line in BufReader::new(stdout).lines() {
        if cancel(&mut child) {
            return Ok(());
        }
        let line = line.map_err(|e| format!("Failed to read Codex output: {e}"))?;
        tap.codex_line(&line);
        let done = if tap.active() {
            let (tx, rx) = mpsc::channel();
            let done = handle_codex_json_line(
                &line,
                &tx,
                &mut current_thread_id,
                &mut final_text,
                started_at,
            )?;
            for message in rx.try_iter() {
                tap.record_line("result", Some(&message));
                if sender.send(message).is_err() {
                    tap.see(Seen::Broken);
                }
            }
            done
        } else {
            handle_codex_json_line(
                &line,
                &sender,
                &mut current_thread_id,
                &mut final_text,
                started_at,
            )?
        };
        if let Some(done) = done {
            saw_done |= done;
        }
    }
    tap.see(Seen::StdoutEof);
    let (status, stderr, drained) = if tap.active() {
        let status = child
            .wait()
            .map_err(|e| format!("Failed to wait for Codex: {e}"))?;
        tap.see(Seen::Exited { waited: true });
        let (stderr, drained) = tap.collect_stderr(pending);
        (status, stderr, drained)
    } else {
        let output = child
            .wait_with_output()
            .map_err(|e| format!("Failed to wait for Codex: {e}"))?;
        (
            output.status,
            String::from_utf8_lossy(&output.stderr).to_string(),
            Seen::StderrDrained,
        )
    };
    if !status.success() && !saw_done {
        let message = if stderr.trim().is_empty() {
            format!("Codex exited with code {:?}", status.code())
        } else {
            stderr.trim().to_string()
        };
        tap.error(&message, &stderr);
        let delivered = sender
            .send(StreamMessage::Error {
                message: message.clone(),
                stdout: String::new(),
                stderr: stderr.clone(),
                exit_code: status.code(),
            })
            .is_ok();
        tap.finish_error(delivered, drained, &message, &stderr);
        return Ok(());
    }
    let delivered = saw_done
        || sender
            .send(StreamMessage::Done {
                result: final_text,
                session_id: current_thread_id,
            })
            .is_ok();
    tap.finish(delivered, drained);
    Ok(())
}
