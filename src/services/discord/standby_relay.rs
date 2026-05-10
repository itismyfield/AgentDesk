//! Phase 5.3 of intake-node-routing (issue #2011): standalone JSONL → Discord
//! relay task for cluster-standby nodes.
//!
//! On the leader, the tmux watcher (`tmux_watcher.rs`) handles streaming
//! agent output to Discord. The watcher's relay path has many gateway-coupled
//! assumptions (cached cache, inflight reconciliation, monitor-auto-turn
//! claims, recent_stop suppression, paused/pause_epoch coordination, etc.)
//! that don't hold on cluster-standby nodes. Phase 5.2 made the watcher
//! *start* on standby via `serenity_http_or_token_fallback()`, but the watcher's
//! relay step still doesn't fire on standby in production (verified
//! 2026-05-10 with channel `1475086789696946196` outbox_id=2: response sat in
//! tmux indefinitely while the placeholder froze at "응답 처리 중").
//!
//! Phase 5.3 takes the simpler, more robust path: when on standby, skip the
//! watcher entirely and run this self-contained relay loop instead. It
//! polls the agent's JSONL output file for the `{"type":"result"}` event,
//! extracts the assistant response, and posts it to Discord via REST
//! (replacing the bridge-allocated placeholder when one is known, otherwise
//! sending a new channel message). No reliance on cached_serenity_ctx,
//! inflight reconciliation, or any of the watcher's leader-only state
//! machinery.
//!
//! Leader path is unchanged.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::Value;
use serenity::model::id::{ChannelId, MessageId};

use super::SharedData;
use super::formatting;
use crate::services::provider::ProviderKind;

const POLL_INTERVAL: Duration = Duration::from_millis(500);
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(900); // 15 min
const MAX_FILE_BYTES_PER_TICK: u64 = 1_048_576; // 1 MiB safety cap

/// Spawned per-turn on cluster-standby nodes. Returns when:
/// - `cancel` or `shared.shutting_down` flips to true,
/// - the JSONL emits a `{"type":"result"}` event and we deliver the response,
/// - or `timeout` elapses.
pub(super) async fn run_standby_relay(
    http: Arc<serenity::http::Http>,
    channel_id: ChannelId,
    placeholder_msg_id: Option<MessageId>,
    output_path: String,
    start_offset: u64,
    cancel: Arc<AtomicBool>,
    shared: Arc<SharedData>,
    provider: ProviderKind,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    let mut current_offset = start_offset;
    // Buffer for incomplete trailing line across reads.
    let mut tail_buf = String::new();
    let ts_start = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts_start}] 👁 standby_relay started for channel {} from offset {} (placeholder={:?})",
        channel_id.get(),
        start_offset,
        placeholder_msg_id.map(|m| m.get())
    );

    loop {
        if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 standby_relay cancelled for channel {} (offset={})",
                channel_id.get(),
                current_offset
            );
            return;
        }
        if Instant::now() > deadline {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ standby_relay timeout for channel {} (offset={}, no result event)",
                channel_id.get(),
                current_offset
            );
            return;
        }

        let file_size = match std::fs::metadata(&output_path) {
            Ok(meta) => meta.len(),
            Err(_) => {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        if file_size <= current_offset {
            tokio::time::sleep(POLL_INTERVAL).await;
            continue;
        }

        let read_to = (current_offset + MAX_FILE_BYTES_PER_TICK).min(file_size);
        let new_chunk = match read_file_range(&output_path, current_offset, read_to) {
            Ok(s) => s,
            Err(_) => {
                tokio::time::sleep(POLL_INTERVAL).await;
                continue;
            }
        };
        current_offset = read_to;

        // Stitch any prior incomplete tail with the new chunk and process
        // line-by-line. Keep the trailing partial line (no '\n') for next tick.
        let stitched = if tail_buf.is_empty() {
            new_chunk
        } else {
            let mut s = std::mem::take(&mut tail_buf);
            s.push_str(&new_chunk);
            s
        };
        let mut last_complete_end = 0usize;
        let bytes = stitched.as_bytes();
        for (idx, b) in bytes.iter().enumerate() {
            if *b == b'\n' {
                let line = &stitched[last_complete_end..idx];
                last_complete_end = idx + 1;
                if line.trim().is_empty() {
                    continue;
                }
                if let Some(result_text) = extract_result_text(line) {
                    deliver_response(
                        &http,
                        channel_id,
                        placeholder_msg_id,
                        &shared,
                        &provider,
                        &result_text,
                    )
                    .await;
                    return;
                }
            }
        }
        if last_complete_end < stitched.len() {
            tail_buf = stitched[last_complete_end..].to_string();
        }

        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn read_file_range(path: &str, start: u64, end: u64) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(start))?;
    let len = end.saturating_sub(start) as usize;
    let mut buf = vec![0u8; len];
    let read = file.read(&mut buf)?;
    buf.truncate(read);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

fn extract_result_text(line: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(line.trim()).ok()?;
    if parsed.get("type").and_then(Value::as_str) != Some("result") {
        return None;
    }
    let result_text = parsed.get("result").and_then(Value::as_str)?;
    let trimmed = result_text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(result_text.to_string())
}

async fn deliver_response(
    http: &Arc<serenity::http::Http>,
    channel_id: ChannelId,
    placeholder_msg_id: Option<MessageId>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    response_text: &str,
) {
    let formatted = if shared.status_panel_v2_enabled {
        formatting::format_for_discord_with_status_panel(response_text, provider)
    } else {
        formatting::format_for_discord_with_provider(response_text, provider)
    };
    let chars = formatted.chars().count();

    match placeholder_msg_id {
        Some(msg_id) => {
            let outcome = formatting::replace_long_message_raw_with_outcome(
                http, channel_id, msg_id, &formatted, shared,
            )
            .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            match outcome {
                Ok(_) => tracing::info!(
                    "  [{ts}] 👁 standby_relay ✓ delivered terminal response (edit) channel {} msg {} ({} chars)",
                    channel_id.get(),
                    msg_id.get(),
                    chars
                ),
                Err(e) => tracing::warn!(
                    "  [{ts}] ⚠ standby_relay edit failed for channel {} msg {}: {e}",
                    channel_id.get(),
                    msg_id.get()
                ),
            }
        }
        None => {
            let result =
                formatting::send_long_message_raw(http, channel_id, &formatted, shared).await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            match result {
                Ok(()) => tracing::info!(
                    "  [{ts}] 👁 standby_relay ✓ delivered terminal response (new message) channel {} ({} chars)",
                    channel_id.get(),
                    chars
                ),
                Err(e) => tracing::warn!(
                    "  [{ts}] ⚠ standby_relay send failed for channel {}: {e}",
                    channel_id.get()
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_result_text_returns_none_for_non_result_lines() {
        let line = r#"{"type":"assistant","message":{"content":[{"text":"hi"}]}}"#;
        assert!(extract_result_text(line).is_none());
    }

    #[test]
    fn extract_result_text_returns_text_for_result_subtype_success() {
        let line = r#"{"type":"result","subtype":"success","result":"hello"}"#;
        assert_eq!(extract_result_text(line).as_deref(), Some("hello"));
    }

    #[test]
    fn extract_result_text_skips_empty_result() {
        let line = r#"{"type":"result","subtype":"success","result":"   "}"#;
        assert!(extract_result_text(line).is_none());
    }

    #[test]
    fn extract_result_text_handles_invalid_json() {
        assert!(extract_result_text("not json").is_none());
        assert!(extract_result_text("").is_none());
    }
}
