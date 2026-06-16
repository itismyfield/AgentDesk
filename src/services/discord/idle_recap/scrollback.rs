//! Scrollback capture and transcript parsing for the idle-recap summarizer.
//!
//! Extracted verbatim from the parent `idle_recap` module (#3479): tmux pane
//! capture, the `claude-e` transcript-tail fallback, JSONL line parsing, and
//! the Claude Haiku summarizer call. Behavior is unchanged — only the module
//! boundary moved.

use super::*;

/// Best-effort tail capture of the live tmux pane via `tmux capture-pane`.
/// Returns `None` if the session is gone or the binary is unavailable —
/// the caller treats that as "no scrollback, post header-only".
pub(crate) async fn capture_tmux_scrollback(session_name: &str) -> Option<String> {
    let session = session_name.to_string();
    task::spawn_blocking(move || {
        std::process::Command::new("tmux")
            .args([
                "capture-pane",
                "-p",
                "-J",
                "-S",
                &format!("-{TMUX_SCROLLBACK_LINES}"),
                "-t",
                &session,
            ])
            .output()
            .ok()
            .and_then(|out| {
                if out.status.success() {
                    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
                } else {
                    None
                }
            })
    })
    .await
    .ok()
    .flatten()
    .filter(|s| !s.is_empty())
}

/// Fallback scrollback source for runtimes without a live tmux pane —
/// notably the `claude-e` per-turn spawn runtime, which never attaches a
/// long-lived tmux session. Reads the Claude transcript JSONL at
/// `~/.claude/projects/<encoded-cwd>/<session_id>.jsonl`, parses each line,
/// and emits the last ~`TMUX_SCROLLBACK_LINES` user/assistant text turns in
/// a `[role] text` shape that the recap summarizer can consume the same
/// way it consumes tmux scrollback.
///
/// Returns `None` when the transcript is missing, unreadable, contains no
/// human-readable turns, or `session_id` is not a valid UUID. The recap
/// pipeline degrades gracefully to a header-only card in that case.
///
/// As a free bonus this also covers stale tmux sessions whose pane has
/// already been torn down: the transcript file outlives the tmux pane.
pub(crate) async fn capture_transcript_scrollback(
    cwd: &std::path::Path,
    session_id: &str,
) -> Option<String> {
    let transcript_path =
        crate::services::claude_tui::transcript_tail::claude_transcript_path(cwd, session_id, None)
            .ok()?;
    let path_for_blocking = transcript_path.clone();
    task::spawn_blocking(move || extract_transcript_tail_text(&path_for_blocking))
        .await
        .ok()
        .flatten()
}

/// Synchronous worker for `capture_transcript_scrollback`. Splits out so
/// the parsing logic is unit-testable without an async runtime.
pub(super) fn extract_transcript_tail_text(transcript_path: &std::path::Path) -> Option<String> {
    use std::collections::VecDeque;
    use std::io::BufRead;

    let file = std::fs::File::open(transcript_path).ok()?;
    let reader = std::io::BufReader::new(file);
    let cap = TMUX_SCROLLBACK_LINES as usize;
    let mut buf: VecDeque<String> = VecDeque::with_capacity(cap);
    for line in reader.lines().map_while(Result::ok) {
        let Some(entry) = parse_transcript_line_text(&line) else {
            continue;
        };
        if buf.len() == cap {
            buf.pop_front();
        }
        buf.push_back(entry);
    }
    if buf.is_empty() {
        None
    } else {
        Some(buf.into_iter().collect::<Vec<_>>().join("\n"))
    }
}

/// Extract a `[role] text` line from a single Claude transcript JSONL row.
/// Returns `None` for rows without human-readable content (init/done/status,
/// tool uses, tool results, attachments, etc.) so the recap summarizer
/// only sees signal.
pub(super) fn parse_transcript_line_text(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(trimmed).ok()?;
    let role = match value.get("type")?.as_str()? {
        "user" => "user",
        "assistant" => "assistant",
        _ => return None,
    };
    let content = value.get("message")?.get("content")?;
    let text = match content {
        serde_json::Value::String(s) => s.trim().to_string(),
        serde_json::Value::Array(blocks) => {
            let mut parts = Vec::new();
            for block in blocks {
                if block.get("type").and_then(|t| t.as_str()) != Some("text") {
                    continue;
                }
                if let Some(piece) = block.get("text").and_then(|t| t.as_str()) {
                    let piece = piece.trim();
                    if !piece.is_empty() {
                        parts.push(piece.to_string());
                    }
                }
            }
            parts.join(" ")
        }
        _ => return None,
    };
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(format!("[{role}] {text}"))
}

/// Ask Claude Haiku for a 1-2 sentence Korean recap. Time-bounded; returns
/// `None` on any failure so the caller can fall back to a header-only card.
///
/// Previously this routed to a local `opencode serve` (Gemma 27B) build,
/// but resident memory on the mac-book host was the bottleneck. Haiku 4.5
/// is cheap enough per call (a few cents per million tokens) and fast
/// enough on remote API that it comfortably fits inside the 20s budget
/// without holding any RAM on the host.
///
/// The Claude call is wrapped in `spawn_blocking`. A `tokio::time::timeout`
/// alone would only cancel the *await* on the JoinHandle and leave the
/// blocking thread running with a live `claude` subprocess. So we also
/// pass a `CancelToken` into the Claude wrapper and *signal it* from the
/// timeout watchdog. The Claude simple-cancel watcher polls
/// `cancel_requested` and tears down the child process tree as soon as it
/// sees the flag.
pub(crate) async fn summarize_with_haiku(scrollback: &str) -> Option<String> {
    if scrollback.is_empty() {
        return None;
    }
    let prompt = format!(
        "다음은 AI 코딩 에이전트와 사용자의 마지막 대화 ~100줄입니다. \
         사용자가 지금 다시 돌아왔을 때 \"어떤 작업을 하던 중이었는지\"를 \
         즉시 기억할 수 있도록 1-2문장으로 한국어 요약을 만드세요. \
         도구 호출 / 스크롤 / 진행 표시 같은 노이즈는 무시하고 \
         실제 작업 내용(파일·결정·다음 단계)에 집중하세요. \
         결과만 출력하고 다른 말은 붙이지 마세요.\n\n---\n\n{scrollback}",
    );

    let cancel = std::sync::Arc::new(CancelToken::new());
    let cancel_for_blocking = cancel.clone();
    let join = task::spawn_blocking(move || {
        crate::services::claude::execute_command_simple_cancellable_with_model(
            &prompt,
            Some(RECAP_SUMMARY_MODEL),
            Some(cancel_for_blocking),
        )
    });

    let result = match tokio::time::timeout(RECAP_SUMMARY_TIMEOUT, join).await {
        Ok(join_result) => match join_result {
            Ok(Ok(text)) => text,
            Ok(Err(_)) => return None,
            Err(_) => return None,
        },
        Err(_) => {
            // Timeout fired. Signal the cancel token so the blocking
            // closure exits at the next Claude wrapper poll and the
            // spawned child tree is reaped.
            cancel.cancel_with_tmux_cleanup();
            return None;
        }
    };

    let trimmed = result.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}
