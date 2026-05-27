//! Idle-recap renderer (PR #3b) — turns a "5-min idle" trigger into a
//! Discord card under the originating channel.
//!
//! Lifecycle:
//!   1. The policy module `policies/timeouts/idle-recap.js` calls
//!      `POST /api/sessions/{key}/idle-recap` every 5 minutes for each
//!      eligible main-channel session.
//!   2. `post_recap` (here) captures the tail of the tmux scrollback, asks
//!      Claude Haiku for a short Korean summary (graceful fallback to the raw
//!      tail if the model call is unavailable), and posts a single-line notify-bot
//!      message of the form
//!         📦 {used}/{window} tokens ({pct}%) · idle {dur} · {summary}
//!   3. The previous recap card (if any) for the same channel is deleted
//!      best-effort, and the new message id is persisted on `sessions`.
//!   4. The next user message in that channel deletes the card — handled
//!      by `message_handler::clear_idle_recap_for_channel`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use poise::serenity_prelude::{
    self as serenity, ButtonKind, ButtonStyle, ChannelId, CreateActionRow, CreateButton, MessageId,
};
use sqlx::PgPool;
use tokio::task;

use crate::services::provider::{CancelToken, ProviderKind};

const FALLBACK_CONTEXT_WINDOW_TOKENS: u64 = 128_000;
const SESSION_TOKEN_FRESHNESS_MAX_SECS: i64 = 30 * 60;

/// Lines of tmux scrollback captured for the recap summary. Inherited from
/// the previous opencode-backed implementation that used a local Gemma 27B
/// build: empirical timing on that model (2026-05-14) showed 500 → 20s+
/// timeout, 100 → ~8s, 50 → ~5s. 100 was the smallest cap that still
/// covered a user+assistant turn pair with surrounding chrome. Keeping the
/// same cap for the Claude Haiku 4.5 summarizer (#2802 follow-up) — the
/// remote API is fast enough that input size is no longer the bottleneck,
/// but 100 lines remains a sensible content budget for a 1-2 sentence
/// summary and bounds API token cost.
const TMUX_SCROLLBACK_LINES: i64 = 100;
const RECAP_SUMMARY_TIMEOUT: Duration = Duration::from_secs(20);
/// Cheap, fast model for the idle-recap summary. Local mac-book runs out of
/// RAM when we keep `opencode serve` (Gemma 27B) resident, so we route the
/// 1-2 sentence summary to the remote Claude Haiku API instead.
const RECAP_SUMMARY_MODEL: &str = "claude-haiku-4-5-20251001";

/// Custom-id prefix for the `[새 세션 시작]` button on a recap card. The
/// numeric suffix is the Discord message id of the card; the interaction
/// handler resolves it back to a `session_key` via the
/// `sessions.idle_recap_message_id` index.
pub(crate) const IDLE_RECAP_CLEAR_BUTTON_PREFIX: &str = "idle_recap:clear:";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RecapLiveContextUsage {
    pub(crate) used_tokens: u64,
    pub(crate) context_window_tokens: u64,
}

/// Snapshot of the data the recap renderer needs in a single SQL round-trip.
#[derive(Debug, Clone)]
pub(crate) struct RecapSnapshot {
    pub(crate) session_key: String,
    pub(crate) provider: String,
    pub(crate) model: Option<String>,
    pub(crate) tokens: Option<i64>,
    pub(crate) tokens_updated_at: Option<DateTime<Utc>>,
    pub(crate) last_heartbeat: Option<DateTime<Utc>>,
    pub(crate) claude_session_id: Option<String>,
    pub(crate) raw_provider_session_id: Option<String>,
    pub(crate) live_context_usage: Option<RecapLiveContextUsage>,
    pub(crate) latest_turn_provider: Option<String>,
    pub(crate) latest_turn_session_key: Option<String>,
    pub(crate) latest_turn_session_id: Option<String>,
    pub(crate) latest_turn_finished_at: Option<DateTime<Utc>>,
    pub(crate) latest_turn_input_tokens: Option<i64>,
    pub(crate) latest_turn_cache_create_tokens: Option<i64>,
    pub(crate) latest_turn_cache_read_tokens: Option<i64>,
    pub(crate) previous_message_id: Option<i64>,
    pub(crate) previous_channel_id: Option<i64>,
    pub(crate) discord_channel_id: Option<String>,
    pub(crate) discord_channel_cc: Option<String>,
    pub(crate) discord_channel_cdx: Option<String>,
    pub(crate) discord_channel_alt: Option<String>,
    /// Working directory of the provider session, persisted on `sessions.cwd`.
    /// Used as the fallback source for transcript-based scrollback when no
    /// live tmux pane exists (e.g. the `claude-e` per-turn spawn runtime).
    pub(crate) cwd: Option<String>,
}

impl RecapSnapshot {
    pub(crate) fn has_resumable_provider_session(&self) -> bool {
        self.claude_session_id
            .as_deref()
            .map(str::trim)
            .is_some_and(|value| !value.is_empty())
            || self
                .raw_provider_session_id
                .as_deref()
                .map(str::trim)
                .is_some_and(|value| !value.is_empty())
    }
}

/// Load everything the renderer needs in one SQL hit.
pub(crate) async fn load_recap_snapshot(
    pool: &PgPool,
    session_key: &str,
) -> Result<Option<RecapSnapshot>, sqlx::Error> {
    sqlx::query_as::<_, RecapSnapshotRow>(
        "SELECT s.session_key,
                s.provider,
                s.model,
                s.tokens,
                s.tokens_updated_at,
                s.last_heartbeat,
                s.claude_session_id,
                s.raw_provider_session_id,
                s.cwd,
                s.idle_recap_message_id,
                s.idle_recap_channel_id,
                a.discord_channel_id,
                a.discord_channel_cc,
                a.discord_channel_cdx,
                a.discord_channel_alt,
                lt.provider AS latest_turn_provider,
                lt.session_key AS latest_turn_session_key,
                lt.session_id AS latest_turn_session_id,
                lt.finished_at AS latest_turn_finished_at,
                lt.input_tokens::BIGINT AS latest_turn_input_tokens,
                lt.cache_create_tokens::BIGINT AS latest_turn_cache_create_tokens,
                lt.cache_read_tokens::BIGINT AS latest_turn_cache_read_tokens
         FROM sessions s
         LEFT JOIN agents a ON a.id = s.agent_id
         LEFT JOIN LATERAL (
            SELECT t.provider,
                   t.session_key,
                   t.session_id,
                   t.finished_at,
                   t.input_tokens,
                   t.cache_create_tokens,
                   t.cache_read_tokens
            FROM turns t
            WHERE lower(COALESCE(t.provider, '')) = lower(COALESCE(s.provider, ''))
              AND (
                t.session_key = s.session_key
                OR (
                    s.claude_session_id IS NOT NULL
                    AND BTRIM(s.claude_session_id) != ''
                    AND t.session_id = s.claude_session_id
                )
                OR (
                    s.raw_provider_session_id IS NOT NULL
                    AND BTRIM(s.raw_provider_session_id) != ''
                    AND t.session_id = s.raw_provider_session_id
                )
              )
            ORDER BY t.finished_at DESC, t.started_at DESC, t.created_at DESC
            LIMIT 1
         ) lt ON true
         WHERE s.session_key = $1",
    )
    .bind(session_key)
    .fetch_optional(pool)
    .await
    .map(|row| row.map(RecapSnapshotRow::into_snapshot))
}

#[derive(Debug, sqlx::FromRow)]
struct RecapSnapshotRow {
    session_key: String,
    provider: String,
    model: Option<String>,
    tokens: Option<i64>,
    tokens_updated_at: Option<DateTime<Utc>>,
    last_heartbeat: Option<DateTime<Utc>>,
    claude_session_id: Option<String>,
    raw_provider_session_id: Option<String>,
    cwd: Option<String>,
    idle_recap_message_id: Option<i64>,
    idle_recap_channel_id: Option<i64>,
    discord_channel_id: Option<String>,
    discord_channel_cc: Option<String>,
    discord_channel_cdx: Option<String>,
    discord_channel_alt: Option<String>,
    latest_turn_provider: Option<String>,
    latest_turn_session_key: Option<String>,
    latest_turn_session_id: Option<String>,
    latest_turn_finished_at: Option<DateTime<Utc>>,
    latest_turn_input_tokens: Option<i64>,
    latest_turn_cache_create_tokens: Option<i64>,
    latest_turn_cache_read_tokens: Option<i64>,
}

impl RecapSnapshotRow {
    fn into_snapshot(self) -> RecapSnapshot {
        RecapSnapshot {
            session_key: self.session_key,
            provider: self.provider,
            model: self.model,
            tokens: self.tokens,
            tokens_updated_at: self.tokens_updated_at,
            last_heartbeat: self.last_heartbeat,
            claude_session_id: self.claude_session_id,
            raw_provider_session_id: self.raw_provider_session_id,
            live_context_usage: None,
            latest_turn_provider: self.latest_turn_provider,
            latest_turn_session_key: self.latest_turn_session_key,
            latest_turn_session_id: self.latest_turn_session_id,
            latest_turn_finished_at: self.latest_turn_finished_at,
            latest_turn_input_tokens: self.latest_turn_input_tokens,
            latest_turn_cache_create_tokens: self.latest_turn_cache_create_tokens,
            latest_turn_cache_read_tokens: self.latest_turn_cache_read_tokens,
            previous_message_id: self.idle_recap_message_id,
            previous_channel_id: self.idle_recap_channel_id,
            discord_channel_id: self.discord_channel_id,
            discord_channel_cc: self.discord_channel_cc,
            discord_channel_cdx: self.discord_channel_cdx,
            discord_channel_alt: self.discord_channel_alt,
            cwd: self.cwd,
        }
    }
}

/// Pick the Discord channel the recap card should be posted to.
///
/// Mirrors `AgentChannelBindings::channel_for_provider` in `src/db/agents.rs`
/// but operates on the flat `RecapSnapshot` (one SQL hit). Returns the
/// numeric Discord channel id parsed as `u64`.
pub(crate) fn resolve_post_channel(snapshot: &RecapSnapshot) -> Option<u64> {
    let candidate = match ProviderKind::from_str(&snapshot.provider) {
        Some(ProviderKind::Claude) => snapshot
            .discord_channel_cc
            .as_deref()
            .or(snapshot.discord_channel_id.as_deref()),
        Some(ProviderKind::Codex) => snapshot
            .discord_channel_cdx
            .as_deref()
            .or(snapshot.discord_channel_alt.as_deref()),
        _ => snapshot
            .discord_channel_id
            .as_deref()
            .or(snapshot.discord_channel_cc.as_deref()),
    }?;
    candidate.trim().parse::<u64>().ok()
}

pub(crate) async fn attach_live_context_usage(
    registry: &super::health::HealthRegistry,
    snapshot: &mut RecapSnapshot,
    channel_id: u64,
) {
    let Some(provider) = ProviderKind::from_str(&snapshot.provider) else {
        return;
    };
    let Some(shared) = registry.shared_for_provider(&provider).await else {
        return;
    };
    let Some(live) = shared
        .placeholder_live_events
        .context_panel_snapshot(ChannelId::new(channel_id))
    else {
        return;
    };
    let live_session_matches = live
        .provider_session_id
        .as_deref()
        .and_then(normalized_text)
        .is_some_and(|session_id| {
            provider_session_ids(snapshot).any(|active| active == session_id)
        });
    if live_session_matches && live.used_tokens > 0 && live.context_window_tokens > 0 {
        snapshot.live_context_usage = Some(RecapLiveContextUsage {
            used_tokens: live.used_tokens,
            context_window_tokens: live.context_window_tokens,
        });
    }
}

/// Compose the recap card body. PR #3b shipped a header-only card; PR #3c
/// adds an optional `summary` line below the header (rendered as a Discord
/// blockquote when present).
pub(crate) fn compose_recap_text(snapshot: &RecapSnapshot, summary: Option<&str>) -> String {
    let header = compose_recap_header(snapshot);
    match summary.map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => {
            // Discord blockquote — single `>` on each line. Keep the
            // summary on one line to avoid breaking the blockquote layout.
            let single_line = s.replace('\n', " ");
            format!("{header}\n> {single_line}")
        }
        None => header,
    }
}

fn compose_recap_header(snapshot: &RecapSnapshot) -> String {
    let now = Utc::now();
    let idle_since = snapshot
        .last_heartbeat
        .map(|t| format_korean_duration(now - t))
        .unwrap_or_else(|| "방금 전".to_string());

    match select_recap_context(snapshot, now) {
        RecapContextDisplay::Known { used, window } => {
            let used_label = format_token_count(used);
            let window_label = format_token_count(window);
            let pct = if window == 0 {
                None
            } else {
                Some(((u128::from(used) * 100) / u128::from(window)).min(100) as u64)
            };
            match pct {
                Some(percent) if used > window => {
                    format!(
                        "📦 {used_label} / {window_label} tokens ({percent}%+, over limit) · idle {idle_since}"
                    )
                }
                Some(percent) => {
                    format!(
                        "📦 {used_label} / {window_label} tokens ({percent}%) · idle {idle_since}"
                    )
                }
                None => format!("📦 context unknown · idle {idle_since}"),
            }
        }
        RecapContextDisplay::Stale => format!("📦 context stale · idle {idle_since}"),
        RecapContextDisplay::Unknown => format!("📦 context unknown · idle {idle_since}"),
    }
}

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
fn extract_transcript_tail_text(transcript_path: &std::path::Path) -> Option<String> {
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
fn parse_transcript_line_text(line: &str) -> Option<String> {
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

fn format_token_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecapContextDisplay {
    Known { used: u64, window: u64 },
    Stale,
    Unknown,
}

fn select_recap_context(snapshot: &RecapSnapshot, now: DateTime<Utc>) -> RecapContextDisplay {
    if let Some(live) = snapshot.live_context_usage {
        if live.used_tokens > 0 && live.context_window_tokens > 0 {
            return RecapContextDisplay::Known {
                used: live.used_tokens,
                window: live.context_window_tokens,
            };
        }
    }

    let window = context_window_for(snapshot);
    if let Some(used) = latest_turn_context_tokens(snapshot) {
        return RecapContextDisplay::Known { used, window };
    }

    if session_tokens_are_stale_or_incompatible(snapshot, now) {
        return RecapContextDisplay::Stale;
    }

    if let Some(used) = fresh_session_tokens(snapshot, now) {
        return RecapContextDisplay::Known { used, window };
    }

    RecapContextDisplay::Unknown
}

fn latest_turn_context_tokens(snapshot: &RecapSnapshot) -> Option<u64> {
    if !latest_turn_matches_active_session(snapshot) {
        return None;
    }
    let input = non_negative_i64_to_u64(snapshot.latest_turn_input_tokens?)?;
    let cache_create =
        non_negative_i64_to_u64(snapshot.latest_turn_cache_create_tokens.unwrap_or(0))?;
    let cache_read = non_negative_i64_to_u64(snapshot.latest_turn_cache_read_tokens.unwrap_or(0))?;
    let used = input
        .saturating_add(cache_create)
        .saturating_add(cache_read);
    (used > 0).then_some(used)
}

fn latest_turn_matches_active_session(snapshot: &RecapSnapshot) -> bool {
    if snapshot.latest_turn_finished_at.is_none() {
        return false;
    }
    if !same_normalized_opt(
        snapshot.latest_turn_provider.as_deref(),
        Some(snapshot.provider.as_str()),
    ) {
        return false;
    }
    if same_normalized_opt(
        snapshot.latest_turn_session_key.as_deref(),
        Some(snapshot.session_key.as_str()),
    ) {
        return true;
    }
    let latest_session_id = snapshot
        .latest_turn_session_id
        .as_deref()
        .and_then(normalized_text);
    let Some(latest_session_id) = latest_session_id else {
        return false;
    };
    provider_session_ids(snapshot).any(|session_id| session_id == latest_session_id)
}

fn provider_session_ids(snapshot: &RecapSnapshot) -> impl Iterator<Item = &str> {
    [
        snapshot.claude_session_id.as_deref(),
        snapshot.raw_provider_session_id.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter_map(normalized_text)
}

fn session_tokens_are_stale_or_incompatible(snapshot: &RecapSnapshot, now: DateTime<Utc>) -> bool {
    let Some(tokens) = snapshot.tokens.filter(|value| *value > 0) else {
        return false;
    };
    if non_negative_i64_to_u64(tokens).is_none() {
        return true;
    }
    if snapshot.latest_turn_finished_at.is_some() && !latest_turn_matches_active_session(snapshot) {
        return true;
    }
    !session_tokens_are_fresh(snapshot, now)
}

fn fresh_session_tokens(snapshot: &RecapSnapshot, now: DateTime<Utc>) -> Option<u64> {
    let tokens = non_negative_i64_to_u64(snapshot.tokens?)?;
    if tokens == 0 || !session_tokens_are_fresh(snapshot, now) {
        return None;
    }
    Some(tokens)
}

fn session_tokens_are_fresh(snapshot: &RecapSnapshot, now: DateTime<Utc>) -> bool {
    let Some(updated_at) = snapshot.tokens_updated_at else {
        return false;
    };
    let age = now - updated_at;
    age.num_seconds() >= 0 && age.num_seconds() <= SESSION_TOKEN_FRESHNESS_MAX_SECS
}

fn non_negative_i64_to_u64(value: i64) -> Option<u64> {
    u64::try_from(value).ok()
}

fn same_normalized_opt(left: Option<&str>, right: Option<&str>) -> bool {
    match (
        left.and_then(normalized_text),
        right.and_then(normalized_text),
    ) {
        (Some(left), Some(right)) => left.eq_ignore_ascii_case(right),
        _ => false,
    }
}

fn normalized_text(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn context_window_for(snapshot: &RecapSnapshot) -> u64 {
    match ProviderKind::from_str(&snapshot.provider) {
        Some(provider) => provider.resolve_context_window(snapshot.model.as_deref()),
        _ => FALLBACK_CONTEXT_WINDOW_TOKENS,
    }
}

fn format_korean_duration(dur: chrono::Duration) -> String {
    let secs = dur.num_seconds().max(0);
    if secs >= 86_400 {
        format!("{}일", secs / 86_400)
    } else if secs >= 3_600 {
        format!("{}시간", secs / 3_600)
    } else if secs >= 60 {
        format!("{}분", secs / 60)
    } else {
        format!("{}초", secs)
    }
}

/// Post the recap card via the configured notify bot. Routes through
/// `super::http::send_channel_message_with_components` so the
/// maintainability audit's `direct_discord_sends` rule (hard gate, #1282)
/// stays happy — that helper lives in the allowlisted `discord/http.rs`
/// module.
///
/// The recap card carries a single `[새 세션 시작]` button with a
/// `idle_recap:clear:<message_id>` custom id. The interaction handler
/// (see `idle_recap_interaction.rs`) resolves the suffix back to the
/// session_key via the `sessions.idle_recap_message_id` lookup and calls
/// the existing `adk_session::clear_provider_session_id` to perform the
/// explicit "start a fresh session" action the user opted into.
pub(crate) async fn post_recap_card(
    http: &serenity::Http,
    channel_id: u64,
    content: &str,
) -> Result<u64, String> {
    // We need the post-then-edit dance because the custom_id has to embed
    // the message id, but the message id is only known after Discord
    // assigns it. Step 1: post the card with a placeholder button whose
    // custom_id encodes a sentinel. Step 2: edit the card with the real
    // button. The brief window with the sentinel is harmless — the
    // interaction handler ignores unknown ids.
    let placeholder = make_recap_components("0");
    let msg = super::http::send_channel_message_with_components(
        http,
        ChannelId::new(channel_id),
        content,
        placeholder,
    )
    .await
    .map_err(|e| format!("send_message: {e}"))?;
    let id = msg.id.get();
    let real = make_recap_components(&id.to_string());
    if let Err(e) = super::http::edit_channel_message_with_components(
        http,
        ChannelId::new(channel_id),
        msg.id,
        content,
        real,
    )
    .await
    {
        // Edit failure here is non-fatal — the placeholder button is
        // harmless. Surface the error in logs for diagnostics.
        tracing::warn!(
            error = %e,
            channel_id = channel_id,
            message_id = id,
            "idle_recap: button id rewrite edit failed (placeholder button left in place)"
        );
    }
    Ok(id)
}

fn make_recap_components(message_id_suffix: &str) -> Vec<CreateActionRow> {
    let custom_id = format!("{IDLE_RECAP_CLEAR_BUTTON_PREFIX}{message_id_suffix}");
    vec![CreateActionRow::Buttons(vec![
        CreateButton::new(custom_id)
            .style(ButtonStyle::Secondary)
            .label("새 세션 시작"),
    ])]
}

fn content_looks_like_idle_recap_card(content: &str) -> bool {
    let trimmed = content.trim_start();
    trimmed == "📦" || trimmed.starts_with("📦 ")
}

fn component_is_idle_recap_clear_button(component: &serenity::ActionRowComponent) -> bool {
    match component {
        serenity::ActionRowComponent::Button(button) => match &button.data {
            ButtonKind::NonLink { custom_id, .. } => {
                custom_id.starts_with(IDLE_RECAP_CLEAR_BUTTON_PREFIX)
            }
            _ => false,
        },
        _ => false,
    }
}

fn message_is_idle_recap_card(message: &serenity::Message) -> bool {
    content_looks_like_idle_recap_card(&message.content)
        && message.components.iter().any(|row| {
            row.components
                .iter()
                .any(component_is_idle_recap_clear_button)
        })
}

/// Delete the previous recap card if one is recorded and still looks like an
/// idle-recap card. A stale/corrupt `sessions.idle_recap_message_id` must
/// never be allowed to delete a real turn response, so this probes Discord
/// first and only deletes messages carrying both the recap content marker and
/// the recap button custom id. Errors are swallowed so the renderer never
/// fails the cycle just because Discord has GC'd the old message itself.
/// Same allowlist rationale as `post_recap_card`.
pub(crate) async fn delete_previous_card(http: &serenity::Http, channel_id: u64, message_id: u64) {
    let channel = ChannelId::new(channel_id);
    let message = MessageId::new(message_id);
    match http.get_message(channel, message).await {
        Ok(current) if message_is_idle_recap_card(&current) => {
            let _ = super::http::delete_channel_message(http, channel, message).await;
        }
        Ok(current) => {
            tracing::warn!(
                channel_id = channel_id,
                message_id = message_id,
                author_id = current.author.id.get(),
                "idle_recap: preserving recorded message because it is not an idle recap card"
            );
        }
        Err(error) => {
            tracing::debug!(
                channel_id = channel_id,
                message_id = message_id,
                error = %error,
                "idle_recap: previous card probe failed; skipping destructive delete"
            );
        }
    }
}

/// Persist the freshly-posted message id (and the channel it lives in) so
/// the next cycle can delete it and `message_handler` can clear it the
/// moment the user sends their next turn.
pub(crate) async fn persist_recap_message_id(
    pool: &PgPool,
    session_key: &str,
    channel_id: u64,
    message_id: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE sessions
         SET idle_recap_message_id = $1,
             idle_recap_channel_id = $2,
             idle_recap_posted_at  = NOW()
         WHERE session_key = $3",
    )
    .bind(message_id as i64)
    .bind(channel_id as i64)
    .bind(session_key)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Stamp `idle_recap_posted_at = NOW()` for this cycle's dedupe window.
/// Called *before* the renderer runs so the policy treats this cycle as
/// "handled" even if the post / persist legs below fall through (no
/// channel binding, notify bot offline, transient send_message 429, …).
/// Without this, a flaky renderer would cause the policy to re-fire on
/// every 5-min tick.
pub(crate) async fn stamp_recap_cycle(pool: &PgPool, session_key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("UPDATE sessions SET idle_recap_posted_at = NOW() WHERE session_key = $1")
        .bind(session_key)
        .execute(pool)
        .await
        .map(|_| ())
}

/// Compare-and-clear the stored recap pointer: only clears the columns
/// when the row's current `idle_recap_message_id` still matches the
/// `expected_message_id` passed in. This avoids a race where a stale
/// auto-delete task wakes after the next 5-min cycle has already posted
/// a fresh card and would otherwise nullify a still-live pointer,
/// orphaning the new Discord message.
pub(crate) async fn clear_recap_pointer(
    pool: &PgPool,
    session_key: &str,
    expected_message_id: u64,
) -> Result<bool, sqlx::Error> {
    sqlx::query(
        "UPDATE sessions
         SET idle_recap_message_id = NULL,
             idle_recap_channel_id = NULL
         WHERE session_key = $1
           AND idle_recap_message_id = $2",
    )
    .bind(session_key)
    .bind(expected_message_id as i64)
    .execute(pool)
    .await
    .map(|result| result.rows_affected() > 0)
}

/// Lookup the active recap pointer for a Discord channel id so the
/// message_handler can clear it on the next user message without knowing
/// the session_key in advance.
pub(crate) async fn lookup_active_recap_for_channel(
    pool: &PgPool,
    channel_id: u64,
) -> Result<Option<(String, u64, u64)>, sqlx::Error> {
    let row = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT session_key, idle_recap_channel_id, idle_recap_message_id
         FROM sessions
         WHERE idle_recap_channel_id = $1
           AND idle_recap_message_id IS NOT NULL
         LIMIT 1",
    )
    .bind(channel_id as i64)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(k, c, m)| (k, c as u64, m as u64)))
}

/// Extract `tmux_session_name` from a session_key — the part after the last
/// `:`. Mirrors `parseSessionTmuxName` from `policies/lib/timeouts-helpers.js`.
pub(crate) fn tmux_session_name_from_key(session_key: &str) -> Option<&str> {
    session_key
        .rsplit_once(':')
        .map(|(_, name)| name)
        .filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot_with_sessions(
        claude_session_id: Option<&str>,
        raw_provider_session_id: Option<&str>,
    ) -> RecapSnapshot {
        RecapSnapshot {
            session_key: "discord:codex:AgentDesk-codex-test".to_string(),
            provider: "codex".to_string(),
            model: None,
            tokens: None,
            tokens_updated_at: None,
            last_heartbeat: None,
            claude_session_id: claude_session_id.map(str::to_string),
            raw_provider_session_id: raw_provider_session_id.map(str::to_string),
            live_context_usage: None,
            latest_turn_provider: None,
            latest_turn_session_key: None,
            latest_turn_session_id: None,
            latest_turn_finished_at: None,
            latest_turn_input_tokens: None,
            latest_turn_cache_create_tokens: None,
            latest_turn_cache_read_tokens: None,
            previous_message_id: None,
            previous_channel_id: None,
            discord_channel_id: None,
            discord_channel_cc: None,
            discord_channel_cdx: Some("1506295335096549406".to_string()),
            discord_channel_alt: None,
            cwd: None,
        }
    }

    #[test]
    fn recap_requires_resumable_provider_session_id() {
        assert!(!snapshot_with_sessions(None, None).has_resumable_provider_session());
        assert!(!snapshot_with_sessions(Some("  "), Some("")).has_resumable_provider_session());
        assert!(snapshot_with_sessions(Some("session-1"), None).has_resumable_provider_session());
        assert!(snapshot_with_sessions(None, Some("raw-1")).has_resumable_provider_session());
    }

    #[test]
    fn idle_recap_delete_guard_requires_recap_content_marker() {
        assert!(content_looks_like_idle_recap_card("📦 idle 8분"));
        assert!(content_looks_like_idle_recap_card(
            "📦 12k / 200k tokens (6%) · idle 8분"
        ));
        assert!(!content_looks_like_idle_recap_card(
            "✅ **응답 완료** — codex"
        ));
        assert!(!content_looks_like_idle_recap_card(
            "> 📦 mentioned inside a normal response"
        ));
    }

    #[test]
    fn recap_prefers_known_live_session_context_window() {
        let mut snapshot = snapshot_with_sessions(None, Some("raw-1"));
        snapshot.live_context_usage = Some(RecapLiveContextUsage {
            used_tokens: 117_600,
            context_window_tokens: 272_000,
        });

        let display = select_recap_context(&snapshot, Utc::now());
        assert_eq!(
            display,
            RecapContextDisplay::Known {
                used: 117_600,
                window: 272_000
            }
        );
        let header = compose_recap_header(&snapshot);
        assert!(header.contains("117.6k / 272.0k tokens (43%)"));
    }

    #[test]
    fn recap_uses_provider_registry_window_for_matching_latest_turn() {
        let mut snapshot = snapshot_with_sessions(Some("claude-session-1"), None);
        snapshot.provider = "claude".to_string();
        snapshot.latest_turn_provider = Some("claude".to_string());
        snapshot.latest_turn_session_id = Some("claude-session-1".to_string());
        snapshot.latest_turn_finished_at = Some(Utc::now());
        snapshot.latest_turn_input_tokens = Some(12_000);
        snapshot.latest_turn_cache_create_tokens = Some(3_000);
        snapshot.latest_turn_cache_read_tokens = Some(5_000);

        assert_eq!(
            select_recap_context(&snapshot, Utc::now()),
            RecapContextDisplay::Known {
                used: 20_000,
                window: ProviderKind::Claude.default_context_window()
            }
        );
    }

    #[test]
    fn recap_unknown_provider_uses_conservative_fallback_when_tokens_are_fresh() {
        let mut snapshot = snapshot_with_sessions(None, Some("raw-1"));
        snapshot.provider = "unknown-provider".to_string();
        snapshot.tokens = Some(10_000);
        snapshot.tokens_updated_at = Some(Utc::now());

        assert_eq!(
            select_recap_context(&snapshot, Utc::now()),
            RecapContextDisplay::Known {
                used: 10_000,
                window: FALLBACK_CONTEXT_WINDOW_TOKENS
            }
        );
    }

    #[test]
    fn recap_stale_session_tokens_are_not_rendered_as_live_context() {
        let mut snapshot = snapshot_with_sessions(None, Some("raw-1"));
        let now = Utc::now();
        snapshot.tokens = Some(303_000);
        snapshot.tokens_updated_at =
            Some(now - chrono::Duration::seconds(SESSION_TOKEN_FRESHNESS_MAX_SECS + 1));

        assert_eq!(
            select_recap_context(&snapshot, now),
            RecapContextDisplay::Stale
        );
        let header = compose_recap_header(&snapshot);
        assert!(header.contains("context stale"));
        assert!(!header.contains("303.0k"));
    }

    #[test]
    fn recap_over_window_usage_is_capped_and_flagged() {
        let mut snapshot = snapshot_with_sessions(None, Some("raw-1"));
        snapshot.live_context_usage = Some(RecapLiveContextUsage {
            used_tokens: 303_000,
            context_window_tokens: 272_000,
        });

        let header = compose_recap_header(&snapshot);
        assert!(header.contains("303.0k / 272.0k tokens (100%+, over limit)"));
        assert!(!header.contains("(111%)"));
    }

    #[test]
    fn recap_latest_turn_usage_must_match_active_provider_session() {
        let mut snapshot = snapshot_with_sessions(None, Some("raw-active"));
        snapshot.latest_turn_provider = Some("codex".to_string());
        snapshot.latest_turn_session_id = Some("raw-stale".to_string());
        snapshot.latest_turn_finished_at = Some(Utc::now());
        snapshot.latest_turn_input_tokens = Some(42_000);

        assert_eq!(
            select_recap_context(&snapshot, Utc::now()),
            RecapContextDisplay::Unknown
        );

        snapshot.latest_turn_session_id = Some("raw-active".to_string());
        assert_eq!(
            select_recap_context(&snapshot, Utc::now()),
            RecapContextDisplay::Known {
                used: 42_000,
                window: ProviderKind::Codex.default_context_window()
            }
        );
    }

    #[test]
    fn parse_transcript_line_text_extracts_user_text_block() {
        let line = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"이거 봐줘"}]},"sessionId":"sess"}"#;
        assert_eq!(
            parse_transcript_line_text(line),
            Some("[user] 이거 봐줘".to_string())
        );
    }

    #[test]
    fn parse_transcript_line_text_extracts_assistant_concatenated_text() {
        let line = r#"{"type":"assistant","message":{"content":[{"type":"text","text":"part one"},{"type":"tool_use","input":{}},{"type":"text","text":"part two"}]}}"#;
        assert_eq!(
            parse_transcript_line_text(line),
            Some("[assistant] part one part two".to_string())
        );
    }

    #[test]
    fn parse_transcript_line_text_skips_non_conversational_envelopes() {
        let cases = [
            r#"{"type":"system","subtype":"init","sessionId":"sess"}"#,
            r#"{"type":"result","result":"done","sessionId":"sess"}"#,
            r#"{"type":"attachment","attachment":{"type":"hook_success"}}"#,
            r#"{"type":"assistant","message":{"content":[{"type":"tool_use","input":{}}]}}"#,
            "",
            "not even json",
        ];
        for case in cases {
            assert_eq!(parse_transcript_line_text(case), None, "case: {case}");
        }
    }

    #[test]
    fn extract_transcript_tail_text_caps_to_last_tmux_scrollback_lines() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let mut body = String::new();
        // 150 user turns; the helper must keep only the last
        // TMUX_SCROLLBACK_LINES (100).
        for i in 0..150 {
            body.push_str(&format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"text","text":"line-{i}"}}]}}}}"#,
            ));
            body.push('\n');
        }
        std::fs::write(file.path(), body).unwrap();

        let out = extract_transcript_tail_text(file.path()).expect("tail text");
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines.len(), TMUX_SCROLLBACK_LINES as usize);
        assert_eq!(lines.first().copied(), Some("[user] line-50"));
        assert_eq!(lines.last().copied(), Some("[user] line-149"));
    }

    #[test]
    fn extract_transcript_tail_text_returns_none_for_pure_noise_transcripts() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            concat!(
                r#"{"type":"system","subtype":"init","sessionId":"sess"}"#,
                "\n",
                r#"{"type":"attachment","attachment":{"type":"hook_success"}}"#,
                "\n",
                r#"{"type":"result","result":"done","sessionId":"sess"}"#,
                "\n",
            ),
        )
        .unwrap();
        assert_eq!(extract_transcript_tail_text(file.path()), None);
    }

    #[test]
    fn extract_transcript_tail_text_returns_none_when_file_missing() {
        let path = std::path::Path::new("/nonexistent-idle-recap-transcript.jsonl");
        assert_eq!(extract_transcript_tail_text(path), None);
    }
}
