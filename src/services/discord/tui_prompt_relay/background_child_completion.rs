//! Closes background children from the native completion records in a Claude
//! transcript, whether or not a turn was running when each record was written.

use std::io::{BufRead, Seek, SeekFrom};

use serde_json::Value;

use super::*;
use crate::db::session_observability::{
    close_background_child_for_tool_use_pg, has_open_background_children_pg,
};

/// Read position per tmux session, separate from the prompt cursor so records a
/// running turn already consumed are still read here.
static COMPLETION_CURSORS: std::sync::LazyLock<
    std::sync::Mutex<std::collections::HashMap<String, CompletionCursor>>,
> = std::sync::LazyLock::new(Default::default);

#[derive(Clone, PartialEq, Eq)]
struct CompletionCursor {
    path: PathBuf,
    offset: u64,
    revision: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct ChildCompletion {
    line_start: u64,
    tool_use_id: String,
    status: String,
}

/// Applies each Claude TUI binding's new transcript completions before the idle
/// tick's per-binding skips (inflight, no channel), then returns the bindings.
pub(in crate::services::discord) async fn claude_bindings_after_child_completions(
    shared: &Arc<SharedData>,
) -> Vec<(
    String,
    crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
)> {
    let bindings = crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(
        RuntimeHandoffKind::ClaudeTui,
    );
    for (tmux_session_name, binding) in &bindings {
        close_children_finished_in_transcript(
            shared,
            tmux_session_name,
            Path::new(&binding.output_path),
        )
        .await;
    }
    bindings
}

/// Applies every terminal `<task-notification>` written to `transcript_path`
/// since the last read to the child its tool call opened under this session.
async fn close_children_finished_in_transcript(
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    transcript_path: &Path,
) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return;
    };
    let Ok(len) = std::fs::metadata(transcript_path).map(|metadata| metadata.len()) else {
        return;
    };
    let parent = super::super::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        &ProviderKind::Claude,
        tmux_session_name,
    );
    let observed = completion_cursors().get(tmux_session_name).cloned();
    let known = observed
        .as_ref()
        .filter(|cursor| cursor.path == transcript_path && cursor.offset <= len)
        .map(|cursor| cursor.offset);
    let start = match known {
        Some(offset) => offset,
        // Unread history can hold a completion only if a child is still open.
        None => match has_open_background_children_pg(pool, &parent).await {
            Ok(true) => 0,
            Ok(false) => len,
            Err(error) => {
                tracing::warn!(tmux_session_name, %error,
                    "background child completion scan deferred; open-child lookup failed");
                return;
            }
        },
    };
    let mut cursor = start;
    if start < len {
        let path = transcript_path.to_path_buf();
        let Ok(Ok((completions, end))) =
            tokio::task::spawn_blocking(move || read_child_completions(&path, start)).await
        else {
            return;
        };
        cursor = end;
        for completion in completions {
            let close_status = close_status_for(&completion.status);
            match close_background_child_for_tool_use_pg(
                pool,
                &parent,
                &completion.tool_use_id,
                close_status,
            )
            .await
            {
                Ok(Some(child_session_id)) => tracing::info!(
                    tmux_session_name,
                    tool_use_id = %completion.tool_use_id,
                    child_session_id,
                    close_status,
                    "closed background child from its transcript completion"
                ),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(tmux_session_name, tool_use_id = %completion.tool_use_id,
                        %error, "background child close failed; retrying from this record");
                    cursor = completion.line_start;
                    break;
                }
            }
        }
    }
    let mut cursors = completion_cursors();
    // A registration retry may rewind while this scan is awaiting the database.
    if cursors.get(tmux_session_name) == observed.as_ref() {
        cursors.insert(
            tmux_session_name.to_string(),
            CompletionCursor {
                path: transcript_path.to_path_buf(),
                offset: cursor,
                revision: observed.map_or(0, |cursor| cursor.revision.wrapping_add(1)),
            },
        );
    }
}

/// Closes a child the bridge just registered when the turn's transcript already
/// holds its terminal completion; returns whether the child is now closed.
pub(in crate::services::discord) async fn registered_child_already_finished(
    pool: &sqlx::PgPool,
    parent_session_key: &str,
    tool_use_id: Option<&str>,
    turn: &InflightTurnState,
) -> bool {
    let (Some(RuntimeHandoffKind::ClaudeTui), Some(tool_use_id), Some(path)) = (
        turn.runtime_kind,
        tool_use_id.map(str::trim).filter(|id| !id.is_empty()),
        turn.output_path.clone(),
    ) else {
        return false;
    };
    // Old restored turns without a start offset require a full transcript read.
    let from = turn
        .turn_start_offset
        .map_or(0, |offset| offset.min(turn.last_offset));
    let scan_path = path.clone();
    let Ok(Ok((completions, _))) = tokio::task::spawn_blocking(move || {
        let len = std::fs::metadata(&scan_path)?.len();
        read_child_completions(Path::new(&scan_path), if from > len { 0 } else { from })
    })
    .await
    else {
        schedule_registration_retry(turn, Path::new(&path), from);
        return false;
    };
    let Some(completion) = completions
        .into_iter()
        .find(|completion| completion.tool_use_id == tool_use_id)
    else {
        // A pre-registration scan may have sampled EOF inside this record.
        schedule_registration_retry(turn, Path::new(&path), from);
        return false;
    };
    match close_background_child_for_tool_use_pg(
        pool,
        parent_session_key,
        tool_use_id,
        close_status_for(&completion.status),
    )
    .await
    {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(parent_session_key, tool_use_id, %error,
                "background child finished before registration but its close failed");
            schedule_registration_retry(turn, Path::new(&path), from);
            false
        }
    }
}

fn schedule_registration_retry(turn: &InflightTurnState, path: &Path, from: u64) {
    let Some(tmux) = turn.tmux_session_name.as_deref() else {
        return;
    };
    let mut cursors = completion_cursors();
    let cursor = cursors.entry(tmux.to_owned()).or_insert(CompletionCursor {
        path: path.to_path_buf(),
        offset: from,
        revision: 0,
    });
    cursor.offset = if cursor.path == path {
        cursor.offset.min(from)
    } else {
        from
    };
    cursor.path = path.to_path_buf();
    cursor.revision = cursor.revision.wrapping_add(1);
}

fn close_status_for(notification_status: &str) -> &'static str {
    if super::super::placeholder_live_events::notification_is_error(notification_status) {
        "aborted"
    } else {
        "completed"
    }
}

fn completion_cursors()
-> std::sync::MutexGuard<'static, std::collections::HashMap<String, CompletionCursor>> {
    COMPLETION_CURSORS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Process restart: every read position is forgotten.
#[cfg(test)]
pub(in crate::services::discord) fn forget_completion_cursors_for_tests() {
    completion_cursors().clear();
}

/// Reads complete JSONL records from `start` and returns their terminal child
/// completions plus the offset just past the last complete record.
fn read_child_completions(path: &Path, start: u64) -> std::io::Result<(Vec<ChildCompletion>, u64)> {
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut offset = start.saturating_sub(1);
    reader.seek(SeekFrom::Start(offset))?;
    let mut line = Vec::new();
    if start > 0 {
        // Unless the byte before `start` ends a record, `start` is mid-record
        // (an EOF taken mid-write) and that partial record is skipped.
        let read = reader.read_until(b'\n', &mut line)?;
        if line.last() != Some(&b'\n') {
            return Ok((Vec::new(), start));
        }
        offset += read as u64;
    }
    let mut completions = Vec::new();
    loop {
        line.clear();
        let read = reader.read_until(b'\n', &mut line)?;
        if line.last() != Some(&b'\n') {
            break;
        }
        if let Some((tool_use_id, status)) = std::str::from_utf8(&line)
            .ok()
            .and_then(child_completion_from_record)
        {
            completions.push(ChildCompletion {
                line_start: offset,
                tool_use_id,
                status,
            });
        }
        offset += read as u64;
    }
    Ok((completions, offset))
}

/// A terminal `<task-notification>` naming its launching tool call, delivered
/// as a user record (idle, including `isMeta`) or a queued mid-turn attachment.
fn child_completion_from_record(line: &str) -> Option<(String, String)> {
    if !line.contains("task-notification") {
        return None;
    }
    let record: Value = serde_json::from_str(line).ok()?;
    let text = match record.get("type")?.as_str()? {
        "user" => match record.get("message")?.get("content")? {
            Value::String(text) => text.as_str(),
            Value::Array(items) => items.iter().find_map(|item| {
                (item.get("type")?.as_str()? == "text").then(|| item.get("text")?.as_str())?
            })?,
            _ => return None,
        },
        "attachment" => {
            let attachment = record.get("attachment")?;
            if attachment.get("type")?.as_str()? != "queued_command" {
                return None;
            }
            attachment.get("prompt")?.as_str()?
        }
        _ => return None,
    };
    if !injected_prompt_policy::is_start_anchored_task_notification(text) {
        return None;
    }
    let notification = super::super::tui_task_card::parse_task_notification(text);
    let status = notification.status?;
    if !super::super::placeholder_live_events::notification_is_terminal(&status) {
        return None;
    }
    Some((notification.tool_use_id?, status))
}
