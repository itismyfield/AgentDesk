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
    /// #3148: per-channel turn-generation counter captured at snapshot load.
    /// The persist CAS (`persist_recap_message_id`) folds this into the UPDATE
    /// `WHERE` so a turn claimed during the (multi-second) compose/persist
    /// window — which bumps this counter via `bump_turn_generation` — makes the
    /// persist a no-op (0 rows) and the just-posted card is deleted instead of
    /// being left over the now-active turn. See migration 0070.
    pub(crate) idle_recap_turn_generation: i64,
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
                s.idle_recap_turn_generation,
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
    idle_recap_turn_generation: i64,
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
            idle_recap_turn_generation: self.idle_recap_turn_generation,
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
    let known = |used, window| RecapContextDisplay::Known {
        used: display_context_tokens(snapshot, used, window),
        window,
    };
    if let Some(live) = snapshot
        .live_context_usage
        .filter(|live| live.used_tokens > 0 && live.context_window_tokens > 0)
    {
        return known(live.used_tokens, live.context_window_tokens);
    }

    let window = context_window_for(snapshot);
    if let Some(used) = latest_turn_context_tokens(snapshot) {
        return known(used, window);
    }
    if session_tokens_are_stale_or_incompatible(snapshot, now) {
        return RecapContextDisplay::Stale;
    }
    fresh_session_tokens(snapshot, now)
        .map_or(RecapContextDisplay::Unknown, |used| known(used, window))
}

fn display_context_tokens(snapshot: &RecapSnapshot, used: u64, window: u64) -> u64 {
    match ProviderKind::from_str(&snapshot.provider) {
        Some(ProviderKind::Codex) if window > 0 => used.min(window),
        _ => used,
    }
}

fn latest_turn_context_tokens(snapshot: &RecapSnapshot) -> Option<u64> {
    latest_turn_matches_active_session(snapshot).then_some(())?;
    let input = non_negative_i64_to_u64(snapshot.latest_turn_input_tokens?)?;
    let used = input
        .saturating_add(non_negative_i64_to_u64(
            snapshot.latest_turn_cache_create_tokens.unwrap_or(0),
        )?)
        .saturating_add(non_negative_i64_to_u64(
            snapshot.latest_turn_cache_read_tokens.unwrap_or(0),
        )?);
    (used > 0).then_some(used)
}

fn latest_turn_matches_active_session(snapshot: &RecapSnapshot) -> bool {
    snapshot.latest_turn_finished_at.is_some()
        && same_normalized_opt(
            snapshot.latest_turn_provider.as_deref(),
            Some(snapshot.provider.as_str()),
        )
        && (same_normalized_opt(
            snapshot.latest_turn_session_key.as_deref(),
            Some(snapshot.session_key.as_str()),
        ) || snapshot
            .latest_turn_session_id
            .as_deref()
            .and_then(normalized_text)
            .is_some_and(|latest| provider_session_ids(snapshot).any(|active| active == latest)))
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
    non_negative_i64_to_u64(tokens).is_none()
        || (snapshot.latest_turn_finished_at.is_some()
            && !latest_turn_matches_active_session(snapshot))
        || !session_tokens_are_fresh(snapshot, now)
}

fn fresh_session_tokens(snapshot: &RecapSnapshot, now: DateTime<Utc>) -> Option<u64> {
    let tokens = non_negative_i64_to_u64(snapshot.tokens?)?;
    (tokens > 0 && session_tokens_are_fresh(snapshot, now)).then_some(tokens)
}

fn session_tokens_are_fresh(snapshot: &RecapSnapshot, now: DateTime<Utc>) -> bool {
    snapshot.tokens_updated_at.is_some_and(|updated_at| {
        let age = now - updated_at;
        age.num_seconds() >= 0 && age.num_seconds() <= SESSION_TOKEN_FRESHNESS_MAX_SECS
    })
}

fn non_negative_i64_to_u64(value: i64) -> Option<u64> {
    u64::try_from(value).ok()
}

fn same_normalized_opt(left: Option<&str>, right: Option<&str>) -> bool {
    matches!(
        (left.and_then(normalized_text), right.and_then(normalized_text),),
        (Some(left), Some(right)) if left.eq_ignore_ascii_case(right)
    )
}

fn normalized_text(value: &str) -> Option<&str> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then_some(trimmed)
}

fn context_window_for(snapshot: &RecapSnapshot) -> u64 {
    ProviderKind::from_str(&snapshot.provider).map_or(FALLBACK_CONTEXT_WINDOW_TOKENS, |provider| {
        provider.resolve_context_window(snapshot.model.as_deref())
    })
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
///
/// #3148 (compare-and-swap on the turn generation): the persist is conditional
/// on `idle_recap_turn_generation` still equalling `captured_generation` — the
/// value read at snapshot load (`load_recap_snapshot`), ~20s before this
/// commit. A turn claimed anywhere in the compose/persist window calls
/// `bump_turn_generation`, which increments the same row's counter; the two
/// UPDATEs serialize on the Postgres row, so if a claim committed first this
/// CAS matches 0 rows and the caller deletes the just-posted card instead of
/// stamping it over the now-active turn (closing Window 1 atomically). Returns
/// the number of rows affected: `1` = card persisted, `0` = CAS lost (a turn
/// raced in).
pub(crate) async fn persist_recap_message_id(
    pool: &PgPool,
    session_key: &str,
    channel_id: u64,
    message_id: u64,
    captured_generation: i64,
) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE sessions
         SET idle_recap_message_id = $1,
             idle_recap_channel_id = $2,
             idle_recap_posted_at  = NOW()
         WHERE session_key = $3
           AND idle_recap_turn_generation = $4",
    )
    .bind(message_id as i64)
    .bind(channel_id as i64)
    .bind(session_key)
    .bind(captured_generation)
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
}

/// #3148 / #3158: which row(s) a turn-claim bump targets.
///
/// #3158 fixed a false-skip: the old bump joined `agents` with a 4-column OR
/// and then incremented ALL sessions for the matched agent. A claim in one of
/// an agent's channels (e.g. Claude `cc`) thus bumped the generation of the
/// agent's OTHER provider sessions (e.g. Codex `cdx`) too, and a sibling
/// channel's in-flight idle recap then lost its persist CAS and had its
/// just-posted card FALSE-deleted though that channel stayed idle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BumpScope {
    /// The normal case for both callers: a namespaced `session_key` is held,
    /// so the bump targets the IDENTICAL single (UNIQUE) `sessions.session_key`
    /// row that `load_recap_snapshot` reads the generation from and that
    /// `persist_recap_message_id` CAS-keys on — they share scope.
    SessionKey(String),
    /// Fallback when `session_key` is absent (channel name unresolvable). Bump
    /// only THIS channel's provider column(s) (mirroring `resolve_post_channel`)
    /// AND additionally filter `lower(provider)`, so a sibling-provider session
    /// of the same agent is never over-bumped.
    ProviderChannel { channel_id: u64, provider: String },
}

/// Decide the bump scope from the claim's provider and optional `session_key`.
///
/// A non-empty (after trim) `session_key` selects the single-row primary path;
/// `None`/empty/whitespace selects the provider-scoped channel fallback. Kept
/// as a pure function so the keying decision is unit-testable without a DB.
pub(crate) fn bump_scope_for_claim(
    channel_id: u64,
    provider: &ProviderKind,
    session_key: Option<&str>,
) -> BumpScope {
    match session_key.map(str::trim).filter(|key| !key.is_empty()) {
        Some(key) => BumpScope::SessionKey(key.to_string()),
        None => BumpScope::ProviderChannel {
            channel_id,
            provider: provider.as_str().to_string(),
        },
    }
}

/// #3148/#3158: bump the per-channel turn-generation counter for the session
/// the just-claimed turn belongs to. Called best-effort right after a turn is
/// claimed (TUI or Discord-intake) and BEFORE the relocated recap-clear, so any
/// idle-recap POST job whose persist CAS captured the pre-bump generation fails
/// to persist its card over this now-active turn.
///
/// #3158: scoped to the SAME row the persist CAS reads.
/// - PRIMARY (normal case): keyed `WHERE session_key = $1` on the caller's
///   namespaced `session_key` — the IDENTICAL (UNIQUE) row identity that
///   `load_recap_snapshot` reads the generation from and that
///   `persist_recap_message_id` CAS-keys on. Bump and persist CAS share scope.
/// - FALLBACK (`session_key` absent): a provider-scoped channel bump that
///   matches only THIS channel's provider column(s) (mirroring
///   `resolve_post_channel`) ANDed with `lower(provider)`, so a sibling-provider
///   session of the same agent is never touched. As a belt-and-suspenders for
///   any legacy non-namespaced row, the fallback also runs when the primary
///   UPDATE matches 0 rows.
///
/// Keyed via the same channel→session resolution `load_recap_snapshot`/
/// `resolve_post_channel` use, NOT by `idle_recap_channel_id` (which is NULL
/// whenever no card is currently recorded — and the whole point is to bump even
/// when no card exists yet so a mid-flight POST cannot persist one). Each
/// increment is a single atomic SQL UPDATE; Postgres row-level locking
/// serializes it against the persist CAS, so there is no app-side
/// read-modify-write gap.
pub(crate) async fn bump_turn_generation(
    pool: &PgPool,
    channel_id: u64,
    provider: &ProviderKind,
    session_key: Option<&str>,
) -> Result<(), sqlx::Error> {
    let scope = bump_scope_for_claim(channel_id, provider, session_key);
    let provider_fallback = match &scope {
        BumpScope::SessionKey(key) => {
            let affected = bump_by_session_key(pool, key).await?;
            if affected > 0 {
                return Ok(());
            }
            // Belt-and-suspenders: a legacy (non-namespaced) row would not
            // match the namespaced caller key, leaving Window 1 unguarded. Fall
            // through to the provider-scoped channel bump in that rare case.
            ProviderKind::from_str(provider.as_str()).unwrap_or_else(|| provider.clone())
        }
        BumpScope::ProviderChannel { provider, .. } => ProviderKind::from_str(provider)
            .unwrap_or_else(|| ProviderKind::Unsupported(provider.clone())),
    };
    bump_by_provider_channel(pool, channel_id, &provider_fallback).await
}

/// PRIMARY: increment the single UNIQUE `session_key` row. Returns rows affected.
async fn bump_by_session_key(pool: &PgPool, session_key: &str) -> Result<u64, sqlx::Error> {
    sqlx::query(
        "UPDATE sessions
         SET idle_recap_turn_generation = idle_recap_turn_generation + 1
         WHERE session_key = $1",
    )
    .bind(session_key)
    .execute(pool)
    .await
    .map(|result| result.rows_affected())
}

/// FALLBACK: provider-scoped channel bump. Mirrors `resolve_post_channel`'s
/// provider→column mapping and `load_recap_snapshot`'s `lower(COALESCE(...))`
/// provider comparison, so only THIS channel's same-provider session(s) are
/// bumped — never a sibling-provider session of the same agent.
async fn bump_by_provider_channel(
    pool: &PgPool,
    channel_id: u64,
    provider: &ProviderKind,
) -> Result<(), sqlx::Error> {
    let channel_text = channel_id.to_string();
    let column_predicate = match provider {
        ProviderKind::Claude => "(a.discord_channel_cc = $1 OR a.discord_channel_id = $1)",
        ProviderKind::Codex => "(a.discord_channel_cdx = $1 OR a.discord_channel_alt = $1)",
        _ => "(a.discord_channel_id = $1 OR a.discord_channel_cc = $1)",
    };
    let query = format!(
        "UPDATE sessions s
         SET idle_recap_turn_generation = s.idle_recap_turn_generation + 1
         FROM agents a
         WHERE a.id = s.agent_id
           AND {column_predicate}
           AND lower(COALESCE(s.provider, '')) = $2"
    );
    sqlx::query(&query)
        .bind(channel_text)
        .bind(provider.as_str().to_lowercase())
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

/// Core clear sequence for an idle-recap card bound to a Discord channel,
/// generic over the lookup / delete / clear-pointer operations so the
/// decision logic is unit-testable without a live Postgres or Discord http.
///
/// Invariant (#3146): while an active turn exists for a channel — regardless
/// of origin (Discord-intake OR a TUI-driven turn detected by the watcher) —
/// the `📦 … idle N분` recap card must not remain shown. Both call sites feed
/// the same `(channel_id)` key into this helper, so a turn that starts via the
/// TUI clears the card exactly the way a Discord-origin turn already does.
///
/// When the lookup returns `None` (no recap card recorded for the channel)
/// this is a no-op: a still-idle channel keeps its card.
#[allow(dead_code)] // #3034: test-only seam (prod wrappers removed; see codex R3 P2 note above).
async fn clear_idle_recap_for_channel_with<Lookup, LookupFut, Delete, DeleteFut, Clear, ClearFut>(
    channel_id: u64,
    lookup: Lookup,
    delete: Delete,
    clear: Clear,
) where
    Lookup: FnOnce(u64) -> LookupFut,
    LookupFut: std::future::Future<Output = Result<Option<(String, u64, u64)>, sqlx::Error>>,
    Delete: FnOnce(u64, u64) -> DeleteFut,
    DeleteFut: std::future::Future<Output = ()>,
    Clear: FnOnce(String, u64) -> ClearFut,
    ClearFut: std::future::Future<Output = Result<bool, sqlx::Error>>,
{
    match lookup(channel_id).await {
        Ok(Some((session_key, chan, msg))) => {
            delete(chan, msg).await;
            let _ = clear(session_key, msg).await;
        }
        Ok(None) => {}
        Err(e) => tracing::warn!(
            error = %e,
            channel_id = channel_id,
            "idle_recap clear lookup failed"
        ),
    }
}

// codex R3 P2: the non-captured `clear_idle_recap_for_channel` /
// `spawn_clear_idle_recap_for_channel` wrappers were removed. Both the
// Discord-intake path and the TUI claim path now capture the recap pointer
// SYNCHRONOUSLY at claim time and clear ONLY that captured id via
// `spawn_clear_captured_idle_recap_for_channel` — see its doc-comment. The
// non-captured variant ran `lookup_active_recap_for_channel` inside the
// detached task, so a delayed clear could delete a NEWER card from a later
// idle period (NOT self-healing). The generic
// `clear_idle_recap_for_channel_with` seam below is retained only because the
// unit tests exercise its lookup → delete → clear-pointer decision logic.

/// Clear ONLY the specific recap card identified by `(session_key, channel_id,
/// message_id)` — never whatever card happens to be current at clear time.
///
/// codex R2 P2: the idle-recap policy (`policies/timeouts/idle-recap.js`) posts
/// at most ONCE per idle period (`idle_recap_posted_at < last_heartbeat`), so a
/// card it deletes is NOT re-posted until new activity re-arms the session. A
/// delayed clear that ran the generic `lookup_active_recap_for_channel` could
/// therefore delete a LATER, legitimately-posted card and lose it for the rest
/// of the idle period (NOT self-healing). Binding the clear to the card id that
/// existed when the turn was CLAIMED makes a delayed clear a no-op against any
/// newer card:
///   - `delete_previous_card` probes the captured message id and only deletes
///     it if it is still an idle-recap card (a newer card has a different id, so
///     the probe targets the old — now-replaced or already-gone — message).
///   - `clear_recap_pointer` is a compare-and-clear: it nulls the pointer ONLY
///     when the row's `idle_recap_message_id` still equals the captured id, so
///     a pointer that has already advanced to a newer card is left intact.
pub(in crate::services::discord) async fn clear_specific_idle_recap_card(
    http: &serenity::Http,
    pool: &PgPool,
    session_key: &str,
    channel_id: u64,
    message_id: u64,
) {
    clear_specific_idle_recap_card_with(
        session_key.to_string(),
        channel_id,
        message_id,
        |chan, msg| delete_previous_card(http, chan, msg),
        |session_key, msg| async move { clear_recap_pointer(pool, &session_key, msg).await },
    )
    .await;
}

/// Generic seam for `clear_specific_idle_recap_card`, parameterised over the
/// delete / compare-and-clear operations so the codex R2 P2 invariant (the
/// clear targets ONLY the captured id, and the CAS is a no-op when the pointer
/// has advanced) is unit-testable without a live Postgres or Discord http.
async fn clear_specific_idle_recap_card_with<Delete, DeleteFut, Clear, ClearFut>(
    session_key: String,
    channel_id: u64,
    message_id: u64,
    delete: Delete,
    clear: Clear,
) where
    Delete: FnOnce(u64, u64) -> DeleteFut,
    DeleteFut: std::future::Future<Output = ()>,
    Clear: FnOnce(String, u64) -> ClearFut,
    ClearFut: std::future::Future<Output = Result<bool, sqlx::Error>>,
{
    delete(channel_id, message_id).await;
    let _ = clear(session_key, message_id).await;
}

/// Capture-at-claim + compare-and-clear for the TUI claim path (codex R2 P2).
///
/// Resolves the recap card that exists for `channel_id` RIGHT NOW (synchronously
/// relative to the turn becoming active), then spawns a detached task that
/// clears ONLY that captured card id via `clear_specific_idle_recap_card`. A
/// clear that wakes after a newer card was posted cannot delete the newer card,
/// because both the delete probe and the pointer CAS are bound to the captured
/// id. When no card is recorded for the channel this is a no-op.
pub(in crate::services::discord) async fn spawn_clear_captured_idle_recap_for_channel(
    http: std::sync::Arc<serenity::Http>,
    pool: PgPool,
    channel_id: u64,
) {
    let captured = match lookup_active_recap_for_channel(&pool, channel_id).await {
        Ok(Some((session_key, chan, msg))) => (session_key, chan, msg),
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(
                error = %e,
                channel_id = channel_id,
                "idle_recap captured-clear lookup failed"
            );
            return;
        }
    };
    let (session_key, chan, msg) = captured;
    tokio::spawn(async move {
        clear_specific_idle_recap_card(&http, &pool, &session_key, chan, msg).await;
    });
}

/// Extract `tmux_session_name` from a session_key — the part after the last
/// `:`. Mirrors `parseSessionTmuxName` from `policies/lib/timeouts-helpers.js`.
pub(crate) fn tmux_session_name_from_key(session_key: &str) -> Option<&str> {
    session_key
        .rsplit_once(':')
        .map(|(_, name)| name)
        .filter(|s| !s.is_empty())
}

/// #3146 Part 1 (codex clear/post race): does this channel currently have an
/// ACTIVE turn? Probed right before the recap post job commits a card.
///
/// Two ORed signals, both consulted so the recheck is correct at the EARLIEST
/// turn-active point (codex R2 P1):
///
///   1. MAILBOX active turn (authoritative, set FIRST). `claim_tui_direct_
///      synthetic_turn` makes the turn active via `mailbox_try_start_turn`
///      BEFORE it writes the inflight sidecar — there is a multi-step window
///      between the two. The mailbox is the same signal `idle_detector` treats
///      as authoritative; `ChannelMailboxRegistry::global_handle` resolves the
///      per-channel actor from a process-global registry (the handle is mirrored
///      into `GLOBAL_CHANNEL_MAILBOXES` by `mailbox()`), so the server route can
///      consult it without an `Arc<SharedData>`. If no turn has ever touched the
///      channel the global handle is absent → falls through to the inflight check.
///   2. INFLIGHT sidecar (defense-in-depth). A present, NON-stale inflight state
///      for `(provider, channel_id)` — the marker the claim path writes LATER via
///      `save_inflight_state`. Staleness is applied so a leftover inflight from a
///      long-crashed dispatch never produces a false skip on a genuinely idle
///      channel. Kept so a turn that exists only as inflight (e.g. restored from
///      disk before the mailbox actor is re-spawned) is still detected.
///
/// Cross-platform note: both the global mailbox handle and the on-disk inflight
/// sidecar are cross-platform (no tmux, no `#[cfg(unix)]` symbol), so this
/// compiles and behaves identically on Windows.
pub(crate) async fn channel_has_active_turn(provider: &ProviderKind, channel_id: u64) -> bool {
    if mailbox_has_active_turn(channel_id).await {
        return true;
    }
    inflight_has_active_turn(provider, channel_id)
}

/// Earliest turn-active signal: consult the process-global mailbox actor for
/// this channel (set by `mailbox_try_start_turn` BEFORE the inflight sidecar is
/// written). Returns `false` when no mailbox has ever been spawned for the
/// channel — a genuinely-idle channel that never hosted a turn.
async fn mailbox_has_active_turn(channel_id: u64) -> bool {
    match crate::services::turn_orchestrator::ChannelMailboxRegistry::global_handle(
        serenity::ChannelId::new(channel_id),
    ) {
        Some(handle) => handle.has_active_turn().await,
        None => false,
    }
}

/// Defense-in-depth turn-active signal: a present, NON-stale inflight sidecar
/// for `(provider, channel_id)`.
fn inflight_has_active_turn(provider: &ProviderKind, channel_id: u64) -> bool {
    let Some(state) = super::inflight::load_inflight_state(provider, channel_id) else {
        return false;
    };
    let now_unix = Utc::now().timestamp();
    !super::inflight::inflight_state_is_stale(
        &state,
        now_unix,
        super::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS,
    )
}

/// #3146 Part 1 (codex clear/post race): pure decision seam for the recap
/// post job. We post the fresh recap card ONLY when the channel is still
/// idle. If a turn became active during the (multi-second) tmux-capture +
/// Haiku-summary compose window, posting would slap a stale `📦 … idle` card
/// over a live turn — exactly the bug #3146 closes. The idle-cycle stamp at
/// the top of the route already deduped this cycle, so skipping is safe.
pub(crate) fn should_post_recap(active_turn: bool) -> bool {
    !active_turn
}

/// #3146 Part 1 (codex R3 P1 — check-then-post TOCTOU): what the post job does
/// with a card it ALREADY posted, given the active-turn state re-checked AFTER
/// the Discord POST returned (and before persisting the pointer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PostRecheckAction {
    /// Channel is still idle — persist the just-posted card's pointer.
    Persist,
    /// A turn raced into the (check → post) window — UNDO the post: delete the
    /// just-posted card and do NOT persist its pointer. The capture-at-claim
    /// clear of that racing turn grabbed the OLD pointer, so it cannot remove
    /// THIS card; persisting would leave a stale `📦 … idle` card over the live
    /// turn. The idle-cycle stamp already deduped this cycle, so not persisting
    /// is safe.
    DeleteAndSkipPersist,
}

/// Pure decision seam for the post-recheck. The post job calls this with the
/// active-turn state observed AFTER `post_recap_card` returns; the residual
/// window (a turn starting in the few-ms between the POST returning and this
/// recheck) is inherent and is documented at the call site in
/// `server::routes::idle_recap::run_idle_recap_post_job`.
pub(crate) fn post_recheck_action(active_turn_after_post: bool) -> PostRecheckAction {
    if should_post_recap(active_turn_after_post) {
        PostRecheckAction::Persist
    } else {
        PostRecheckAction::DeleteAndSkipPersist
    }
}

/// #3148: what the post job does with the result of the conditional persist
/// (`persist_recap_message_id`, which folds the turn-generation CAS into its
/// `WHERE`). This is the atomic close of Window 1: a turn that claims in the
/// post-recheck→persist gap bumps the generation, the persist matches 0 rows,
/// and we treat the card the same as the `DeleteAndSkipPersist` path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PersistCasOutcome {
    /// `rows_affected == 1`: the generation was unchanged, the pointer is now
    /// persisted, log success.
    Persisted,
    /// `rows_affected == 0`: a turn claimed during the persist window and
    /// bumped the generation, so the CAS lost. Delete the just-posted card and
    /// skip persisting (an expected race outcome, NOT an error — the idle-cycle
    /// stamp already deduped this cycle so it does not re-fire).
    CasLostDeleteAndSkip,
}

/// Pure decision seam mapping the persist CAS `rows_affected` to the post job's
/// action, so the "0 rows ⇒ delete + skip" branch is unit-testable without a
/// live Postgres.
pub(crate) fn persist_cas_outcome(rows_affected: u64) -> PersistCasOutcome {
    if rows_affected == 0 {
        PersistCasOutcome::CasLostDeleteAndSkip
    } else {
        PersistCasOutcome::Persisted
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// #3146 Part 1: when a turn becomes active for a channel that has a
    /// recorded idle-recap card, the clear sequence must delete the card AND
    /// clear the recap pointer. This exercises the SAME core helper both the
    /// Discord-intake and the TUI-driven turn call sites use, so a TUI-origin
    /// active turn clears the card exactly the way a Discord-origin turn does.
    #[tokio::test]
    async fn clear_idle_recap_for_channel_deletes_card_and_clears_pointer() {
        let deleted: Rc<RefCell<Vec<(u64, u64)>>> = Rc::new(RefCell::new(Vec::new()));
        let cleared: Rc<RefCell<Vec<(String, u64)>>> = Rc::new(RefCell::new(Vec::new()));
        let deleted_for_closure = deleted.clone();
        let cleared_for_closure = cleared.clone();

        clear_idle_recap_for_channel_with(
            777,
            |channel_id| async move {
                assert_eq!(channel_id, 777);
                Ok(Some(("discord:codex:tui-sess".to_string(), 777, 4242)))
            },
            move |chan, msg| {
                let deleted = deleted_for_closure.clone();
                async move {
                    deleted.borrow_mut().push((chan, msg));
                }
            },
            move |session_key, msg| {
                let cleared = cleared_for_closure.clone();
                async move {
                    cleared.borrow_mut().push((session_key, msg));
                    Ok(true)
                }
            },
        )
        .await;

        assert_eq!(deleted.borrow().as_slice(), &[(777, 4242)]);
        assert_eq!(
            cleared.borrow().as_slice(),
            &[("discord:codex:tui-sess".to_string(), 4242)]
        );
    }

    /// A still-idle channel (no recap pointer recorded) must NOT have anything
    /// deleted or cleared — the clear is a no-op so a legitimately idle card
    /// survives.
    #[tokio::test]
    async fn clear_idle_recap_for_channel_noop_when_no_card_recorded() {
        let deleted = Rc::new(RefCell::new(0u32));
        let cleared = Rc::new(RefCell::new(0u32));
        let deleted_for_closure = deleted.clone();
        let cleared_for_closure = cleared.clone();

        clear_idle_recap_for_channel_with(
            123,
            |_channel_id| async move { Ok(None) },
            move |_chan, _msg| {
                let deleted = deleted_for_closure.clone();
                async move {
                    *deleted.borrow_mut() += 1;
                }
            },
            move |_session_key, _msg| {
                let cleared = cleared_for_closure.clone();
                async move {
                    *cleared.borrow_mut() += 1;
                    Ok(true)
                }
            },
        )
        .await;

        assert_eq!(*deleted.borrow(), 0);
        assert_eq!(*cleared.borrow(), 0);
    }

    /// codex R2 P2: the captured-id clear targets ONLY the id captured when the
    /// turn was claimed — it passes the CAPTURED message id to both the delete
    /// probe and the pointer compare-and-clear, never a freshly-looked-up id.
    /// When the pointer has advanced to a NEWER card (the row's
    /// `idle_recap_message_id` no longer matches the captured id), the CAS
    /// reports `rows_affected == 0` (modeled here as `Ok(false)`) and the newer
    /// card's pointer is left intact. The delete probe likewise targets the old
    /// captured id, so `delete_previous_card`'s card-type guard would no-op on a
    /// message that is no longer the recorded card.
    #[tokio::test]
    async fn clear_specific_card_targets_captured_id_and_cas_is_noop_when_pointer_advanced() {
        let deleted: Rc<RefCell<Vec<(u64, u64)>>> = Rc::new(RefCell::new(Vec::new()));
        let cleared: Rc<RefCell<Vec<(String, u64)>>> = Rc::new(RefCell::new(Vec::new()));
        let deleted_for_closure = deleted.clone();
        let cleared_for_closure = cleared.clone();

        // Captured at claim time: the OLD card (id 4242). A NEWER card (id 9999)
        // was posted afterwards, so the live pointer is 9999 — the CAS keyed on
        // 4242 must NOT clear it.
        let captured_msg = 4242u64;
        clear_specific_idle_recap_card_with(
            "discord:codex:tui-sess".to_string(),
            777,
            captured_msg,
            move |chan, msg| {
                let deleted = deleted_for_closure.clone();
                async move {
                    deleted.borrow_mut().push((chan, msg));
                }
            },
            move |session_key, msg| {
                let cleared = cleared_for_closure.clone();
                async move {
                    cleared.borrow_mut().push((session_key, msg));
                    // CAS no-op: pointer advanced to 9999, so an
                    // `... AND idle_recap_message_id = 4242` UPDATE affects 0 rows.
                    Ok(false)
                }
            },
        )
        .await;

        // Delete probe + CAS both used the CAPTURED id (4242), never 9999.
        assert_eq!(deleted.borrow().as_slice(), &[(777, captured_msg)]);
        assert_eq!(
            cleared.borrow().as_slice(),
            &[("discord:codex:tui-sess".to_string(), captured_msg)]
        );
    }

    /// codex R2 P2 (positive case): when the pointer still points at the
    /// captured card (no newer card posted), the captured-id clear deletes that
    /// card and the CAS clears the pointer (`Ok(true)`).
    #[tokio::test]
    async fn clear_specific_card_clears_when_pointer_still_matches_captured_id() {
        let deleted: Rc<RefCell<Vec<(u64, u64)>>> = Rc::new(RefCell::new(Vec::new()));
        let cleared_count = Rc::new(RefCell::new(0u32));
        let deleted_for_closure = deleted.clone();
        let cleared_for_closure = cleared_count.clone();

        clear_specific_idle_recap_card_with(
            "discord:codex:tui-sess".to_string(),
            777,
            4242,
            move |chan, msg| {
                let deleted = deleted_for_closure.clone();
                async move {
                    deleted.borrow_mut().push((chan, msg));
                }
            },
            move |_session_key, msg| {
                let cleared = cleared_for_closure.clone();
                async move {
                    assert_eq!(msg, 4242);
                    *cleared.borrow_mut() += 1;
                    Ok(true)
                }
            },
        )
        .await;

        assert_eq!(deleted.borrow().as_slice(), &[(777, 4242)]);
        assert_eq!(*cleared_count.borrow(), 1);
    }

    /// codex R3 P2: the Discord-INTAKE clear now uses the SAME capture-at-claim
    /// variant (`spawn_clear_captured_idle_recap_for_channel`) the TUI claim
    /// path uses, instead of the old non-captured `spawn_clear_idle_recap_for_
    /// channel` that looked up the pointer INSIDE the detached task. Capturing
    /// the id at intake time and clearing ONLY that captured id means a delayed
    /// intake-clear is a no-op against a NEWER card from a later idle period:
    /// both the delete probe and the pointer CAS are keyed on the captured id,
    /// so when the pointer has advanced the CAS reports 0 rows affected
    /// (`Ok(false)`) and the newer card survives. Mirrors the TUI capture test
    /// for the intake call site (both now route through the same seam).
    #[tokio::test]
    async fn intake_capture_clear_targets_captured_id_and_cas_is_noop_when_pointer_advanced() {
        let deleted: Rc<RefCell<Vec<(u64, u64)>>> = Rc::new(RefCell::new(Vec::new()));
        let cleared: Rc<RefCell<Vec<(String, u64)>>> = Rc::new(RefCell::new(Vec::new()));
        let deleted_for_closure = deleted.clone();
        let cleared_for_closure = cleared.clone();

        // Captured at intake time: the OLD card (id 4242). A NEWER card (id
        // 9999) was posted by a later idle period, so the live pointer is 9999.
        let intake_captured_msg = 4242u64;
        clear_specific_idle_recap_card_with(
            "discord:claude:intake-sess".to_string(),
            555,
            intake_captured_msg,
            move |chan, msg| {
                let deleted = deleted_for_closure.clone();
                async move {
                    deleted.borrow_mut().push((chan, msg));
                }
            },
            move |session_key, msg| {
                let cleared = cleared_for_closure.clone();
                async move {
                    cleared.borrow_mut().push((session_key, msg));
                    // CAS no-op: the pointer advanced to 9999 (a later idle
                    // period's card), so the captured-id UPDATE affects 0 rows.
                    Ok(false)
                }
            },
        )
        .await;

        // Both the delete probe and the CAS used the id captured AT INTAKE
        // (4242), never the newer 9999 — the later card is left intact.
        assert_eq!(deleted.borrow().as_slice(), &[(555, intake_captured_msg)]);
        assert_eq!(
            cleared.borrow().as_slice(),
            &[(
                "discord:claude:intake-sess".to_string(),
                intake_captured_msg
            )]
        );
    }

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
            idle_recap_turn_generation: 0,
            discord_channel_id: None,
            discord_channel_cc: None,
            discord_channel_cdx: Some("1506295335096549406".to_string()),
            discord_channel_alt: None,
            cwd: None,
        }
    }

    // #3158: the turn-claim bump must target the SAME single session row the
    // persist CAS reads (keyed by session_key), with a provider-scoped channel
    // fallback only when session_key is absent. These tests pin the keying
    // decision seam so a claim in channel X cannot false-skip-delete a sibling
    // channel/provider's idle recap.

    #[test]
    fn bump_scope_keys_on_session_key_when_present() {
        // Window 1 (#3148): a claim in channel X bumps X's recap session — and
        // it does so by the IDENTICAL session_key the persist CAS keys on.
        let scope = bump_scope_for_claim(
            123,
            &ProviderKind::Claude,
            Some("host:agent-x:claude:chan-cc"),
        );
        assert_eq!(
            scope,
            BumpScope::SessionKey("host:agent-x:claude:chan-cc".to_string())
        );
    }

    #[test]
    fn bump_scope_session_key_is_provider_independent() {
        // A sibling provider's claim resolves to a DIFFERENT (UNIQUE)
        // session_key, so it never touches another session's generation:
        // no cross-provider over-bump, no false skip.
        let cc = bump_scope_for_claim(123, &ProviderKind::Claude, Some("k-claude"));
        let cdx = bump_scope_for_claim(123, &ProviderKind::Codex, Some("k-codex"));
        assert_eq!(cc, BumpScope::SessionKey("k-claude".to_string()));
        assert_eq!(cdx, BumpScope::SessionKey("k-codex".to_string()));
        assert_ne!(cc, cdx);
    }

    #[test]
    fn bump_scope_trims_and_treats_empty_session_key_as_absent() {
        for empty in [None, Some(""), Some("   "), Some("\t\n")] {
            assert_eq!(
                bump_scope_for_claim(123, &ProviderKind::Claude, empty),
                BumpScope::ProviderChannel {
                    channel_id: 123,
                    provider: "claude".to_string(),
                },
                "empty/whitespace session_key must fall back to provider-scoped channel bump"
            );
        }
        // A surrounding-whitespace but non-empty key is trimmed, not dropped.
        assert_eq!(
            bump_scope_for_claim(123, &ProviderKind::Claude, Some("  k-claude  ")),
            BumpScope::SessionKey("k-claude".to_string())
        );
    }

    #[test]
    fn bump_scope_fallback_carries_provider_for_provider_filtered_update() {
        // Without a session_key the fallback still scopes to THIS channel's
        // provider, so the agent's sibling-provider session is excluded.
        let claude = bump_scope_for_claim(7, &ProviderKind::Claude, None);
        let codex = bump_scope_for_claim(7, &ProviderKind::Codex, None);
        assert_eq!(
            claude,
            BumpScope::ProviderChannel {
                channel_id: 7,
                provider: "claude".to_string(),
            }
        );
        assert_eq!(
            codex,
            BumpScope::ProviderChannel {
                channel_id: 7,
                provider: "codex".to_string(),
            }
        );
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
    fn recap_keeps_claude_over_window_usage_flagged() {
        let mut snapshot = snapshot_with_sessions(Some("claude-session-1"), None);
        snapshot.provider = "claude".to_string();
        snapshot.live_context_usage = Some(RecapLiveContextUsage {
            used_tokens: 303_000,
            context_window_tokens: 272_000,
        });

        let header = compose_recap_header(&snapshot);
        assert!(header.contains("303.0k / 272.0k tokens (100%+, over limit)"));
        assert!(!header.contains("(111%)"));
    }

    #[test]
    fn recap_clamps_codex_context_display_to_window() {
        let mut snapshot = snapshot_with_sessions(None, Some("raw-1"));
        snapshot.live_context_usage = Some(RecapLiveContextUsage {
            used_tokens: 2_300_000,
            context_window_tokens: 272_000,
        });

        assert_eq!(
            select_recap_context(&snapshot, Utc::now()),
            RecapContextDisplay::Known {
                used: 272_000,
                window: 272_000
            }
        );
        let header = compose_recap_header(&snapshot);
        assert!(header.contains("272.0k / 272.0k tokens (100%)"));
        assert!(!header.contains("2.3M"));
        assert!(!header.contains("over limit"));
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

    // ---------------------------------------------------------------
    // #3146 Part 1 (codex clear/post race): the post job must SKIP when a
    // turn became active during compose, and still POST when idle.
    // ---------------------------------------------------------------

    /// Pure decision seam: post iff NOT active. The detached post job in
    /// `server::routes::idle_recap::run_idle_recap_post_job` gates its
    /// delete + post + persist sequence on `should_post_recap(active_turn)`.
    #[test]
    fn should_post_recap_skips_when_turn_active_posts_when_idle() {
        // Idle channel → still post the recap (normal case, no false skip).
        assert!(should_post_recap(false));
        // A turn became active during compose → skip (do NOT post a stale
        // `📦 … idle` card over the live turn).
        assert!(!should_post_recap(true));
    }

    // ---------------------------------------------------------------
    // #3146 Part 1 (codex R3 P1 — check-then-post TOCTOU): after the POST
    // returns, re-check active-turn. If a turn raced into the (check → post)
    // window, UNDO the post (delete the just-posted card, do NOT persist).
    // ---------------------------------------------------------------

    /// Pure recheck decision: idle-after-post → persist; active-after-post →
    /// delete the just-posted card and skip persist.
    #[test]
    fn post_recheck_action_persists_when_idle_deletes_when_turn_raced_in() {
        assert_eq!(
            post_recheck_action(false),
            PostRecheckAction::Persist,
            "still idle after POST → persist the card pointer"
        );
        assert_eq!(
            post_recheck_action(true),
            PostRecheckAction::DeleteAndSkipPersist,
            "a turn raced the check→post window → delete the just-posted card, skip persist"
        );
    }

    /// End-to-end of the post-job's commit branch using the SAME seam the route
    /// uses: simulate "idle at pre-check, ACTIVE at post-recheck". The recheck
    /// must DELETE the just-posted card and NOT persist its pointer. Mirrors the
    /// route's `run_idle_recap_post_job` post-recheck without a live Postgres or
    /// Discord http.
    #[tokio::test]
    async fn post_recheck_deletes_just_posted_card_and_does_not_persist_when_turn_raced_in() {
        let deleted: Rc<RefCell<Vec<(u64, u64)>>> = Rc::new(RefCell::new(Vec::new()));
        let persisted: Rc<RefCell<Vec<(u64, u64)>>> = Rc::new(RefCell::new(Vec::new()));

        // Pre-post check saw idle (so we posted); the POST returned this id.
        let posted_message_id = 5151u64;
        let channel_id = 777u64;
        // ... but a TUI claim raced in during (check → post): active now.
        let active_turn_after_post = true;

        match post_recheck_action(active_turn_after_post) {
            PostRecheckAction::DeleteAndSkipPersist => {
                deleted.borrow_mut().push((channel_id, posted_message_id));
                // NOTE: no persist call in this branch.
            }
            PostRecheckAction::Persist => {
                persisted.borrow_mut().push((channel_id, posted_message_id));
            }
        }

        assert_eq!(
            deleted.borrow().as_slice(),
            &[(channel_id, posted_message_id)],
            "the just-posted card must be deleted"
        );
        assert!(
            persisted.borrow().is_empty(),
            "the pointer must NOT be persisted when a turn raced the check→post window"
        );
    }

    /// Positive control: still idle at the post-recheck → persist the pointer,
    /// delete nothing. This is the normal, common-case path.
    #[tokio::test]
    async fn post_recheck_persists_when_still_idle() {
        let deleted: Rc<RefCell<Vec<(u64, u64)>>> = Rc::new(RefCell::new(Vec::new()));
        let persisted: Rc<RefCell<Vec<(u64, u64)>>> = Rc::new(RefCell::new(Vec::new()));

        let posted_message_id = 6262u64;
        let channel_id = 888u64;
        let active_turn_after_post = false;

        match post_recheck_action(active_turn_after_post) {
            PostRecheckAction::DeleteAndSkipPersist => {
                deleted.borrow_mut().push((channel_id, posted_message_id));
            }
            PostRecheckAction::Persist => {
                persisted.borrow_mut().push((channel_id, posted_message_id));
            }
        }

        assert!(
            deleted.borrow().is_empty(),
            "nothing deleted when still idle"
        );
        assert_eq!(
            persisted.borrow().as_slice(),
            &[(channel_id, posted_message_id)],
            "the pointer is persisted when the channel is still idle at the recheck"
        );
    }

    // ---------------------------------------------------------------
    // #3148 (per-channel turn-generation CAS): the persist folds a
    // compare-and-swap on the generation captured at snapshot load. A turn
    // claimed in the post-recheck→persist gap (residual Window 1) bumps the
    // generation, so the conditional UPDATE matches 0 rows and the just-posted
    // card is deleted instead of stamped over the now-active turn.
    // ---------------------------------------------------------------

    /// Pure decision seam: `rows_affected == 0` (CAS lost — a turn claimed in
    /// the persist window and bumped the generation) → delete the just-posted
    /// card and skip persist; `rows_affected == 1` → persisted.
    #[test]
    fn persist_cas_outcome_skips_when_generation_bumped_persists_otherwise() {
        assert_eq!(
            persist_cas_outcome(0),
            PersistCasOutcome::CasLostDeleteAndSkip,
            "0 rows ⇒ a turn claimed during the persist window bumped the generation ⇒ delete+skip"
        );
        assert_eq!(
            persist_cas_outcome(1),
            PersistCasOutcome::Persisted,
            "1 row ⇒ generation unchanged ⇒ card persisted"
        );
    }

    /// Interleaving (Window 1): the recap job captures generation `G` at
    /// snapshot load; a turn claims between capture and persist, bumping the
    /// row's generation to `G+1`. The persist's `... AND
    /// idle_recap_turn_generation = G` then matches 0 rows. This models that
    /// conditional UPDATE purely: the persist succeeds only when the captured
    /// generation still equals the row's CURRENT generation. The post job must
    /// then DELETE the just-posted card and NOT persist it.
    #[tokio::test]
    async fn persist_cas_noop_when_turn_claimed_between_capture_and_persist() {
        // The recap job captured this at snapshot load (~20s before persist).
        let captured_generation: i64 = 7;
        // A turn claimed in the compose/persist window and bumped the row.
        let row_generation_now: i64 = 8;

        // Model the conditional persist UPDATE's row count: 1 iff the captured
        // generation still matches the row's current generation, else 0.
        let rows_affected: u64 = if captured_generation == row_generation_now {
            1
        } else {
            0
        };

        let deleted: Rc<RefCell<Vec<(u64, u64)>>> = Rc::new(RefCell::new(Vec::new()));
        let persisted: Rc<RefCell<Vec<(u64, u64)>>> = Rc::new(RefCell::new(Vec::new()));
        let channel_id = 4242u64;
        let posted_message_id = 9090u64;

        match persist_cas_outcome(rows_affected) {
            PersistCasOutcome::CasLostDeleteAndSkip => {
                deleted.borrow_mut().push((channel_id, posted_message_id));
            }
            PersistCasOutcome::Persisted => {
                persisted.borrow_mut().push((channel_id, posted_message_id));
            }
        }

        assert_eq!(
            deleted.borrow().as_slice(),
            &[(channel_id, posted_message_id)],
            "a turn claimed in the persist window bumped the generation ⇒ card deleted, not persisted"
        );
        assert!(
            persisted.borrow().is_empty(),
            "the card must NOT be persisted over the now-active turn (Window 1 closed by the CAS)"
        );
    }

    /// Positive control: no claim raced in, so the captured generation still
    /// equals the row's current generation → persist succeeds (1 row), the card
    /// is persisted, nothing deleted. Guards against a false-skip on a genuinely
    /// idle channel.
    #[tokio::test]
    async fn persist_cas_persists_when_generation_unchanged() {
        let captured_generation: i64 = 3;
        let row_generation_now: i64 = 3;
        let rows_affected: u64 = if captured_generation == row_generation_now {
            1
        } else {
            0
        };

        let deleted: Rc<RefCell<Vec<(u64, u64)>>> = Rc::new(RefCell::new(Vec::new()));
        let persisted: Rc<RefCell<Vec<(u64, u64)>>> = Rc::new(RefCell::new(Vec::new()));
        let channel_id = 555u64;
        let posted_message_id = 6161u64;

        match persist_cas_outcome(rows_affected) {
            PersistCasOutcome::CasLostDeleteAndSkip => {
                deleted.borrow_mut().push((channel_id, posted_message_id));
            }
            PersistCasOutcome::Persisted => {
                persisted.borrow_mut().push((channel_id, posted_message_id));
            }
        }

        assert!(
            deleted.borrow().is_empty(),
            "no false-skip on a genuinely idle channel: nothing deleted when generation unchanged"
        );
        assert_eq!(
            persisted.borrow().as_slice(),
            &[(channel_id, posted_message_id)],
            "the card is persisted when no turn claimed during the persist window"
        );
    }

    // ---------------------------------------------------------------
    // #3148 Window 2 (capture-at-claim parity): the Discord-intake recap-clear
    // (and the generation bump) was relocated to run AFTER the mailbox claim,
    // ONLY when this message won the claim (`started == true`), and the bump
    // runs BEFORE the clear — mirroring the TUI path exactly. These tests pin
    // that ordering/gating without a live Postgres or Discord http.
    // ---------------------------------------------------------------

    /// Models the relocated Discord-intake claim sequence: when `started ==
    /// true` the order is BUMP then CLEAR, both keyed to this channel; a queued
    /// message that lost the claim (`started == false`) does NEITHER (the
    /// winning turn does).
    #[tokio::test]
    async fn intake_clear_runs_after_claim_bump_before_clear_only_when_started() {
        #[derive(Debug, PartialEq, Eq)]
        enum Step {
            Bump(u64),
            Clear(u64),
        }

        async fn run_intake_claim_sequence(
            started: bool,
            channel_id: u64,
            steps: &RefCell<Vec<Step>>,
        ) {
            // Mirrors intake_turn.rs: gated on `started`, bump BEFORE clear.
            if started {
                steps.borrow_mut().push(Step::Bump(channel_id));
                steps.borrow_mut().push(Step::Clear(channel_id));
            }
        }

        // Won the claim → bump precedes clear, both for this channel.
        let steps = RefCell::new(Vec::new());
        run_intake_claim_sequence(true, 1234, &steps).await;
        assert_eq!(
            steps.into_inner(),
            vec![Step::Bump(1234), Step::Clear(1234)],
            "started==true ⇒ bump THEN clear (parity with TUI claim → bump → clear)"
        );

        // Lost the claim race → no bump, no clear (the winning turn handles it).
        let steps = RefCell::new(Vec::new());
        run_intake_claim_sequence(false, 1234, &steps).await;
        assert!(
            steps.into_inner().is_empty(),
            "started==false (queued/lost race) ⇒ neither bump nor clear"
        );
    }

    /// Process-wide mutex: `inflight_has_active_turn` resolves the inflight
    /// root from the PROCESS-WIDE `AGENTDESK_ROOT_DIR`, so the env-mutating
    /// tests must not race the rest of the suite.
    fn active_turn_env_test_mutex() -> &'static std::sync::Mutex<()> {
        static M: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        M.get_or_init(|| std::sync::Mutex::new(()))
    }

    struct RootEnvGuard(Option<std::ffi::OsString>);
    impl Drop for RootEnvGuard {
        fn drop(&mut self) {
            match self.0.take() {
                Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
            }
        }
    }

    fn make_inflight(channel_id: u64) -> super::super::inflight::InflightTurnState {
        super::super::inflight::InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("adk-cdx".to_string()),
            7,
            channel_id + 1,
            channel_id + 1001,
            "hello".to_string(),
            None,
            Some(format!("AgentDesk-codex-adk-cdx-{channel_id}")),
            Some("/tmp/out.jsonl".to_string()),
            Some("/tmp/in.fifo".to_string()),
            0,
        )
    }

    /// A fresh, non-stale inflight for `(provider, channel_id)` — exactly the
    /// marker the TUI claim path writes when a turn becomes active — is read
    /// as an ACTIVE turn, so the post job will skip.
    #[test]
    fn channel_has_active_turn_true_for_fresh_inflight() {
        let _guard = active_turn_env_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let temp = tempfile::TempDir::new().unwrap();
        let _env = RootEnvGuard(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let channel_id = 8_146_001;
        super::super::inflight::save_inflight_state(&make_inflight(channel_id)).expect("save");

        assert!(inflight_has_active_turn(&ProviderKind::Codex, channel_id));
        // And the gate skips the post.
        assert!(!should_post_recap(inflight_has_active_turn(
            &ProviderKind::Codex,
            channel_id
        )));
    }

    /// A genuinely idle channel (no inflight on disk) is NOT active — the
    /// post job must still post the recap. This is the no-false-skip guard.
    #[test]
    fn channel_has_active_turn_false_when_no_inflight() {
        let _guard = active_turn_env_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let temp = tempfile::TempDir::new().unwrap();
        let _env = RootEnvGuard(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let channel_id = 8_146_002;
        assert!(!inflight_has_active_turn(&ProviderKind::Codex, channel_id));
        // Idle → still post.
        assert!(should_post_recap(inflight_has_active_turn(
            &ProviderKind::Codex,
            channel_id
        )));
    }

    /// A stale leftover inflight (its `updated_at` aged past the THREAD-GUARD
    /// staleness threshold) must NOT be treated as an active turn — otherwise
    /// a crashed dispatch would permanently suppress the recap on an idle
    /// channel (a false skip). This mirrors how THREAD-GUARD / the stall-
    /// watchdog treat such rows.
    #[test]
    fn channel_has_active_turn_false_for_stale_inflight() {
        let _guard = active_turn_env_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let temp = tempfile::TempDir::new().unwrap();
        let _env = RootEnvGuard(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let channel_id = 8_146_003;
        // `save_inflight_state` always re-stamps `updated_at = now`, so to
        // simulate an aged row we save normally and then rewrite the
        // persisted `updated_at` on disk to a stale value.
        super::super::inflight::save_inflight_state(&make_inflight(channel_id)).expect("save");
        let state_path = temp
            .path()
            .join("runtime")
            .join("discord_inflight")
            .join(ProviderKind::Codex.as_str())
            .join(format!("{channel_id}.json"));
        // The on-disk `updated_at` uses the same LOCAL-time `%Y-%m-%d
        // %H:%M:%S` form as `now_string()`; the staleness parser only
        // understands that form, so we must match it (an RFC3339 string would
        // be unparseable → treated as NOT stale).
        let stale_local = chrono::Local::now()
            - chrono::Duration::seconds(
                super::super::inflight::INFLIGHT_STALENESS_THRESHOLD_SECS as i64 + 60,
            );
        let stale_str = stale_local.format("%Y-%m-%d %H:%M:%S").to_string();
        let mut json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&state_path).unwrap()).unwrap();
        json["updated_at"] = serde_json::Value::String(stale_str);
        std::fs::write(&state_path, serde_json::to_string_pretty(&json).unwrap()).unwrap();

        assert!(!inflight_has_active_turn(&ProviderKind::Codex, channel_id));
    }

    /// codex R2 P1: a turn that is MAILBOX-active but whose inflight sidecar has
    /// NOT yet been written must still be detected as active by
    /// `channel_has_active_turn`, so the post job skips. This is the exact lag
    /// window between `mailbox_try_start_turn` (turn active) and the later
    /// `save_inflight_state` in `claim_tui_direct_synthetic_turn`. We simulate
    /// it by starting a mailbox turn through the global registry WITHOUT writing
    /// any inflight sidecar, then asserting the channel reads as active.
    // SAFETY (await_holding_lock): `active_turn_env_test_mutex()` is a std Mutex
    // held across awaits to serialize tests that mutate the process-global
    // `AGENTDESK_ROOT_DIR`; the hold must span the awaits so a concurrent test
    // cannot clobber the env mid-flight. Test-only.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn channel_has_active_turn_true_for_mailbox_active_without_inflight() {
        // Isolate the inflight root so no stray sidecar leaks in — the mailbox
        // signal alone must carry the detection.
        let _guard = active_turn_env_test_mutex()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let temp = tempfile::TempDir::new().unwrap();
        let _env = RootEnvGuard(std::env::var_os("AGENTDESK_ROOT_DIR"));
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", temp.path()) };

        let channel_id = 8_146_004u64;
        let chan = serenity::ChannelId::new(channel_id);

        // No inflight sidecar written — the defense-in-depth leg is idle.
        assert!(!inflight_has_active_turn(&ProviderKind::Codex, channel_id));

        // Make the MAILBOX turn active via the same global registry the claim
        // path feeds (`mailbox_try_start_turn` → `mailbox()` → global handle).
        let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
        let handle = registry.handle(chan);
        let started = handle
            .try_start_turn(
                std::sync::Arc::new(CancelToken::new()),
                serenity::UserId::new(1),
                serenity::MessageId::new(99),
            )
            .await;
        assert!(started, "mailbox turn should start");

        // Mailbox-active-but-inflight-not-yet-written ⇒ active ⇒ post skipped.
        assert!(channel_has_active_turn(&ProviderKind::Codex, channel_id).await);
        assert!(!should_post_recap(
            channel_has_active_turn(&ProviderKind::Codex, channel_id).await
        ));

        // Cleanup: finish the turn so the global handle does not leak an active
        // state into other tests sharing the process-global registry.
        handle
            .finish_turn(
                crate::services::turn_orchestrator::QueuePersistenceContext::new(
                    &ProviderKind::Codex,
                    "idle-recap-mailbox-active-test",
                    None,
                ),
            )
            .await;
        assert!(!channel_has_active_turn(&ProviderKind::Codex, channel_id).await);
    }
}
