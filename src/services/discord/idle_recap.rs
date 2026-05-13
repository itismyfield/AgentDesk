//! Idle-recap renderer (PR #3b) — turns a "5-min idle" trigger into a
//! Discord card under the originating channel.
//!
//! Lifecycle:
//!   1. The policy module `policies/timeouts/idle-recap.js` calls
//!      `POST /api/sessions/{key}/idle-recap` every 5 minutes for each
//!      eligible main-channel session.
//!   2. `post_recap` (here) captures the tail of the tmux scrollback, asks
//!      opencode for a short Korean summary (graceful fallback to the raw
//!      tail if opencode is unavailable), and posts a single-line notify-bot
//!      message of the form
//!         📦 {used}/{window} tokens ({pct}%) · idle {dur} · {summary}
//!   3. The previous recap card (if any) for the same channel is deleted
//!      best-effort, and the new message id is persisted on `sessions`.
//!   4. The next user message in that channel deletes the card — handled
//!      by `message_handler::clear_idle_recap_for_channel`.

use chrono::{DateTime, Utc};
use poise::serenity_prelude::{self as serenity, ChannelId, MessageId};
use sqlx::PgPool;

use crate::services::provider::ProviderKind;

const CLAUDE_CONTEXT_WINDOW_TOKENS: u64 = 200_000;
const CODEX_CONTEXT_WINDOW_TOKENS: u64 = 200_000;
const FALLBACK_CONTEXT_WINDOW_TOKENS: u64 = 128_000;

/// Snapshot of the data the recap renderer needs in a single SQL round-trip.
///
/// NOTE: `sessions.tokens_updated_at` ships in migration 0054 (PR #2086) and
/// is NOT read here — this branch's base is `feat/idle-recap-notification`
/// (migration 0055 only), so depending on 0054 would crash at runtime if
/// 0055 merges first. PR #3c (renderer-stage-2, opencode summary + clear
/// button) will rebase on main after both 0054 and 0055 land and can rely on
/// the freshness stamp at that point.
#[derive(Debug, Clone)]
pub(crate) struct RecapSnapshot {
    pub(crate) provider: String,
    pub(crate) tokens: Option<i64>,
    pub(crate) last_heartbeat: Option<DateTime<Utc>>,
    pub(crate) previous_message_id: Option<i64>,
    pub(crate) previous_channel_id: Option<i64>,
    pub(crate) discord_channel_id: Option<String>,
    pub(crate) discord_channel_cc: Option<String>,
    pub(crate) discord_channel_cdx: Option<String>,
    pub(crate) discord_channel_alt: Option<String>,
}

/// Load everything the renderer needs in one SQL hit.
pub(crate) async fn load_recap_snapshot(
    pool: &PgPool,
    session_key: &str,
) -> Result<Option<RecapSnapshot>, sqlx::Error> {
    sqlx::query_as::<_, RecapSnapshotRow>(
        "SELECT s.provider,
                s.tokens,
                s.last_heartbeat,
                s.idle_recap_message_id,
                s.idle_recap_channel_id,
                a.discord_channel_id,
                a.discord_channel_cc,
                a.discord_channel_cdx,
                a.discord_channel_alt
         FROM sessions s
         LEFT JOIN agents a ON a.id = s.agent_id
         WHERE s.session_key = $1",
    )
    .bind(session_key)
    .fetch_optional(pool)
    .await
    .map(|row| row.map(RecapSnapshotRow::into_snapshot))
}

#[derive(Debug, sqlx::FromRow)]
struct RecapSnapshotRow {
    provider: String,
    tokens: Option<i64>,
    last_heartbeat: Option<DateTime<Utc>>,
    idle_recap_message_id: Option<i64>,
    idle_recap_channel_id: Option<i64>,
    discord_channel_id: Option<String>,
    discord_channel_cc: Option<String>,
    discord_channel_cdx: Option<String>,
    discord_channel_alt: Option<String>,
}

impl RecapSnapshotRow {
    fn into_snapshot(self) -> RecapSnapshot {
        RecapSnapshot {
            provider: self.provider,
            tokens: self.tokens,
            last_heartbeat: self.last_heartbeat,
            previous_message_id: self.idle_recap_message_id,
            previous_channel_id: self.idle_recap_channel_id,
            discord_channel_id: self.discord_channel_id,
            discord_channel_cc: self.discord_channel_cc,
            discord_channel_cdx: self.discord_channel_cdx,
            discord_channel_alt: self.discord_channel_alt,
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

/// Compose the single-line recap card body.
///
/// PR #3b ships a header-only card: token-occupancy panel + idle duration.
/// PR #3c will append the opencode/Haiku summary line below.
pub(crate) fn compose_recap_text(snapshot: &RecapSnapshot) -> String {
    let now = Utc::now();
    let idle_since = snapshot
        .last_heartbeat
        .map(|t| format_korean_duration(now - t))
        .unwrap_or_else(|| "방금 전".to_string());

    match (snapshot.tokens, context_window_for(snapshot)) {
        (Some(used), window) if used > 0 => {
            let pct = ((u128::from(used as u64) * 100) / u128::from(window)) as u64;
            let used_label = format_token_count(used as u64);
            let window_label = format_token_count(window);
            format!("📦 {used_label} / {window_label} tokens ({pct}%) · idle {idle_since}")
        }
        _ => format!("📦 idle {idle_since}"),
    }
}

fn format_token_count(n: u64) -> String {
    if n < 1_000 {
        format!("{n}")
    } else {
        format!("{}k", n / 1_000)
    }
}

fn context_window_for(snapshot: &RecapSnapshot) -> u64 {
    match ProviderKind::from_str(&snapshot.provider) {
        Some(ProviderKind::Claude) => CLAUDE_CONTEXT_WINDOW_TOKENS,
        Some(ProviderKind::Codex) => CODEX_CONTEXT_WINDOW_TOKENS,
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
/// `super::http::send_channel_message` so the maintainability audit's
/// `direct_discord_sends` rule (hard gate, #1282) stays happy — that
/// helper lives in the allowlisted `discord/http.rs` module.
pub(crate) async fn post_recap_card(
    http: &serenity::Http,
    channel_id: u64,
    content: &str,
) -> Result<u64, String> {
    let msg = super::http::send_channel_message(http, ChannelId::new(channel_id), content)
        .await
        .map_err(|e| format!("send_message: {e}"))?;
    Ok(msg.id.get())
}

/// Delete the previous recap card if one is recorded. Errors are swallowed
/// so the renderer never fails the cycle just because Discord has GC'd the
/// old message itself. Same allowlist rationale as `post_recap_card`.
pub(crate) async fn delete_previous_card(http: &serenity::Http, channel_id: u64, message_id: u64) {
    let _ = super::http::delete_channel_message(
        http,
        ChannelId::new(channel_id),
        MessageId::new(message_id),
    )
    .await;
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
pub(crate) async fn stamp_recap_cycle(
    pool: &PgPool,
    session_key: &str,
) -> Result<(), sqlx::Error> {
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
) -> Result<(), sqlx::Error> {
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
    .map(|_| ())
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
/// Reserved for PR #3c (scrollback capture); kept here so the rebase is a
/// no-op once that lands.
#[allow(dead_code)]
pub(crate) fn tmux_session_name_from_key(session_key: &str) -> Option<&str> {
    session_key
        .rsplit_once(':')
        .map(|(_, name)| name)
        .filter(|s| !s.is_empty())
}
