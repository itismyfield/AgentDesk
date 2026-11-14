//! Tells a Claude scheduled-task prompt from a typed one off its transcript row; the hook
//! payload names no origin. The row lands after the hook, so the relay re-reads briefly.

use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::Duration;

use serde_json::Value;

use super::injected_prompt_policy::{
    classify_injected_prompt_with_origin, format_direct_prompt_notification,
};
use crate::services::tui_prompt_dedupe::ObservedTuiPrompt;

const HOOK_TRANSCRIPTS_KEPT: usize = 64;
/// Rows between a fire's `enqueue` and its `user` row include request blobs, so the tail is wide.
const TAIL_WINDOW_BYTES: u64 = 2 * 1024 * 1024;
const ROW_WAIT_STEP: Duration = Duration::from_millis(50);
/// Re-reads allowed before the transcript shows whether the input was dequeued to run.
const QUEUE_WAIT_STEPS: usize = 4;
/// Re-reads allowed for the `user` row of an input the transcript shows was dequeued to run.
const ROW_WAIT_STEPS: usize = 12;

static HOOK_TRANSCRIPTS: LazyLock<Mutex<VecDeque<(String, PathBuf)>>> =
    LazyLock::new(Default::default);

/// Remembers which transcript a Claude hook prompt will be written to.
pub(super) fn note_hook_transcript(provider: &str, prompt_id: Option<&str>, payload: &Value) {
    let path = payload
        .get("transcript_path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|path| !path.is_empty());
    let (Some(prompt_id), Some(path), "claude") = (prompt_id, path, provider) else {
        return;
    };
    let mut kept = HOOK_TRANSCRIPTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    kept.retain(|(id, _)| id != prompt_id);
    if kept.len() == HOOK_TRANSCRIPTS_KEPT {
        kept.pop_front();
    }
    kept.push_back((prompt_id.to_string(), PathBuf::from(path)));
}

fn hook_transcript(prompt_id: &str) -> Option<PathBuf> {
    HOOK_TRANSCRIPTS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|(id, _)| id == prompt_id)
        .map(|(_, path)| path.clone())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    /// The prompt's own row carries Claude's scheduled-task marks.
    Scheduled,
    /// The prompt's own row is there and carries none.
    Typed,
    /// The row has not landed; `dequeued`: the transcript shows the input left the queue to run.
    Unwritten { dequeued: bool },
}

impl Origin {
    /// How many re-reads this reading is still worth.
    pub(super) fn wait_steps(self) -> usize {
        match self {
            Origin::Scheduled | Origin::Typed => 0,
            Origin::Unwritten { dequeued: true } => ROW_WAIT_STEPS,
            Origin::Unwritten { dequeued: false } => QUEUE_WAIT_STEPS,
        }
    }
}

/// Reads the hook prompt's origin off transcript rows. An input typed into a running turn
/// shares that turn's prompt id, so a row counts only when it also carries this prompt's text.
pub(super) fn origin_in_rows(rows: &str, prompt_id: &str, prompt: &str) -> Origin {
    let wanted = prompt.trim();
    let (mut enqueued, mut dequeued) = (false, false);
    let mut typed = false;
    for line in rows.lines() {
        if !(line.contains(prompt_id) || line.contains("\"queue-operation\"")) {
            continue;
        }
        let Ok(row) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        match row["type"].as_str() {
            Some("queue-operation") => {
                let ours = row["content"].as_str().map(str::trim) == Some(wanted);
                match row["operation"].as_str() {
                    Some("enqueue") if ours => enqueued = true,
                    // Absorbed into the running turn: it never runs as a prompt of its own.
                    Some("remove") if ours => enqueued = false,
                    Some("dequeue") => dequeued |= enqueued,
                    _ => {}
                }
            }
            Some("user")
                if row["promptId"] == prompt_id
                    && row_text(&row).as_deref().map(str::trim) == Some(wanted) =>
            {
                if carries_scheduled_marks(&row) {
                    return Origin::Scheduled;
                }
                typed = true;
            }
            _ => {}
        }
    }
    if typed {
        Origin::Typed
    } else {
        Origin::Unwritten { dequeued }
    }
}

fn row_text(row: &Value) -> Option<String> {
    match &row["message"]["content"] {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n"))
        }
        _ => None,
    }
}

fn carries_scheduled_marks(row: &Value) -> bool {
    row["turnOrigin"] == "scheduled"
        || row["scheduledTaskId"].is_string()
        || matches!(
            row["wakeupSource"].as_str(),
            Some("schedule_wakeup" | "loop_wakeup")
        )
}

fn read_tail(path: &Path) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_WINDOW_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    // A window that opens mid-row drops that row's remainder.
    let rows = match (start, text.find('\n')) {
        (0, _) => &text[..],
        (_, Some(first_break)) => &text[first_break + 1..],
        (_, None) => "",
    };
    Some(rows.to_string())
}

/// Whether `prompt` is a scheduled-task fire, re-reading the transcript while its row may
/// still land.
pub(super) async fn is_scheduled_fire(prompt: &ObservedTuiPrompt) -> bool {
    if prompt.provider != "claude" {
        return false;
    }
    let Some(prompt_id) = prompt.hook_prompt_id.as_deref() else {
        return false;
    };
    let Some(path) = hook_transcript(prompt_id) else {
        return false;
    };
    let mut step = 0;
    loop {
        // A transcript that is not there yet is one whose first row has not been flushed.
        let origin = read_tail(&path)
            .map(|rows| origin_in_rows(&rows, prompt_id, &prompt.prompt))
            .unwrap_or(Origin::Unwritten { dequeued: false });
        if step >= origin.wait_steps() {
            return origin == Origin::Scheduled;
        }
        step += 1;
        tokio::time::sleep(ROW_WAIT_STEP).await;
    }
}

/// The notice for a direct prompt, headed `⏰ scheduled prompt` when it is a scheduled fire.
pub(super) async fn direct_prompt_notice(prompt: &ObservedTuiPrompt) -> String {
    let class =
        classify_injected_prompt_with_origin(&prompt.prompt, is_scheduled_fire(prompt).await);
    format_direct_prompt_notification(
        class,
        &prompt.provider,
        &prompt.tmux_session_name,
        &prompt.prompt,
    )
}

#[cfg(test)]
#[path = "scheduled_prompt_tests.rs"]
mod tests;
