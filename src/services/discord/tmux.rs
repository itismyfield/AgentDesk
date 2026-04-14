use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock};

use poise::serenity_prelude as serenity;
use serenity::ChannelId;

use crate::db::session_transcripts::{SessionTranscriptEvent, SessionTranscriptEventKind};
use crate::db::turns::TurnTokenUsage;
use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};
use crate::services::session_backend::StreamLineState;
use crate::services::tmux_diagnostics::{
    build_tmux_death_diagnostic, read_tmux_exit_reason, record_tmux_exit_reason,
    tmux_session_exists, tmux_session_has_live_pane,
};

use super::formatting::{
    build_streaming_placeholder_text, format_tool_input, plan_streaming_rollover,
    replace_long_message_raw, send_long_message_raw, truncate_str,
};
use super::settings::{
    channel_supports_provider, load_last_remote_profile, load_last_session_path,
    resolve_role_binding, validate_bot_channel_routing_with_provider_channel,
};
use super::{SharedData, TmuxWatcherHandle, rate_limit_wait};

const PROVIDER_OVERLOAD_MAX_RETRIES: u8 = 3;
const READY_FOR_INPUT_IDLE_PROBE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

static PROVIDER_OVERLOAD_RETRY_STATE: LazyLock<dashmap::DashMap<u64, ProviderOverloadRetryState>> =
    LazyLock::new(dashmap::DashMap::new);

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProviderOverloadRetryState {
    fingerprint: String,
    attempts: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ProviderOverloadDecision {
    Retry {
        attempt: u8,
        delay: std::time::Duration,
        fingerprint: String,
    },
    Exhausted,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct WatcherLineOutcome {
    pub found_result: bool,
    pub is_prompt_too_long: bool,
    pub is_auth_error: bool,
    pub auth_error_message: Option<String>,
    pub is_provider_overloaded: bool,
    pub provider_overload_message: Option<String>,
    pub stale_resume_detected: bool,
    pub auto_compacted: bool,
}

fn stream_line_state_token_usage(state: &StreamLineState) -> Option<TurnTokenUsage> {
    let usage = TurnTokenUsage {
        input_tokens: state.accum_input_tokens,
        cache_create_tokens: state.accum_cache_create_tokens,
        cache_read_tokens: state.accum_cache_read_tokens,
        output_tokens: state.accum_output_tokens,
    };
    (usage.input_tokens > 0
        || usage.cache_create_tokens > 0
        || usage.cache_read_tokens > 0
        || usage.output_tokens > 0)
        .then_some(usage)
}

fn watcher_ready_for_input_turn_completed(
    tracker: &mut crate::services::provider::ReadyForInputIdleTracker,
    data_start_offset: u64,
    current_offset: u64,
    ready_for_input: bool,
    now: std::time::Instant,
) -> bool {
    tracker.observe_idle(current_offset > data_start_offset, ready_for_input, now)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DeadSessionCleanupPlan {
    preserve_tmux_session: bool,
    report_idle_status: bool,
}

fn dead_session_cleanup_plan(dispatch_protected: bool) -> DeadSessionCleanupPlan {
    DeadSessionCleanupPlan {
        preserve_tmux_session: dispatch_protected,
        report_idle_status: true,
    }
}

fn is_prompt_too_long_message(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("prompt is too long")
        || lower.contains("prompt too long")
        || lower.contains("context_length_exceeded")
        || lower.contains("conversation too long")
        || lower.contains("context window")
}

fn is_auth_error_message(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("not logged in")
        || lower.contains("authentication error")
        || lower.contains("unauthorized")
        || lower.contains("please run /login")
        || lower.contains("oauth")
        || lower.contains("access token could not be refreshed")
        || (lower.contains("refresh token")
            && (lower.contains("expired")
                || lower.contains("invalid")
                || lower.contains("revoked")))
        || lower.contains("token expired")
        || lower.contains("invalid api key")
        || (lower.contains("api key")
            && (lower.contains("missing")
                || lower.contains("invalid")
                || lower.contains("expired")))
}

fn detect_provider_overload_message(text: &str) -> Option<String> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }

    let lower = trimmed.to_lowercase();
    let looks_overloaded = lower.contains("selected model is at capacity")
        || lower.contains("model is at capacity")
        || (lower.contains("at capacity") && lower.contains("model"))
        || lower.contains("try a different model")
        || lower.contains("rate limit")
        || lower.contains("too many requests")
        || lower.contains("provider overloaded")
        || lower.contains("server overloaded")
        || lower.contains("service overloaded")
        || lower.contains("overloaded")
        || lower.contains("please try again later");

    if looks_overloaded {
        Some(trimmed.to_string())
    } else {
        None
    }
}

fn extract_result_error_text(value: &serde_json::Value) -> String {
    let errors = value
        .get("errors")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::trim))
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();

    if !errors.trim().is_empty() {
        errors
    } else {
        value
            .get("result")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string()
    }
}

fn load_restored_session_cwd_from_conn(
    conn: &rusqlite::Connection,
    session_keys: &[String],
) -> Option<String> {
    session_keys.iter().find_map(|session_key| {
        conn.query_row(
            "SELECT cwd FROM sessions WHERE session_key = ?1",
            [session_key],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .filter(|path| !path.is_empty() && std::path::Path::new(path).is_dir())
    })
}

fn normalized_retry_payload_text(user_text: &str) -> &str {
    let trimmed = user_text.trim();
    if let Some((header, body)) = trimmed.split_once("\n\n") {
        if header.contains("이전 대화 복원") || header.contains("자동 재시도") {
            return body.trim();
        }
    }
    trimmed
}

fn provider_overload_fingerprint(user_text: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    normalized_retry_payload_text(user_text).hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn provider_overload_retry_delay(attempt: u8) -> std::time::Duration {
    let shift = u32::from(attempt.saturating_sub(1));
    std::time::Duration::from_secs(120 * (1u64 << shift))
}

fn push_transcript_event(events: &mut Vec<SessionTranscriptEvent>, event: SessionTranscriptEvent) {
    let has_payload = !event.content.trim().is_empty()
        || event
            .summary
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty())
        || event
            .tool_name
            .as_deref()
            .is_some_and(|value| !value.trim().is_empty());
    if has_payload
        || matches!(
            event.kind,
            SessionTranscriptEventKind::Thinking
                | SessionTranscriptEventKind::Result
                | SessionTranscriptEventKind::Error
                | SessionTranscriptEventKind::Task
                | SessionTranscriptEventKind::System
        )
    {
        events.push(event);
    }
}

fn inflight_duration_ms(started_at: Option<&str>) -> Option<i64> {
    let started_at = started_at?.trim();
    if started_at.is_empty() {
        return None;
    }
    let parsed = chrono::NaiveDateTime::parse_from_str(started_at, "%Y-%m-%d %H:%M:%S").ok()?;
    let elapsed = chrono::Local::now().naive_local() - parsed;
    Some(elapsed.num_milliseconds().max(0))
}

fn load_restored_provider_session_id(
    db: Option<&crate::db::Db>,
    token_hash: &str,
    provider: &ProviderKind,
    channel_name: &str,
) -> Option<String> {
    let tmux_name = provider.build_tmux_session_name(channel_name);
    let session_keys =
        super::adk_session::build_session_key_candidates(token_hash, provider, &tmux_name);

    db.and_then(|db| {
        db.lock().ok().and_then(|conn| {
            session_keys.iter().find_map(|session_key| {
                conn.query_row(
                    "SELECT claude_session_id FROM sessions WHERE session_key = ?1 AND provider = ?2",
                    rusqlite::params![session_key, provider.as_str()],
                    |row| row.get::<_, Option<String>>(0),
                )
                .ok()
                .flatten()
                .filter(|session_id| !session_id.is_empty())
            })
        })
    })
}

fn clear_provider_overload_retry_state(channel_id: ChannelId) {
    PROVIDER_OVERLOAD_RETRY_STATE.remove(&channel_id.get());
}

fn record_provider_overload_retry(
    channel_id: ChannelId,
    user_text: &str,
) -> ProviderOverloadDecision {
    let fingerprint = provider_overload_fingerprint(user_text);
    let next_attempt = PROVIDER_OVERLOAD_RETRY_STATE
        .get(&channel_id.get())
        .and_then(|state| {
            if state.fingerprint == fingerprint {
                Some(state.attempts.saturating_add(1))
            } else {
                None
            }
        })
        .unwrap_or(1);

    if next_attempt > PROVIDER_OVERLOAD_MAX_RETRIES {
        clear_provider_overload_retry_state(channel_id);
        ProviderOverloadDecision::Exhausted
    } else {
        PROVIDER_OVERLOAD_RETRY_STATE.insert(
            channel_id.get(),
            ProviderOverloadRetryState {
                fingerprint: fingerprint.clone(),
                attempts: next_attempt,
            },
        );
        ProviderOverloadDecision::Retry {
            attempt: next_attempt,
            delay: provider_overload_retry_delay(next_attempt),
            fingerprint,
        }
    }
}

fn schedule_provider_overload_retry(
    shared: Arc<SharedData>,
    http: Arc<serenity::Http>,
    provider: ProviderKind,
    channel_id: ChannelId,
    user_message_id: serenity::MessageId,
    retry_text: String,
    attempt: u8,
    delay: std::time::Duration,
    fingerprint: String,
) {
    tokio::spawn(async move {
        tokio::time::sleep(delay).await;

        if shared.shutting_down.load(Ordering::Relaxed) {
            return;
        }

        let should_send = PROVIDER_OVERLOAD_RETRY_STATE
            .get(&channel_id.get())
            .map(|state| state.fingerprint == fingerprint && state.attempts == attempt)
            .unwrap_or(false);
        if !should_send {
            return;
        }

        if super::mailbox_has_active_turn(&shared, channel_id).await {
            clear_provider_overload_retry_state(channel_id);
            return;
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ↻ watcher overload auto-retry: channel {} attempt {}/{} after {}s",
            channel_id.get(),
            attempt,
            PROVIDER_OVERLOAD_MAX_RETRIES,
            delay.as_secs()
        );
        super::turn_bridge::auto_retry_with_history(
            &http,
            &shared,
            &provider,
            channel_id,
            user_message_id,
            &retry_text,
        )
        .await;
    });
}

async fn clear_provider_session_for_retry(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    tmux_session_name: &str,
    fallback_session_id: Option<&str>,
) {
    let stale_sid = {
        let mut data = shared.core.lock().await;
        let old = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.session_id.clone())
            .or_else(|| fallback_session_id.map(ToString::to_string));
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
        }
        old
    };

    let session_key = format!(
        "{}:{}",
        crate::services::platform::hostname_short(),
        tmux_session_name
    );
    super::adk_session::clear_provider_session_id(&session_key, shared.api_port).await;

    if let Some(sid) = stale_sid {
        let _ = super::internal_api::clear_stale_session_id(&sid).await;
    }
}

async fn resolve_watcher_dispatch_id(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    inflight_state: Option<&super::inflight::InflightTurnState>,
) -> Option<String> {
    inflight_state
        .and_then(|state| state.dispatch_id.clone())
        .or_else(|| {
            inflight_state.and_then(|state| super::adk_session::parse_dispatch_id(&state.user_text))
        })
        .or(super::adk_session::lookup_pending_dispatch_for_thread(
            shared.api_port,
            channel_id.get(),
        )
        .await)
        .or_else(|| {
            resolve_dispatched_thread_dispatch_from_db(shared.db.as_ref(), channel_id.get())
        })
}

/// #226: Atomically claim a channel for watcher creation using DashMap::entry().
/// Returns true if the claim succeeded (caller should spawn the watcher).
/// Returns false if a watcher already exists (caller should skip).
pub(super) fn try_claim_watcher(
    watchers: &dashmap::DashMap<ChannelId, TmuxWatcherHandle>,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
) -> bool {
    use dashmap::mapref::entry::Entry;
    match watchers.entry(channel_id) {
        Entry::Occupied(_) => false,
        Entry::Vacant(entry) => {
            entry.insert(handle);
            true
        }
    }
}

/// #243: Claim a channel for watcher creation, cancelling any existing watcher.
/// Unlike try_claim_watcher (which skips if occupied), this always succeeds:
/// if a watcher already exists, it is cancelled and replaced.
/// Returns true if a fresh slot was created, false if an existing watcher was replaced.
pub(super) fn claim_or_replace_watcher(
    watchers: &dashmap::DashMap<ChannelId, TmuxWatcherHandle>,
    channel_id: ChannelId,
    handle: TmuxWatcherHandle,
) -> bool {
    use dashmap::mapref::entry::Entry;
    match watchers.entry(channel_id) {
        Entry::Occupied(mut entry) => {
            // Cancel the existing watcher — it will exit on its next loop iteration
            // and skip DashMap removal (since cancel is set).
            entry
                .get()
                .cancel
                .store(true, std::sync::atomic::Ordering::Relaxed);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ♻ watcher replaced for channel {} — cancelled stale watcher",
                channel_id
            );
            entry.insert(handle);
            false
        }
        Entry::Vacant(entry) => {
            entry.insert(handle);
            true
        }
    }
}

use crate::utils::format::tail_with_ellipsis;

use crate::services::tmux_common::{current_tmux_owner_marker, tmux_owner_path};

pub(super) fn session_belongs_to_current_runtime(
    session_name: &str,
    current_owner_marker: &str,
) -> bool {
    std::fs::read_to_string(tmux_owner_path(session_name))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| value == current_owner_marker)
        .unwrap_or(false)
}

fn build_restart_handoff_context(
    state: &super::inflight::InflightTurnState,
    best_response: &str,
) -> String {
    let partial = best_response.trim();
    let partial_context = if partial.is_empty() {
        "(재시작 전까지 전달된 partial 응답 없음)".to_string()
    } else {
        tail_with_ellipsis(partial, 1200)
    };
    format!(
        "재시작 중 기존 tmux 세션이 종료되어 동일 turn에 재연결하지 못했습니다.\n\n원래 사용자 요청:\n{}\n\n재시작 전 partial 응답:\n{}",
        state.user_text.trim(),
        partial_context,
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RestartHandoffScope {
    ExactMetadata,
    ProviderChannelScopedFallback,
}

fn resolve_restart_handoff_scope(
    state: &super::inflight::InflightTurnState,
    tmux_session_name: &str,
    output_path: &str,
) -> RestartHandoffScope {
    let tmux_matches = state.tmux_session_name.as_deref() == Some(tmux_session_name);
    let output_matches = state.output_path.as_deref() == Some(output_path);
    if tmux_matches || output_matches {
        RestartHandoffScope::ExactMetadata
    } else {
        RestartHandoffScope::ProviderChannelScopedFallback
    }
}

fn resolve_dispatched_thread_dispatch_from_conn(
    conn: &rusqlite::Connection,
    thread_channel_id: u64,
) -> Option<String> {
    let thread_channel_id = thread_channel_id.to_string();

    conn.query_row(
        "SELECT id FROM task_dispatches
         WHERE status = 'dispatched' AND thread_id = ?1
         ORDER BY datetime(created_at) DESC, rowid DESC
         LIMIT 1",
        [thread_channel_id.as_str()],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .or_else(|| {
        conn.query_row(
            "SELECT active_dispatch_id FROM sessions
             WHERE thread_channel_id = ?1
               AND status = 'working'
               AND active_dispatch_id IS NOT NULL
             ORDER BY datetime(COALESCE(last_heartbeat, created_at)) DESC, id DESC
             LIMIT 1",
            [thread_channel_id.as_str()],
            |row| row.get::<_, String>(0),
        )
        .ok()
    })
}

fn resolve_dispatched_thread_dispatch_from_db(
    db: Option<&crate::db::Db>,
    thread_channel_id: u64,
) -> Option<String> {
    let db = db?;
    let conn = db.separate_conn().ok()?;
    resolve_dispatched_thread_dispatch_from_conn(&conn, thread_channel_id)
}

pub(super) async fn start_restart_handoff_from_state(
    channel_id: ChannelId,
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider_kind: &crate::services::provider::ProviderKind,
    state: super::inflight::InflightTurnState,
    best_response: &str,
) -> bool {
    let stale_text = super::turn_bridge::stale_inflight_message(best_response);
    let _ = super::formatting::replace_long_message_raw(
        http,
        channel_id,
        serenity::MessageId::new(state.current_msg_id),
        &stale_text,
        shared,
    )
    .await;

    let context = build_restart_handoff_context(&state, best_response);
    let handoff_prompt = format!(
        "dcserver가 재시작되었습니다. 재시작 전 작업의 후속 조치를 이어서 진행해주세요.\n\n## 재시작 전 컨텍스트\n{}\n\n## 요청 사항\n재시작 중 중단된 응답을 이어서 마무리",
        context
    );
    let placeholder_id = match channel_id
        .send_message(
            http,
            serenity::CreateMessage::new()
                .content("📎 **Post-restart handoff** — 재시작 후속 작업을 자동으로 이어받습니다."),
        )
        .await
    {
        Ok(msg) => msg.id,
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ failed to send watcher-handoff placeholder for channel {}: {}",
                channel_id.get(),
                e
            );
            serenity::MessageId::new(state.current_msg_id)
        }
    };

    let author_id = serenity::UserId::new(1);
    let mut started_immediately = false;
    if let (Some(ctx), Some(token)) = (
        shared.cached_serenity_ctx.get(),
        shared.cached_bot_token.get(),
    ) {
        match super::router::handle_text_message(
            ctx,
            channel_id,
            placeholder_id,
            author_id,
            "system",
            &handoff_prompt,
            shared,
            token,
            true,
            false,
            false,
            false,
            None,
            false,
        )
        .await
        {
            Ok(()) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ watcher death recovery: started immediate handoff turn for channel {}",
                    channel_id.get()
                );
                started_immediately = true;
            }
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⚠ watcher death recovery: immediate handoff start failed for channel {}: {}",
                    channel_id.get(),
                    e
                );
            }
        }
    }

    if !started_immediately {
        super::mailbox_enqueue_intervention(
            shared,
            provider_kind,
            channel_id,
            super::Intervention {
                author_id,
                message_id: placeholder_id,
                source_message_ids: vec![placeholder_id],
                text: handoff_prompt,
                mode: super::InterventionMode::Soft,
                created_at: std::time::Instant::now(),
                reply_context: None,
                has_reply_boundary: false,
                merge_consecutive: false,
            },
        )
        .await;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ watcher death recovery: queued fallback handoff for channel {}",
            channel_id.get()
        );
    }

    super::inflight::clear_inflight_state(provider_kind, channel_id.get());
    true
}

async fn resume_aborted_restart_turn(
    channel_id: ChannelId,
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    tmux_session_name: &str,
    output_path: &str,
) -> bool {
    let Some((provider_kind, _)) = parse_provider_and_channel_from_tmux_name(tmux_session_name)
    else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⚠ watcher death recovery: failed to parse provider/channel from tmux session {}",
            tmux_session_name
        );
        return false;
    };
    let Some(state) = super::inflight::load_inflight_state(&provider_kind, channel_id.get()) else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⚠ watcher death recovery: no inflight state for channel {} (provider {})",
            channel_id.get(),
            provider_kind.as_str()
        );
        return false;
    };

    let scope = resolve_restart_handoff_scope(&state, tmux_session_name, output_path);
    if matches!(scope, RestartHandoffScope::ProviderChannelScopedFallback) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ watcher death recovery: inflight metadata mismatch for channel {} (state tmux: {:?}, watcher tmux: {}, state output: {:?}, watcher output: {}) — proceeding with provider/channel scoped handoff",
            channel_id.get(),
            state.tmux_session_name.as_deref(),
            tmux_session_name,
            state.output_path.as_deref(),
            output_path
        );
    }

    let extracted_full = super::recovery::extract_response_from_output_pub(output_path, 0);
    let best_response = if matches!(scope, RestartHandoffScope::ProviderChannelScopedFallback) {
        state.full_response.clone()
    } else if !extracted_full.trim().is_empty() {
        extracted_full
    } else {
        state.full_response.clone()
    };
    start_restart_handoff_from_state(
        channel_id,
        http,
        shared,
        &provider_kind,
        state,
        &best_response,
    )
    .await
}

/// Background watcher that continuously tails a tmux output file.
/// When Claude produces output from terminal input (not Discord), relay it to Discord.
pub(super) async fn tmux_output_watcher(
    channel_id: ChannelId,
    http: Arc<serenity::Http>,
    shared: Arc<SharedData>,
    output_path: String,
    tmux_session_name: String,
    initial_offset: u64,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    paused: Arc<std::sync::atomic::AtomicBool>,
    resume_offset: Arc<std::sync::Mutex<Option<u64>>>,
    pause_epoch: Arc<std::sync::atomic::AtomicU64>,
    turn_delivered: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::io::{Read, Seek, SeekFrom};

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 👁 tmux watcher started for #{tmux_session_name} at offset {initial_offset}"
    );

    let watcher_provider = parse_provider_and_channel_from_tmux_name(&tmux_session_name)
        .map(|(provider, _)| provider)
        .unwrap_or(crate::services::provider::ProviderKind::Claude);
    let mut current_offset = initial_offset;
    let mut prompt_too_long_killed = false;
    let mut turn_result_relayed = false;
    // Guard against duplicate relay: track the offset from which the last relay was sent.
    // If the outer loop circles back and current_offset hasn't advanced past this point,
    // the relay is suppressed.
    let mut last_relayed_offset: Option<u64> = None;

    loop {
        // Always consume resume_offset first — the turn bridge may have set it
        // between the previous paused check and now, so reading it here prevents
        // the watcher from using a stale current_offset after unpausing.
        if let Some(new_offset) = resume_offset.lock().ok().and_then(|mut g| g.take()) {
            current_offset = new_offset;
            // Clear turn_delivered: the watcher is now starting from a fresh offset
            // set by the turn bridge, so future data at this offset is safe to relay.
            turn_delivered.store(false, Ordering::Relaxed);
            // Reset duplicate-relay guard: new offset means new data range.
            last_relayed_offset = None;
        }

        // Check cancel or global shutdown (both exit quietly, no "session ended" message)
        if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
            break;
        }

        // If paused (Discord handler is processing its own turn), wait
        if paused.load(Ordering::Relaxed) {
            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            continue;
        }

        // Snapshot pause epoch — if this changes later, a Discord turn claimed this data
        let epoch_snapshot = pause_epoch.load(Ordering::Relaxed);

        // Check if tmux session is still alive (with timeout to prevent
        // blocking thread pool exhaustion if tmux hangs)
        let alive = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking({
                let name = tmux_session_name.clone();
                move || tmux_session_has_live_pane(&name)
            }),
        )
        .await
        .unwrap_or(Ok(false))
        .unwrap_or(false);

        if !alive {
            // Re-check shutdown/cancel — SIGTERM handler may have set the flag
            // between the top-of-loop check and here
            if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
                );
                break;
            }
            // Extra grace: wait briefly and re-check, since SIGTERM handler is async
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended during shutdown, exiting quietly"
                );
                break;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            if let Some(diag) = build_tmux_death_diagnostic(&tmux_session_name, Some(&output_path))
            {
                tracing::info!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended, watcher stopping ({diag})"
                );
            } else {
                tracing::info!(
                    "  [{ts}] 👁 tmux session {tmux_session_name} ended, watcher stopping"
                );
            }
            // Notify: tmux session termination with reason
            // Skip if force-kill already sent its own notification via dispatched_sessions.rs
            {
                let reason_short = read_tmux_exit_reason(&tmux_session_name)
                    .unwrap_or_else(|| "unknown".to_string());
                let is_force_kill = reason_short.contains("force-kill");
                if !is_force_kill {
                    // Strip timestamp prefix if present (format: "[YYYY-MM-DD HH:MM:SS] reason")
                    let reason_text = reason_short
                        .strip_prefix('[')
                        .and_then(|s| s.find("] ").map(|i| &s[i + 2..]))
                        .unwrap_or(&reason_short);
                    let reason_truncated: String = reason_text.chars().take(100).collect();
                    if let Some(ref db) = shared.db {
                        if let Ok(conn) = db.lock() {
                            let _ = conn.execute(
                                "INSERT INTO message_outbox (target, content, bot, source) VALUES (?1, ?2, 'notify', 'system')",
                                rusqlite::params![
                                    format!("channel:{}", channel_id.get()),
                                    format!("🔴 세션 종료: {reason_truncated}"),
                                ],
                            );
                        }
                    }
                }
            }
            if !prompt_too_long_killed && !turn_result_relayed {
                // Suppress warning for normal dispatch completion — not an error
                let is_normal_completion = read_tmux_exit_reason(&tmux_session_name)
                    .map(|r| r.contains("dispatch turn completed"))
                    .unwrap_or(false);
                if !is_normal_completion {
                    let _ = resume_aborted_restart_turn(
                        channel_id,
                        &http,
                        &shared,
                        &tmux_session_name,
                        &output_path,
                    )
                    .await;
                }
            }
            break;
        }

        // Try to read new data from output file
        let read_result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking({
                let path = output_path.clone();
                let offset = current_offset;
                move || -> Result<(Vec<u8>, u64), String> {
                    let mut file =
                        std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
                    file.seek(SeekFrom::Start(offset))
                        .map_err(|e| format!("seek: {}", e))?;
                    let mut buf = vec![0u8; 16384];
                    let n = file.read(&mut buf).map_err(|e| format!("read: {}", e))?;
                    buf.truncate(n);
                    Ok((buf, offset + n as u64))
                }
            }),
        )
        .await;

        let (data, new_offset) = match read_result {
            Ok(Ok(Ok((data, off)))) => (data, off),
            _ => {
                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
                continue;
            }
        };

        if data.is_empty() {
            // No new data, sleep and retry
            tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            continue;
        }

        // We got new data while not paused — this means terminal input triggered a response
        let data_start_offset = current_offset; // offset where this read batch started
        current_offset = new_offset;

        // Collect the full turn: keep reading until we see a "result" event
        let mut all_data = String::from_utf8_lossy(&data).to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();
        let narrate_progress = super::settings::load_narrate_progress(shared.db.as_ref());

        // Create a placeholder message for real-time status display
        const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
        let mut spin_idx: usize = 0;
        let mut placeholder_msg_id: Option<serenity::MessageId> = None;
        let mut last_edit_text = String::new();
        let mut response_sent_offset = 0usize;

        // Process any complete lines we already have
        let initial_outcome = process_watcher_lines(
            &mut all_data,
            &mut state,
            &mut full_response,
            &mut tool_state,
        );
        let mut found_result = initial_outcome.found_result;
        let mut is_prompt_too_long = initial_outcome.is_prompt_too_long;
        let mut is_auth_error = initial_outcome.is_auth_error;
        let mut auth_error_message = initial_outcome.auth_error_message;
        let mut is_provider_overloaded = initial_outcome.is_provider_overloaded;
        let mut provider_overload_message = initial_outcome.provider_overload_message;
        let mut stale_resume_detected = initial_outcome.stale_resume_detected;

        // Keep reading until result or timeout
        // Check if a Discord turn claimed this data since our epoch snapshot
        let epoch_changed = pause_epoch.load(Ordering::Relaxed) != epoch_snapshot;
        let mut was_paused = paused.load(Ordering::Relaxed) || epoch_changed;
        if was_paused {
            // A Discord turn took over — discard what we read
            continue;
        }
        if !found_result {
            let turn_start = tokio::time::Instant::now();
            let turn_timeout = super::turn_watchdog_timeout();
            let mut last_status_update = tokio::time::Instant::now();
            let mut ready_for_input_tracker =
                crate::services::provider::ReadyForInputIdleTracker::default();
            let mut last_ready_probe_at: Option<std::time::Instant> = None;

            while !found_result && turn_start.elapsed() < turn_timeout {
                if cancel.load(Ordering::Relaxed) || shared.shutting_down.load(Ordering::Relaxed) {
                    break;
                }
                if paused.load(Ordering::Relaxed) {
                    was_paused = true;
                    break;
                }

                let read_more = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    tokio::task::spawn_blocking({
                        let path = output_path.clone();
                        let offset = current_offset;
                        move || -> Result<(Vec<u8>, u64), String> {
                            let mut file =
                                std::fs::File::open(&path).map_err(|e| format!("open: {}", e))?;
                            file.seek(SeekFrom::Start(offset))
                                .map_err(|e| format!("seek: {}", e))?;
                            let mut buf = vec![0u8; 16384];
                            let n = file.read(&mut buf).map_err(|e| format!("read: {}", e))?;
                            buf.truncate(n);
                            Ok((buf, offset + n as u64))
                        }
                    }),
                )
                .await;

                match read_more {
                    Ok(Ok(Ok((chunk, off)))) if !chunk.is_empty() => {
                        current_offset = off;
                        ready_for_input_tracker.record_output();
                        all_data.push_str(&String::from_utf8_lossy(&chunk));
                        let outcome = process_watcher_lines(
                            &mut all_data,
                            &mut state,
                            &mut full_response,
                            &mut tool_state,
                        );
                        found_result = found_result || outcome.found_result;
                        is_prompt_too_long = is_prompt_too_long || outcome.is_prompt_too_long;
                        is_auth_error = is_auth_error || outcome.is_auth_error;
                        if auth_error_message.is_none() {
                            auth_error_message = outcome.auth_error_message;
                        }
                        is_provider_overloaded =
                            is_provider_overloaded || outcome.is_provider_overloaded;
                        stale_resume_detected =
                            stale_resume_detected || outcome.stale_resume_detected;
                        if provider_overload_message.is_none() {
                            provider_overload_message = outcome.provider_overload_message;
                        }
                        // Notify when auto-compaction is detected in output
                        if outcome.auto_compacted {
                            if let Some(ref db) = shared.db {
                                if let Ok(conn) = db.lock() {
                                    let _ = conn.execute(
                                        "INSERT INTO message_outbox (target, content, bot, source) VALUES (?1, ?2, 'notify', 'system')",
                                        rusqlite::params![
                                            format!("channel:{}", channel_id.get()),
                                            "🗜️ 자동 컨텍스트 압축 감지",
                                        ],
                                    );
                                }
                            }
                        }
                    }
                    Ok(Ok(Ok((_, off)))) => {
                        current_offset = off;
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        let now = std::time::Instant::now();
                        let should_probe_ready = last_ready_probe_at
                            .map(|last| {
                                now.duration_since(last) >= READY_FOR_INPUT_IDLE_PROBE_INTERVAL
                            })
                            .unwrap_or(true);
                        if should_probe_ready {
                            last_ready_probe_at = Some(now);
                            let ready_for_input = tokio::time::timeout(
                                std::time::Duration::from_secs(5),
                                tokio::task::spawn_blocking({
                                    let name = tmux_session_name.clone();
                                    move || {
                                        crate::services::provider::tmux_session_ready_for_input(
                                            &name,
                                        )
                                    }
                                }),
                            )
                            .await
                            .unwrap_or(Ok(false))
                            .unwrap_or(false);
                            if watcher_ready_for_input_turn_completed(
                                &mut ready_for_input_tracker,
                                data_start_offset,
                                current_offset,
                                ready_for_input,
                                now,
                            ) {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] 👁 watcher synthesized completion for {tmux_session_name}: tmux ready for input with idle output at offset {current_offset}"
                                );
                                found_result = true;
                            }
                        }
                    }
                    _ => {
                        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                    }
                }

                // Check for stale session error during streaming — abort relay immediately.
                // Only structured error/result events can trip this flag.
                if stale_resume_detected {
                    break;
                }

                // Update Discord placeholder at configurable interval
                if last_status_update.elapsed() >= super::status_update_interval() {
                    last_status_update = tokio::time::Instant::now();
                    let indicator = SPINNER[spin_idx % SPINNER.len()];
                    spin_idx += 1;

                    loop {
                        let current_portion =
                            full_response.get(response_sent_offset..).unwrap_or("");
                        if current_portion.is_empty() {
                            break;
                        }

                        let status_block = super::formatting::build_placeholder_status_block(
                            indicator,
                            tool_state.prev_tool_status.as_deref(),
                            tool_state.current_tool_line.as_deref(),
                            &full_response,
                            narrate_progress,
                        );
                        let Some(msg_id) = placeholder_msg_id else {
                            break;
                        };
                        let Some(plan) = plan_streaming_rollover(current_portion, &status_block)
                        else {
                            break;
                        };

                        rate_limit_wait(&shared, channel_id).await;
                        match channel_id
                            .edit_message(
                                &http,
                                msg_id,
                                serenity::EditMessage::new().content(&plan.frozen_chunk),
                            )
                            .await
                        {
                            Ok(_) => {
                                rate_limit_wait(&shared, channel_id).await;
                                match channel_id
                                    .send_message(
                                        &http,
                                        serenity::CreateMessage::new().content(&status_block),
                                    )
                                    .await
                                {
                                    Ok(message) => {
                                        placeholder_msg_id = Some(message.id);
                                        response_sent_offset += plan.split_at;
                                        last_edit_text = status_block;
                                    }
                                    Err(error) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ tmux rollover placeholder send failed in channel {}: {}",
                                            channel_id.get(),
                                            error
                                        );
                                        rate_limit_wait(&shared, channel_id).await;
                                        let _ = channel_id
                                            .edit_message(
                                                &http,
                                                msg_id,
                                                serenity::EditMessage::new()
                                                    .content(&plan.display_snapshot),
                                            )
                                            .await;
                                        last_edit_text = plan.display_snapshot;
                                        break;
                                    }
                                }
                            }
                            Err(error) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::warn!(
                                    "  [{ts}] ⚠ tmux rollover freeze failed for msg {} in channel {}: {}",
                                    msg_id.get(),
                                    channel_id.get(),
                                    error
                                );
                                break;
                            }
                        }
                    }

                    let status_block = super::formatting::build_placeholder_status_block(
                        indicator,
                        tool_state.prev_tool_status.as_deref(),
                        tool_state.current_tool_line.as_deref(),
                        &full_response,
                        narrate_progress,
                    );
                    let current_portion = full_response.get(response_sent_offset..).unwrap_or("");
                    let display_text =
                        build_streaming_placeholder_text(current_portion, &status_block);

                    if display_text != last_edit_text {
                        match placeholder_msg_id {
                            Some(msg_id) => {
                                // Edit existing placeholder
                                rate_limit_wait(&shared, channel_id).await;
                                let _ = channel_id
                                    .edit_message(
                                        &http,
                                        msg_id,
                                        serenity::EditMessage::new().content(&display_text),
                                    )
                                    .await;
                            }
                            None => {
                                // Create new placeholder
                                if let Ok(msg) = channel_id.say(&http, &display_text).await {
                                    placeholder_msg_id = Some(msg.id);
                                }
                            }
                        }
                        last_edit_text = display_text;
                    }
                }
            }
        }

        // If paused was set while we were reading (even if already unpaused), discard partial data.
        // Also check epoch: if it changed, a Discord turn claimed this data even if paused is now false.
        if was_paused
            || paused.load(Ordering::Relaxed)
            || pause_epoch.load(Ordering::Relaxed) != epoch_snapshot
        {
            // Clean up placeholder if we created one
            if let Some(msg_id) = placeholder_msg_id {
                let _ = channel_id.delete_message(&http, msg_id).await;
            }
            continue;
        }

        // Handle prompt-too-long: kill session so next message creates a fresh one
        if is_prompt_too_long {
            clear_provider_overload_retry_state(channel_id);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Prompt too long detected in watcher for {tmux_session_name}, killing session"
            );
            prompt_too_long_killed = true;

            let sess = tmux_session_name.clone();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        "prompt_too_long",
                        Some("watcher cleanup: prompt too long"),
                        None,
                    );
                    record_tmux_exit_reason(&sess, "watcher cleanup: prompt too long");
                    crate::services::platform::tmux::kill_session(&sess);
                }),
            )
            .await;

            let notice = "⚠️ 컨텍스트 한도 초과로 세션을 초기화했습니다. 다음 메시지부터 새 세션으로 처리됩니다.";
            match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    let _ = channel_id
                        .edit_message(&http, msg_id, serenity::EditMessage::new().content(notice))
                        .await;
                }
                None => {
                    let _ = channel_id.say(&http, notice).await;
                }
            }
            // Don't break — let the watcher exit naturally when session-alive check fails
            continue;
        }

        // Handle auth error: kill session and notify user to re-authenticate
        if is_auth_error {
            clear_provider_overload_retry_state(channel_id);
            let inflight_state =
                super::inflight::load_inflight_state(&watcher_provider, channel_id.get());
            let fallback_session_id = inflight_state
                .as_ref()
                .and_then(|state| state.session_id.as_deref());
            let dispatch_id =
                resolve_watcher_dispatch_id(&shared, channel_id, inflight_state.as_ref()).await;
            let auth_detail = auth_error_message
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("authentication expired");
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Auth error detected in watcher for {tmux_session_name}: {}",
                truncate_str(auth_detail, 300)
            );
            prompt_too_long_killed = true; // reuse flag to suppress duplicate "session ended" message

            clear_provider_session_for_retry(
                &shared,
                channel_id,
                &tmux_session_name,
                fallback_session_id,
            )
            .await;

            let sess = tmux_session_name.clone();
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        "auth_error",
                        Some("watcher cleanup: authentication failed"),
                        None,
                    );
                    record_tmux_exit_reason(&sess, "watcher cleanup: authentication failed");
                    crate::services::platform::tmux::kill_session(&sess);
                }),
            )
            .await;

            let notice = format!(
                "⚠️ 인증이 만료되어 현재 dispatch를 실패 처리했습니다. 세션을 종료합니다.\n관리자가 CLI에서 재인증(`/login`)을 완료한 후 다시 디스패치해주세요.\n\n사유: {}",
                truncate_str(auth_detail, 300)
            );
            match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    let _ = channel_id
                        .edit_message(&http, msg_id, serenity::EditMessage::new().content(&notice))
                        .await;
                }
                None => {
                    let _ = channel_id.say(&http, &notice).await;
                }
            }
            if let Some(state) = inflight_state.as_ref() {
                let user_msg_id = serenity::MessageId::new(state.user_msg_id);
                super::formatting::remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;
                super::formatting::add_reaction_raw(&http, channel_id, user_msg_id, '⚠').await;
            }
            super::inflight::clear_inflight_state(&watcher_provider, channel_id.get());
            let failure_text = format!(
                "authentication expired; re-authentication required: {}",
                truncate_str(auth_detail, 300)
            );
            super::turn_bridge::fail_dispatch_with_retry(
                shared.api_port,
                dispatch_id.as_deref(),
                &failure_text,
            )
            .await;
            continue;
        }

        if is_provider_overloaded {
            let overload_message = provider_overload_message
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or("provider overload detected");
            let inflight_state =
                super::inflight::load_inflight_state(&watcher_provider, channel_id.get());
            let retry_text = inflight_state
                .as_ref()
                .map(|state| state.user_text.clone())
                .filter(|text| !text.trim().is_empty());
            let fallback_session_id = inflight_state
                .as_ref()
                .and_then(|state| state.session_id.as_deref());
            let dispatch_id =
                resolve_watcher_dispatch_id(&shared, channel_id, inflight_state.as_ref()).await;

            let decision = retry_text
                .as_deref()
                .map(|text| record_provider_overload_retry(channel_id, text))
                .unwrap_or(ProviderOverloadDecision::Exhausted);
            let retry_notice = match &decision {
                ProviderOverloadDecision::Retry { attempt, delay, .. } => format!(
                    "⚠️ 모델 capacity 상태를 감지해 세션을 정리했습니다. {}분 후 자동 재시도합니다. ({}/{})",
                    delay.as_secs() / 60,
                    attempt,
                    PROVIDER_OVERLOAD_MAX_RETRIES
                ),
                ProviderOverloadDecision::Exhausted => format!(
                    "⚠️ 모델 capacity 상태가 계속되어 자동 재시도를 중단했습니다. 잠시 후 다시 시도해 주세요.\n\n사유: {}",
                    truncate_str(overload_message, 300)
                ),
            };

            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Provider overload detected in watcher for {}: {}",
                tmux_session_name,
                overload_message
            );
            prompt_too_long_killed = true;

            clear_provider_session_for_retry(
                &shared,
                channel_id,
                &tmux_session_name,
                fallback_session_id,
            )
            .await;

            let sess = tmux_session_name.clone();
            let termination_reason = match &decision {
                ProviderOverloadDecision::Retry { .. } => "provider_overload_retry",
                ProviderOverloadDecision::Exhausted => "provider_overload_exhausted",
            };
            let termination_detail = format!("watcher cleanup: {overload_message}");
            let _ = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                tokio::task::spawn_blocking(move || {
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        termination_reason,
                        Some(&termination_detail),
                        None,
                    );
                    record_tmux_exit_reason(&sess, &termination_detail);
                    crate::services::platform::tmux::kill_session(&sess);
                }),
            )
            .await;

            match placeholder_msg_id {
                Some(msg_id) => {
                    rate_limit_wait(&shared, channel_id).await;
                    let _ = channel_id
                        .edit_message(
                            &http,
                            msg_id,
                            serenity::EditMessage::new().content(&retry_notice),
                        )
                        .await;
                }
                None => {
                    let _ = channel_id.say(&http, &retry_notice).await;
                }
            }

            if let Some(state) = inflight_state.as_ref() {
                let user_msg_id = serenity::MessageId::new(state.user_msg_id);
                super::formatting::remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;
                if matches!(&decision, ProviderOverloadDecision::Exhausted) {
                    super::formatting::add_reaction_raw(&http, channel_id, user_msg_id, '⚠').await;
                }
            }
            super::inflight::clear_inflight_state(&watcher_provider, channel_id.get());

            match decision {
                ProviderOverloadDecision::Retry {
                    attempt,
                    delay,
                    fingerprint,
                } => {
                    if let Some(retry_text) = retry_text {
                        if let Some(state) = inflight_state.as_ref() {
                            schedule_provider_overload_retry(
                                shared.clone(),
                                http.clone(),
                                watcher_provider.clone(),
                                channel_id,
                                serenity::MessageId::new(state.user_msg_id),
                                retry_text,
                                attempt,
                                delay,
                                fingerprint,
                            );
                        } else {
                            clear_provider_overload_retry_state(channel_id);
                        }
                    } else {
                        clear_provider_overload_retry_state(channel_id);
                    }
                }
                ProviderOverloadDecision::Exhausted => {
                    let failure_text = format!(
                        "provider overloaded after {} auto-retries: {}",
                        PROVIDER_OVERLOAD_MAX_RETRIES,
                        truncate_str(overload_message, 300)
                    );
                    super::turn_bridge::fail_dispatch_with_retry(
                        shared.api_port,
                        dispatch_id.as_deref(),
                        &failure_text,
                    )
                    .await;
                }
            }
            continue;
        }

        // Final guard: re-check epoch and turn_delivered right before relay.
        // Closes the race window where a Discord turn starts between the epoch check
        // above (line 277) and this relay — the turn_bridge may have already delivered
        // the same response to its own placeholder.
        if paused.load(Ordering::Relaxed)
            || pause_epoch.load(Ordering::Relaxed) != epoch_snapshot
            || turn_delivered.load(Ordering::Relaxed)
        {
            if let Some(msg_id) = placeholder_msg_id {
                let _ = channel_id.delete_message(&http, msg_id).await;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] 👁 Late epoch/delivered guard: suppressed duplicate relay for {}",
                tmux_session_name
            );
            continue;
        }

        // Duplicate-relay guard: if we already relayed from this same data range, suppress.
        if let Some(prev_offset) = last_relayed_offset {
            if data_start_offset <= prev_offset {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] 👁 Duplicate relay guard: suppressed re-relay for {} (data_start={}, last_relayed={})",
                    tmux_session_name,
                    data_start_offset,
                    prev_offset
                );
                if let Some(msg_id) = placeholder_msg_id {
                    let _ = channel_id.delete_message(&http, msg_id).await;
                }
                continue;
            }
        }

        // Detect stale session resume failure in watcher output
        let is_stale_resume = stale_resume_detected;
        if is_stale_resume {
            clear_provider_overload_retry_state(channel_id);
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Watcher detected stale session resume failure (channel {}), clearing session_id",
                channel_id
            );
            let stale_sid = {
                let mut data = shared.core.lock().await;
                let old = data
                    .sessions
                    .get(&channel_id)
                    .and_then(|s| s.session_id.clone());
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.clear_provider_session();
                }
                old
            };
            // Clear DB session_id
            {
                let hostname = crate::services::platform::hostname_short();
                let session_key = format!("{}:{}", hostname, tmux_session_name);
                super::adk_session::clear_provider_session_id(&session_key, shared.api_port).await;
            }
            if let Some(ref sid) = stale_sid {
                let _ = super::internal_api::clear_stale_session_id(sid).await;
            }
            crate::services::termination_audit::record_termination_for_tmux(
                &tmux_session_name,
                None,
                "tmux_watcher",
                "stale_resume_retry",
                Some("stale session resume detected — forcing fresh session before auto-retry"),
                None,
            );
            record_tmux_exit_reason(
                &tmux_session_name,
                "stale session resume detected — forcing fresh session before auto-retry",
            );
            crate::services::platform::tmux::kill_session(&tmux_session_name);
            // Replace placeholder with recovery notice (don't delete — avoids visual gap)
            if let Some(msg_id) = placeholder_msg_id {
                let _ = channel_id
                    .edit_message(
                        &http,
                        msg_id,
                        serenity::EditMessage::new()
                            .content("↻ 세션 복구 중... 잠시 후 자동으로 이어갑니다."),
                    )
                    .await;
            }
            // Auto-retry: persist Discord history for LLM injection, then queue the
            // original user message as an internal follow-up instead of self-routing
            // through /api/send announce.
            if let Some(state) =
                super::inflight::load_inflight_state(&watcher_provider, channel_id.get())
            {
                super::turn_bridge::auto_retry_with_history(
                    &http,
                    &shared,
                    &watcher_provider,
                    channel_id,
                    serenity::MessageId::new(state.user_msg_id),
                    &state.user_text,
                )
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ↻ Watcher auto-retry queued for channel {}",
                    channel_id
                );
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ Watcher auto-retry skipped: inflight state missing for channel {}",
                    channel_id
                );
            }
            // Skip normal response relay
            full_response = String::new();
        }

        let has_assistant_response = !full_response.trim().is_empty();
        let current_response = full_response.get(response_sent_offset..).unwrap_or("");
        let has_current_response = !current_response.trim().is_empty();

        // Send the terminal response to Discord
        // #225 P1-2: Track relay success across branches
        let relay_ok = if has_assistant_response {
            let formatted = super::formatting::format_for_discord_with_provider(
                current_response,
                &watcher_provider,
            );
            let prefixed = formatted.to_string();
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 👁 Relaying terminal response to Discord ({} chars, offset {})",
                prefixed.len(),
                data_start_offset
            );
            // #225 P1-2: Track relay success to gate turn_result_relayed
            let mut relay_ok = true;
            match placeholder_msg_id {
                Some(msg_id) => {
                    if has_current_response {
                        if let Err(e) =
                            replace_long_message_raw(&http, channel_id, msg_id, &prefixed, &shared)
                                .await
                        {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::info!("  [{ts}] 👁 Failed to relay: {e}");
                            relay_ok = false;
                        }
                    } else {
                        let _ = channel_id.delete_message(&http, msg_id).await;
                    }
                }
                None => {
                    if has_current_response
                        && let Err(e) =
                            send_long_message_raw(&http, channel_id, &prefixed, &shared).await
                    {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!("  [{ts}] 👁 Failed to relay: {e}");
                        relay_ok = false;
                    }
                }
            }
            // Record the offset range we just relayed to prevent duplicate relay.
            last_relayed_offset = Some(data_start_offset);
            if relay_ok {
                clear_provider_overload_retry_state(channel_id);
            }
            relay_ok
        } else {
            if let Some(msg_id) = placeholder_msg_id {
                // No response text but placeholder exists — clean up
                let _ = channel_id.delete_message(&http, msg_id).await;
            }
            false
        };

        let provider_kind = watcher_provider.clone();
        let inflight_state = super::inflight::load_inflight_state(&provider_kind, channel_id.get());
        let watcher_session_id = state.last_session_id.clone();
        let result_usage = stream_line_state_token_usage(&state);
        if inflight_state.is_none() {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: inflight state missing for channel {} — using DB dispatch fallback",
                channel_id.get()
            );
        }

        // Mark user message as completed: ⏳ → ✅ when inflight metadata is available.
        if let Some(state) = inflight_state.as_ref() {
            let user_msg_id = serenity::MessageId::new(state.user_msg_id);
            super::formatting::remove_reaction_raw(&http, channel_id, user_msg_id, '⏳').await;
            super::formatting::add_reaction_raw(&http, channel_id, user_msg_id, '✅').await;

            if has_assistant_response && let Some(db) = shared.db.as_ref() {
                let turn_id = format!("discord:{}:{}", channel_id.get(), state.user_msg_id);
                let channel_id_text = channel_id.get().to_string();
                let resolved_did = inflight_state
                    .as_ref()
                    .and_then(|s| s.dispatch_id.clone())
                    .or_else(|| super::adk_session::parse_dispatch_id(&state.user_text))
                    .or(super::adk_session::lookup_pending_dispatch_for_thread(
                        shared.api_port,
                        channel_id.get(),
                    )
                    .await)
                    .or_else(|| {
                        resolve_dispatched_thread_dispatch_from_db(
                            shared.db.as_ref(),
                            channel_id.get(),
                        )
                    });
                if let Err(e) = crate::db::session_transcripts::persist_turn(
                    db,
                    crate::db::session_transcripts::PersistSessionTranscript {
                        turn_id: &turn_id,
                        session_key: state.session_key.as_deref(),
                        channel_id: Some(channel_id_text.as_str()),
                        agent_id: resolve_role_binding(channel_id, state.channel_name.as_deref())
                            .as_ref()
                            .map(|binding| binding.role_id.as_str()),
                        provider: Some(provider_kind.as_str()),
                        dispatch_id: resolved_did.as_deref().or(state.dispatch_id.as_deref()),
                        user_message: &state.user_text,
                        assistant_message: &full_response,
                        events: &tool_state.transcript_events,
                        duration_ms: inflight_duration_ms(Some(state.started_at.as_str())),
                    },
                ) {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!("  [{ts}] ⚠ watcher: failed to persist session transcript: {e}");
                }

                super::turn_bridge::persist_turn_analytics_row(
                    db,
                    &provider_kind,
                    channel_id,
                    user_msg_id,
                    resolve_role_binding(channel_id, state.channel_name.as_deref()).as_ref(),
                    resolved_did.as_deref().or(state.dispatch_id.as_deref()),
                    state.session_key.as_deref(),
                    watcher_session_id
                        .as_deref()
                        .or(state.session_id.as_deref()),
                    state,
                    result_usage.unwrap_or_default(),
                    inflight_duration_ms(Some(state.started_at.as_str())).unwrap_or(0),
                );
            }
        }

        let resolved_did = inflight_state
            .as_ref()
            .and_then(|state| state.dispatch_id.clone())
            .or_else(|| {
                inflight_state
                    .as_ref()
                    .and_then(|state| super::adk_session::parse_dispatch_id(&state.user_text))
            })
            .or(super::adk_session::lookup_pending_dispatch_for_thread(
                shared.api_port,
                channel_id.get(),
            )
            .await)
            .or_else(|| {
                resolve_dispatched_thread_dispatch_from_db(shared.db.as_ref(), channel_id.get())
            });

        if resolved_did.is_none() && has_assistant_response {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ watcher: no dispatch id resolved for channel {} after terminal success",
                channel_id.get()
            );
        }
        let current_worktree_path = {
            let mut data = shared.core.lock().await;
            data.sessions
                .get_mut(&channel_id)
                .and_then(|session| session.validated_path(channel_id.get()))
        };

        let dispatch_ok = if let Some(did) = resolved_did.as_deref() {
            let dispatch_type = shared.db.as_ref().and_then(|db| {
                db.separate_conn().ok().and_then(|conn| {
                    conn.query_row(
                        "SELECT dispatch_type FROM task_dispatches WHERE id = ?1",
                        [did],
                        |row| row.get::<_, String>(0),
                    )
                    .ok()
                })
            });

            match dispatch_type.as_deref() {
                Some("implementation") | Some("rework") => {
                    if !has_assistant_response {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ watcher: refusing to complete work dispatch {did} without assistant response"
                        );
                        false
                    } else if let (Some(db), Some(engine)) = (&shared.db, &shared.engine) {
                        let mut work_completion_context =
                            super::turn_bridge::build_work_dispatch_completion_result(
                                shared.db.as_ref(),
                                did,
                                "watcher_completed",
                                false,
                                current_worktree_path.as_deref(),
                                Some(&full_response),
                            );
                        if let Some(obj) = work_completion_context.as_object_mut() {
                            obj.insert(
                                "agent_response_present".to_string(),
                                serde_json::Value::Bool(true),
                            );
                        }
                        match crate::dispatch::finalize_dispatch(
                            db,
                            engine,
                            did,
                            "watcher_completed",
                            Some(&work_completion_context),
                        ) {
                            Ok(_) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] ✓ watcher: completed dispatch {did} via finalize_dispatch"
                                );
                                crate::server::routes::dispatches::queue_dispatch_followup(db, did);
                                true
                            }
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::warn!(
                                    "  [{ts}] ⚠ watcher: finalize_dispatch failed for {did}: {e}"
                                );
                                let mut fallback_result =
                                    super::turn_bridge::build_work_dispatch_completion_result(
                                        shared.db.as_ref(),
                                        did,
                                        "watcher_db_fallback",
                                        true,
                                        current_worktree_path.as_deref(),
                                        Some(&full_response),
                                    );
                                if let Some(obj) = fallback_result.as_object_mut() {
                                    obj.insert(
                                        "agent_response_present".to_string(),
                                        serde_json::Value::Bool(true),
                                    );
                                }
                                super::turn_bridge::runtime_db_fallback_complete_with_result(
                                    did,
                                    &fallback_result,
                                )
                            }
                        }
                    } else {
                        let mut fallback_result =
                            super::turn_bridge::build_work_dispatch_completion_result(
                                shared.db.as_ref(),
                                did,
                                "watcher_db_fallback",
                                true,
                                current_worktree_path.as_deref(),
                                Some(&full_response),
                            );
                        if let Some(obj) = fallback_result.as_object_mut() {
                            obj.insert(
                                "agent_response_present".to_string(),
                                serde_json::Value::Bool(true),
                            );
                        }
                        super::turn_bridge::runtime_db_fallback_complete_with_result(
                            did,
                            &fallback_result,
                        )
                    }
                }
                Some(_) => {
                    // Non-work dispatches — leave for their own completion flow
                    true
                }
                None => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ watcher: cannot determine dispatch type for {did} — preserving state"
                    );
                    false
                }
            }
        } else {
            true
        };

        // #225 P1-2: Only mark relayed + clear inflight if Discord relay succeeded.
        // If relay failed, preserve retry/handoff path for next startup.
        if relay_ok {
            if has_assistant_response && let Some(state) = inflight_state.as_ref() {
                let mut data = shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    if !session.cleared {
                        session.history.push(crate::ui::ai_screen::HistoryItem {
                            item_type: crate::ui::ai_screen::HistoryType::User,
                            content: state.user_text.clone(),
                        });
                        session.history.push(crate::ui::ai_screen::HistoryItem {
                            item_type: crate::ui::ai_screen::HistoryType::Assistant,
                            content: full_response.clone(),
                        });
                    }
                }
                drop(data);
            }
            turn_result_relayed = true;
            if dispatch_ok {
                super::inflight::clear_inflight_state(&provider_kind, channel_id.get());
            }
            let mailbox = shared.mailbox(channel_id);
            let has_active_turn = mailbox.has_active_turn().await;
            let should_kickoff_queue = if has_active_turn {
                false
            } else {
                mailbox
                    .has_pending_soft_queue(super::queue_persistence_context(
                        &shared,
                        &provider_kind,
                        channel_id,
                    ))
                    .await
                    .has_pending
            };
            if dispatch_ok && should_kickoff_queue {
                super::schedule_deferred_idle_queue_kickoff(
                    shared.clone(),
                    provider_kind.clone(),
                    channel_id,
                    "watcher completed with queued backlog",
                );
            }
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ watcher: relay failed — preserving inflight for retry");
        }

        // Update session tokens from result event and auto-compact if threshold exceeded
        if let Some(tokens) = result_usage.map(|usage| usage.total_input_tokens()) {
            let provider = shared.settings.read().await.provider.clone();
            let session_key =
                super::adk_session::build_adk_session_key(&shared, channel_id, &provider).await;
            let channel_name = {
                let data = shared.core.lock().await;
                data.sessions
                    .get(&channel_id)
                    .and_then(|s| s.channel_name.clone())
            };
            let thread_channel_id = channel_name
                .as_deref()
                .and_then(super::adk_session::parse_thread_channel_id_from_name);
            let agent_id = resolve_role_binding(channel_id, channel_name.as_deref())
                .map(|binding| binding.role_id);
            super::adk_session::post_adk_session_status(
                session_key.as_deref(),
                channel_name.as_deref(),
                None,
                "idle",
                &provider,
                None,
                Some(tokens),
                None,
                None,
                thread_channel_id,
                agent_id.as_deref(),
                shared.api_port,
            )
            .await;

            let ctx_cfg = super::adk_session::fetch_context_thresholds(shared.api_port).await;
            let pct = (tokens * 100) / ctx_cfg.context_window.max(1);
            // #227: Re-enabled with 5-min cooldown (matches turn_bridge path).
            // Without cooldown, the compact turn's own result could re-trigger compact.
            let compact_cooldown_ok = shared.db.as_ref().map_or(true, |db| {
                db.lock().ok().map_or(true, |conn| {
                    let cooldown_key = format!("auto_compact_cooldown:{}", channel_id.get());
                    let last: Option<String> = conn
                        .query_row(
                            "SELECT value FROM kv_meta WHERE key = ?1",
                            [&cooldown_key],
                            |row| row.get(0),
                        )
                        .ok();
                    last.and_then(|v| v.parse::<i64>().ok()).map_or(true, |ts| {
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs() as i64;
                        now - ts > 300 // 5 min cooldown
                    })
                })
            });
            // DISABLED — token counting still unreliable
            if false && pct >= ctx_cfg.compact_pct && !is_prompt_too_long && compact_cooldown_ok {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚡ [watcher] Auto-compact: {} at {pct}% ({tokens} tokens)",
                    tmux_session_name
                );
                let name = tmux_session_name.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    crate::services::platform::tmux::send_keys(&name, &["/compact", "Enter"])
                })
                .await;
                // Set cooldown timestamp
                if let Some(ref db) = shared.db {
                    if let Ok(conn) = db.lock() {
                        let cooldown_key = format!("auto_compact_cooldown:{}", channel_id.get());
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        conn.execute(
                            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                            rusqlite::params![cooldown_key, now.to_string()],
                        )
                        .ok();
                    }
                }
                // Notify: auto-compact triggered
                if let Some(ref db) = shared.db {
                    if let Ok(conn) = db.lock() {
                        let _ = conn.execute(
                            "INSERT INTO message_outbox (target, content, bot, source) VALUES (?1, ?2, 'notify', 'system')",
                            rusqlite::params![
                                format!("channel:{}", channel_id.get()),
                                format!("🗜️ 자동 컨텍스트 압축 (사용률: {pct}%)"),
                            ],
                        );
                    }
                }
            }
        }
    }

    // Cleanup: only remove from DashMap if we weren't cancelled/replaced.
    // #243: When a watcher is cancelled (replaced by a new watcher or shutdown),
    // the replacement already occupies the slot — removing would delete the new entry.
    if !cancel.load(Ordering::Relaxed) {
        shared.tmux_watchers.remove(&channel_id);
    }

    let api_port = shared.api_port;
    let provider = shared.settings.read().await.provider.clone();
    let session_key =
        super::adk_session::build_adk_session_key(&shared, channel_id, &provider).await;
    let channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.clone())
    };
    let dispatch_protection = super::tmux_lifecycle::resolve_dispatch_tmux_protection(
        shared.db.as_ref(),
        &shared.token_hash,
        &provider,
        &tmux_session_name,
        channel_name.as_deref(),
    );
    let cleanup_plan = dead_session_cleanup_plan(dispatch_protection.is_some());

    if let Some(protection) = dispatch_protection {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ♻ tmux watcher: preserving dispatch session {} — {}",
            tmux_session_name,
            protection.log_reason()
        );
    }

    if !cleanup_plan.preserve_tmux_session {
        // Kill dead tmux session to prevent accumulation (especially for thread sessions
        // which are created per-dispatch and would otherwise linger for 24h).
        // #145: skip kill for unified-thread sessions with active auto-queue runs.
        {
            let sess = tmux_session_name.clone();
            let _ = tokio::task::spawn_blocking(move || {
                if tmux_session_exists(&sess) && !tmux_session_has_live_pane(&sess) {
                    // Check if this is a unified-thread session before killing
                    if let Some((_, ch_name)) =
                        crate::services::provider::parse_provider_and_channel_from_tmux_name(&sess)
                    {
                        if crate::dispatch::is_unified_thread_channel_name_active(&ch_name) {
                            return;
                        }
                    }
                    crate::services::termination_audit::record_termination_for_tmux(
                        &sess,
                        None,
                        "tmux_watcher",
                        "dead_after_turn",
                        Some("watcher cleanup: dead session after turn"),
                        None,
                    );
                    record_tmux_exit_reason(&sess, "watcher cleanup: dead session after turn");
                    crate::services::platform::tmux::kill_session(&sess);
                }
            })
            .await;
        }
    }

    if cleanup_plan.report_idle_status {
        // Report idle status to DB so the dashboard doesn't show stale "working" state.
        // Always report idle when the watcher exits, even if dispatch protection
        // keeps the dead tmux session around for the active-dispatch safety path.
        let thread_channel_id = channel_name
            .as_deref()
            .and_then(super::adk_session::parse_thread_channel_id_from_name);
        let agent_id = resolve_role_binding(channel_id, channel_name.as_deref())
            .map(|binding| binding.role_id);
        super::adk_session::post_adk_session_status(
            session_key.as_deref(),
            channel_name.as_deref(),
            None, // model
            "idle",
            &provider,
            None, // session_info
            None, // tokens
            None, // cwd
            None, // dispatch_id
            thread_channel_id,
            agent_id.as_deref(),
            api_port,
        )
        .await;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!("  [{ts}] 👁 tmux watcher stopped for #{tmux_session_name}");
}

/// Tracks tool/thinking status during watcher output processing.
pub(super) struct WatcherToolState {
    /// Current tool status line (e.g. "⚙ Bash: `ls`")
    pub current_tool_line: Option<String>,
    /// Previous distinct tool/thinking status for 2-line trail rendering.
    pub prev_tool_status: Option<String>,
    /// Accumulated thinking text from streaming deltas
    pub thinking_buffer: String,
    /// Whether we are currently inside a thinking block
    pub in_thinking: bool,
    /// Whether any tool_use block has been seen in this turn
    pub any_tool_used: bool,
    /// Whether a text block was streamed after the last tool_use
    pub has_post_tool_text: bool,
    /// Structured transcript events collected during watcher replay
    pub transcript_events: Vec<SessionTranscriptEvent>,
}

impl WatcherToolState {
    pub fn new() -> Self {
        Self {
            current_tool_line: None,
            prev_tool_status: None,
            thinking_buffer: String::new(),
            in_thinking: false,
            any_tool_used: false,
            has_post_tool_text: false,
            transcript_events: Vec::new(),
        }
    }

    fn set_current_tool_line(&mut self, next_tool_line: Option<String>) {
        let current_tool_line = self.current_tool_line.clone();
        super::formatting::preserve_previous_tool_status(
            &mut self.prev_tool_status,
            current_tool_line.as_deref(),
            next_tool_line.as_deref(),
        );
        self.current_tool_line = next_tool_line;
    }

    fn clear_current_tool_line(&mut self) {
        let current_tool_line = self.current_tool_line.clone();
        super::formatting::preserve_previous_tool_status(
            &mut self.prev_tool_status,
            current_tool_line.as_deref(),
            None,
        );
        self.current_tool_line = None;
    }
}

/// Process buffered lines for the tmux watcher.
/// Extracts text content, tracks tool status, and detects result events.
/// Returns true if a "result" event was found.
pub(super) fn process_watcher_lines(
    buffer: &mut String,
    state: &mut StreamLineState,
    full_response: &mut String,
    tool_state: &mut WatcherToolState,
) -> WatcherLineOutcome {
    let mut outcome = WatcherLineOutcome::default();

    while let Some(pos) = buffer.find('\n') {
        let line: String = buffer.drain(..=pos).collect();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse the JSON line
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let event_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match event_type {
                "assistant" => {
                    if let Some(message) = val.get("message") {
                        if let Some(model) = message.get("model").and_then(|value| value.as_str()) {
                            state.last_model = Some(model.to_string());
                        }
                        if let Some(usage) = message.get("usage") {
                            state.accum_input_tokens = state.accum_input_tokens.saturating_add(
                                usage
                                    .get("input_tokens")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0),
                            );
                            state.accum_cache_read_tokens =
                                state.accum_cache_read_tokens.saturating_add(
                                    usage
                                        .get("cache_read_input_tokens")
                                        .and_then(|value| value.as_u64())
                                        .unwrap_or(0),
                                );
                            state.accum_cache_create_tokens =
                                state.accum_cache_create_tokens.saturating_add(
                                    usage
                                        .get("cache_creation_input_tokens")
                                        .and_then(|value| value.as_u64())
                                        .unwrap_or(0),
                                );
                            state.accum_output_tokens = state.accum_output_tokens.saturating_add(
                                usage
                                    .get("output_tokens")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0),
                            );
                        }
                        // Text content from assistant message
                        if let Some(content) = message.get("content") {
                            if let Some(arr) = content.as_array() {
                                for block in arr {
                                    let block_type = block.get("type").and_then(|t| t.as_str());
                                    if block_type == Some("text") {
                                        if let Some(text) =
                                            block.get("text").and_then(|t| t.as_str())
                                        {
                                            full_response.push_str(text);
                                            push_transcript_event(
                                                &mut tool_state.transcript_events,
                                                SessionTranscriptEvent {
                                                    kind: SessionTranscriptEventKind::Assistant,
                                                    tool_name: None,
                                                    summary: None,
                                                    content: text.to_string(),
                                                    status: Some("success".to_string()),
                                                    is_error: false,
                                                },
                                            );
                                            if tool_state.any_tool_used {
                                                tool_state.has_post_tool_text = true;
                                            }
                                            tool_state.clear_current_tool_line();
                                        }
                                    } else if block_type == Some("tool_use") {
                                        tool_state.any_tool_used = true;
                                        tool_state.has_post_tool_text = false;
                                        let name = block
                                            .get("name")
                                            .and_then(|n| n.as_str())
                                            .unwrap_or("Tool");
                                        let input_str = block
                                            .get("input")
                                            .map(|i| i.to_string())
                                            .unwrap_or_default();
                                        let summary = format_tool_input(name, &input_str);
                                        let display = if summary.is_empty() {
                                            format!("⚙ {}", name)
                                        } else {
                                            let truncated: String =
                                                summary.chars().take(500).collect();
                                            format!("⚙ {}: {}", name, truncated)
                                        };
                                        tool_state.set_current_tool_line(Some(display));
                                        push_transcript_event(
                                            &mut tool_state.transcript_events,
                                            SessionTranscriptEvent {
                                                kind: SessionTranscriptEventKind::ToolUse,
                                                tool_name: Some(name.to_string()),
                                                summary: (!summary.is_empty()).then_some(summary),
                                                content: input_str,
                                                status: Some("running".to_string()),
                                                is_error: false,
                                            },
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                "content_block_start" => {
                    if let Some(cb) = val.get("content_block") {
                        let cb_type = cb.get("type").and_then(|t| t.as_str());
                        if cb_type == Some("thinking") {
                            tool_state.in_thinking = true;
                            tool_state.thinking_buffer.clear();
                            tool_state.set_current_tool_line(Some("💭 Thinking...".to_string()));
                        } else if cb_type == Some("tool_use") {
                            tool_state.any_tool_used = true;
                            tool_state.has_post_tool_text = false;
                            let name = cb.get("name").and_then(|n| n.as_str()).unwrap_or("Tool");
                            tool_state.set_current_tool_line(Some(format!("⚙ {}", name)));
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = val.get("delta") {
                        if let Some(thinking) = delta.get("thinking").and_then(|t| t.as_str()) {
                            // Accumulate thinking text and update display
                            tool_state.thinking_buffer.push_str(thinking);
                            let display = tool_state.thinking_buffer.trim().to_string();
                            if !display.is_empty() {
                                tool_state.set_current_tool_line(Some(format!("💭 {display}")));
                            }
                        } else if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            full_response.push_str(text);
                            if tool_state.any_tool_used {
                                tool_state.has_post_tool_text = true;
                            }
                            tool_state.clear_current_tool_line();
                        }
                    }
                }
                "content_block_stop" => {
                    if tool_state.in_thinking {
                        // Thinking block completed — show full text
                        tool_state.in_thinking = false;
                        let display = tool_state.thinking_buffer.trim().to_string();
                        if !display.is_empty() {
                            tool_state.set_current_tool_line(Some(format!("💭 {display}")));
                            push_transcript_event(
                                &mut tool_state.transcript_events,
                                SessionTranscriptEvent {
                                    kind: SessionTranscriptEventKind::Thinking,
                                    tool_name: None,
                                    summary: Some(truncate_str(&display, 120).to_string()),
                                    content: display,
                                    status: Some("info".to_string()),
                                    is_error: false,
                                },
                            );
                        }
                    } else if let Some(line) = tool_state.current_tool_line.clone() {
                        // Tool completed — mark with checkmark
                        if line.starts_with("⚙") {
                            tool_state.set_current_tool_line(Some(line.replacen("⚙", "✓", 1)));
                        }
                    }
                }
                "result" => {
                    outcome.stale_resume_detected = outcome.stale_resume_detected
                        || super::turn_bridge::result_event_has_stale_resume_error(&val);
                    if let Some(session_id) = val.get("session_id").and_then(|value| value.as_str())
                    {
                        state.last_session_id = Some(session_id.to_string());
                    }
                    let is_error = val
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let result_str = extract_result_error_text(&val);
                    push_transcript_event(
                        &mut tool_state.transcript_events,
                        SessionTranscriptEvent {
                            kind: if is_error {
                                SessionTranscriptEventKind::Error
                            } else {
                                SessionTranscriptEventKind::Result
                            },
                            tool_name: None,
                            summary: Some(if result_str.trim().is_empty() {
                                if is_error {
                                    "error".to_string()
                                } else {
                                    "completed".to_string()
                                }
                            } else {
                                truncate_str(&result_str, 120).to_string()
                            }),
                            content: result_str.clone(),
                            status: Some(if is_error { "error" } else { "success" }.to_string()),
                            is_error,
                        },
                    );

                    if is_error {
                        if is_prompt_too_long_message(&result_str) {
                            outcome.is_prompt_too_long = true;
                        }
                        if is_auth_error_message(&result_str) {
                            outcome.is_auth_error = true;
                            outcome.auth_error_message.get_or_insert(result_str.clone());
                        }
                        if let Some(message) = detect_provider_overload_message(&result_str) {
                            outcome.is_provider_overloaded = true;
                            outcome.provider_overload_message.get_or_insert(message);
                        }
                    }

                    // Use result text when streaming didn't capture the final response:
                    // 1. full_response is empty — no text was streamed at all
                    // 2. tools were used but no text was streamed after the last tool
                    //    (accumulated text is stale pre-tool narration)
                    if !outcome.is_prompt_too_long
                        && !outcome.is_auth_error
                        && !outcome.is_provider_overloaded
                        && !result_str.is_empty()
                    {
                        if full_response.is_empty()
                            || (tool_state.any_tool_used && !tool_state.has_post_tool_text)
                        {
                            full_response.clear();
                            full_response.push_str(&result_str);
                        }
                    }
                    if let Some(usage) = val.get("usage") {
                        state.accum_input_tokens = usage
                            .get("input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        state.accum_cache_read_tokens = usage
                            .get("cache_read_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        state.accum_cache_create_tokens = usage
                            .get("cache_creation_input_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        state.accum_output_tokens = usage
                            .get("output_tokens")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                    }

                    state.final_result = Some(String::new());
                    outcome.found_result = true;
                }
                "system" => {
                    if val.get("subtype").and_then(|s| s.as_str()) == Some("init")
                        && let Some(session_id) =
                            val.get("session_id").and_then(|value| value.as_str())
                    {
                        state.last_session_id = Some(session_id.to_string());
                    }
                    // Detect auto-compaction events from Claude Code
                    if let Some(msg) = val.get("message").and_then(|m| m.as_str()) {
                        let lower = msg.to_ascii_lowercase();
                        if lower.contains("compacted")
                            || lower.contains("auto-compact")
                            || lower.contains("conversation has been compressed")
                        {
                            outcome.auto_compacted = true;
                        }
                    }
                    if let Some(subtype) = val.get("subtype").and_then(|s| s.as_str()) {
                        if subtype == "compact" || subtype == "auto_compact" {
                            outcome.auto_compacted = true;
                        }
                    }
                }
                _ => {}
            }
        } else if is_auth_error_message(trimmed) {
            outcome.found_result = true;
            outcome.is_auth_error = true;
            outcome
                .auth_error_message
                .get_or_insert(trimmed.to_string());
            push_transcript_event(
                &mut tool_state.transcript_events,
                SessionTranscriptEvent {
                    kind: SessionTranscriptEventKind::Error,
                    tool_name: None,
                    summary: Some("authentication error".to_string()),
                    content: trimmed.to_string(),
                    status: Some("error".to_string()),
                    is_error: true,
                },
            );
            state.final_result = Some(String::new());
        } else if let Some(message) = detect_provider_overload_message(trimmed) {
            outcome.found_result = true;
            outcome.is_provider_overloaded = true;
            outcome.provider_overload_message.get_or_insert(message);
            push_transcript_event(
                &mut tool_state.transcript_events,
                SessionTranscriptEvent {
                    kind: SessionTranscriptEventKind::Error,
                    tool_name: None,
                    summary: Some("provider overload".to_string()),
                    content: trimmed.to_string(),
                    status: Some("error".to_string()),
                    is_error: true,
                },
            );
            state.final_result = Some(String::new());
        }
    }

    outcome
}

/// On startup, scan for surviving tmux sessions (AgentDesk-*) and restore watchers.
/// This handles the case where AgentDesk was restarted but tmux sessions are still alive.
pub(super) async fn restore_tmux_watchers(http: &Arc<serenity::Http>, shared: &Arc<SharedData>) {
    let settings_snapshot = { shared.settings.read().await.clone() };
    let provider = settings_snapshot.provider.clone();

    // List tmux sessions matching our naming convention
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(crate::services::platform::tmux::list_session_names),
    )
    .await
    {
        Ok(Ok(Ok(names))) => names,
        _ => return, // No tmux, timeout, or no sessions
    };

    let agent_sessions: Vec<&str> = output
        .iter()
        .map(|l| l.trim())
        .filter(|l| {
            parse_provider_and_channel_from_tmux_name(l)
                .map(|(session_provider, _)| session_provider == provider)
                .unwrap_or(false)
        })
        .collect();

    if agent_sessions.is_empty() {
        return;
    }

    // Build channel name → ChannelId map from Discord API (sessions map may be empty after restart)
    let mut name_to_channel: std::collections::HashMap<String, (ChannelId, String)> =
        std::collections::HashMap::new();

    // Try from in-memory sessions first
    {
        let data = shared.core.lock().await;
        for (&ch_id, session) in &data.sessions {
            if let Some(ref ch_name) = session.channel_name {
                let tmux_name = provider.build_tmux_session_name(ch_name);
                name_to_channel.insert(tmux_name, (ch_id, ch_name.clone()));
            }
        }
    }

    // If in-memory sessions don't cover all tmux sessions, fetch from Discord API
    let unresolved: Vec<&&str> = agent_sessions
        .iter()
        .filter(|s| !name_to_channel.contains_key(**s))
        .collect();

    if !unresolved.is_empty() {
        // Fetch guild channels via Discord API
        if let Ok(guilds) = http.get_guilds(None, None).await {
            for guild_info in &guilds {
                if let Ok(channels) = guild_info.id.channels(http).await {
                    for (ch_id, channel) in &channels {
                        let role_binding = resolve_role_binding(*ch_id, Some(&channel.name));
                        if !channel_supports_provider(
                            &provider,
                            Some(&channel.name),
                            false,
                            role_binding.as_ref(),
                        ) {
                            continue;
                        }
                        let tmux_name = provider.build_tmux_session_name(&channel.name);
                        name_to_channel
                            .entry(tmux_name)
                            .or_insert((*ch_id, channel.name.clone()));
                    }
                }
            }
        }

        // Fallback for thread sessions: guild.channels() doesn't return threads.
        // Extract thread_id from the channel name suffix (-t{id}) and use it
        // as the channel_id directly, since Discord thread IDs are channel IDs.
        let still_unresolved: Vec<&&str> = agent_sessions
            .iter()
            .filter(|s| !name_to_channel.contains_key(**s))
            .collect();
        for session_name in &still_unresolved {
            if let Some((_, ch_name)) = parse_provider_and_channel_from_tmux_name(session_name) {
                if let Some(pos) = ch_name.rfind("-t") {
                    let suffix = &ch_name[pos + 2..];
                    if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
                        if let Ok(thread_id) = suffix.parse::<u64>() {
                            let channel_id = ChannelId::new(thread_id);
                            name_to_channel
                                .entry(session_name.to_string())
                                .or_insert((channel_id, ch_name.clone()));
                        }
                    }
                }
            }
        }
    }

    // Collect sessions to restore
    struct PendingWatcher {
        channel_id: ChannelId,
        output_path: String,
        session_name: String,
        initial_offset: u64,
    }

    // Dead sessions that need DB cleanup (idle status report + tmux kill)
    struct DeadSessionCleanup {
        channel_id: u64,
        channel_name: String,
        session_name: String,
    }

    let mut pending: Vec<PendingWatcher> = Vec::new();
    let mut dead_cleanups: Vec<DeadSessionCleanup> = Vec::new();
    let mut owned_sessions: std::collections::HashMap<ChannelId, String> =
        std::collections::HashMap::new();

    for session_name in &agent_sessions {
        let Some((channel_id, channel_name)) = name_to_channel.get(*session_name) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — channel mapping not found",
                session_name
            );
            continue;
        };

        // #148: Do NOT register in owned_sessions yet — QUARANTINE check below may
        // skip this session. Registering early blocks new session creation for the channel.
        let is_dm = matches!(
            channel_id.to_channel(http.as_ref()).await,
            Ok(serenity::model::channel::Channel::Private(_))
        );
        // Resolve thread parent so validation uses the same semantics
        // as normal message routing (router.rs).
        let (allowlist_channel_id, provider_channel_name) =
            if let Some((pid, pname)) = super::resolve_thread_parent(http, *channel_id).await {
                (pid, pname.unwrap_or_else(|| channel_name.clone()))
            } else {
                (*channel_id, channel_name.clone())
            };
        if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
            &settings_snapshot,
            &provider,
            allowlist_channel_id,
            Some(&channel_name),
            Some(&provider_channel_name),
            is_dm,
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — {reason} for channel {}",
                session_name,
                channel_id
            );
            continue;
        }

        if let Some(started) = super::mailbox_snapshot(&shared, *channel_id)
            .await
            .recovery_started_at
        {
            if started.elapsed() < std::time::Duration::from_secs(60) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏳ watcher skip for {} — recovery in progress ({:.0}s ago)",
                    session_name,
                    started.elapsed().as_secs_f64()
                );
                continue;
            }
            // Stale recovery — remove marker and proceed with watcher
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ clearing stale recovery marker for {} ({:.0}s elapsed)",
                session_name,
                started.elapsed().as_secs_f64()
            );
            super::mailbox_clear_recovery_marker(&shared, *channel_id).await;
        }

        if shared.tmux_watchers.contains_key(channel_id) {
            continue;
        }

        let output_path = crate::services::tmux_common::session_temp_path(session_name, "jsonl");
        if std::fs::metadata(&output_path).is_err() {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — no output file",
                session_name
            );
            continue;
        }

        // Old-gen sessions: adopt instead of killing.
        // The tmux session and Claude CLI process are still alive from the
        // previous dcserver — just update the generation marker and re-attach
        // a watcher. Auto-retry handles stale Claude session IDs if needed.
        let gen_marker_path =
            crate::services::tmux_common::session_temp_path(session_name, "generation");
        let session_gen = std::fs::read_to_string(&gen_marker_path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let current_gen = super::runtime_store::load_generation();
        if session_gen < current_gen && current_gen > 0 {
            // Skip sessions belonging to other runtimes
            let current_owner_marker = current_tmux_owner_marker();
            if !session_belongs_to_current_runtime(session_name, &current_owner_marker) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — owned by other runtime",
                    session_name
                );
                continue;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Adopting old-gen session {} (gen {} → {})",
                session_name,
                session_gen,
                current_gen
            );
            // Update generation marker to current gen
            let _ = std::fs::write(&gen_marker_path, current_gen.to_string());
        }

        if !tmux_session_has_live_pane(session_name) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            if let Some(diag) = build_tmux_death_diagnostic(session_name, Some(&output_path)) {
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux pane dead ({diag})",
                    session_name
                );
            } else {
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux pane dead",
                    session_name
                );
            }
            // Schedule DB cleanup + tmux kill for this dead session
            dead_cleanups.push(DeadSessionCleanup {
                channel_id: channel_id.get(),
                channel_name: channel_name.clone(),
                session_name: session_name.to_string(),
            });
            continue;
        }

        // #148: Only register in owned_sessions after passing QUARANTINE + live-pane checks.
        // Earlier registration blocked new session creation for quarantined/dead channels.
        owned_sessions
            .entry(*channel_id)
            .or_insert_with(|| channel_name.clone());

        let initial_offset = std::fs::metadata(&output_path)
            .map(|m| m.len())
            .unwrap_or(0);

        pending.push(PendingWatcher {
            channel_id: *channel_id,
            output_path,
            session_name: session_name.to_string(),
            initial_offset,
        });
    }

    // Register sessions in CoreState so cleanup_orphan_tmux_sessions recognizes them
    // and message handlers find an active session with current_path
    if !owned_sessions.is_empty() {
        let mut data = shared.core.lock().await;
        for (channel_id, channel_name) in &owned_sessions {
            let persisted_path =
                load_last_session_path(shared.db.as_ref(), &shared.token_hash, channel_id.get());
            let remote_profile =
                load_last_remote_profile(shared.db.as_ref(), &shared.token_hash, channel_id.get());
            let persisted_session_id = load_restored_provider_session_id(
                shared.db.as_ref(),
                &shared.token_hash,
                &provider,
                channel_name,
            );
            let configured_path =
                super::settings::resolve_workspace(*channel_id, Some(channel_name.as_str()));
            let tmux_name = provider.build_tmux_session_name(channel_name);
            let session_keys = super::adk_session::build_session_key_candidates(
                &shared.token_hash,
                &provider,
                &tmux_name,
            );
            let db_cwd = shared.db.as_ref().and_then(|db| {
                db.lock()
                    .ok()
                    .and_then(|conn| load_restored_session_cwd_from_conn(&conn, &session_keys))
            });

            let session =
                data.sessions
                    .entry(*channel_id)
                    .or_insert_with(|| super::DiscordSession {
                        session_id: persisted_session_id.clone(),
                        memento_context_loaded: persisted_session_id.is_some(),
                        memento_reflected: false,
                        current_path: None,
                        history: Vec::new(),
                        pending_uploads: Vec::new(),
                        cleared: false,
                        channel_name: Some(channel_name.clone()),
                        category_name: None,
                        remote_profile_name: remote_profile.clone(),
                        channel_id: Some(channel_id.get()),

                        last_active: tokio::time::Instant::now(),
                        worktree: None,

                        born_generation: super::runtime_store::load_generation(),
                        assistant_turns: 0,
                    });

            if session.session_id.is_none() && persisted_session_id.is_some() {
                session.restore_provider_session(persisted_session_id.clone());
            }

            // Restore current_path: DB cwd (worktree-aware) > last_sessions (yaml, main workspace)
            if session.current_path.is_none() {
                if let (Some(configured), Some(restored)) =
                    (configured_path.as_ref(), db_cwd.as_ref())
                {
                    if configured != restored {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ⚠ Ignoring restored DB cwd for channel {}: {} (configured workspace: {})",
                            channel_id,
                            restored,
                            configured
                        );
                    }
                }
                let effective_path = super::select_restored_session_path(
                    configured_path,
                    db_cwd,
                    persisted_path,
                    remote_profile.as_deref(),
                );
                if let Some(path) = effective_path {
                    session.current_path = Some(path);
                }
            }
        }
    }

    // Spawn watchers
    // #226: Use try_claim_watcher for atomic check-and-insert. The pending list
    // was built during the scan phase, which includes async Discord API calls.
    // A normal turn may have created a watcher in the meantime.
    for pw in pending {
        // #226: Skip channels that recovery already handled — their watchers may have
        // ended quickly (session died), removing themselves from the DashMap, but we
        // should not create a second watcher because recovery already processed the turn.
        let recovery_handled = shared
            .db
            .as_ref()
            .and_then(|db| {
                db.lock().ok().and_then(|conn| {
                    conn.query_row(
                        "SELECT COUNT(*) > 0 FROM kv_meta WHERE key = ?1",
                        [format!("recovery_handled_channel:{}", pw.channel_id.get())],
                        |row| row.get::<_, bool>(0),
                    )
                    .ok()
                })
            })
            .unwrap_or(false);
        if recovery_handled {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — recovery already handled this channel",
                pw.session_name
            );
            continue;
        }

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
        let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let turn_delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let handle = TmuxWatcherHandle {
            paused: paused.clone(),
            resume_offset: resume_offset.clone(),
            cancel: cancel.clone(),
            pause_epoch: pause_epoch.clone(),
            turn_delivered: turn_delivered.clone(),
        };
        if !try_claim_watcher(&shared.tmux_watchers, pw.channel_id, handle) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — already watching (created during scan)",
                pw.session_name
            );
            continue;
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ Restoring tmux watcher for {} (offset {})",
            pw.session_name,
            pw.initial_offset
        );

        tokio::spawn(tmux_output_watcher(
            pw.channel_id,
            http.clone(),
            shared.clone(),
            pw.output_path,
            pw.session_name,
            pw.initial_offset,
            cancel,
            paused,
            resume_offset,
            pause_epoch,
            turn_delivered,
        ));
    }

    // Clean up dead sessions: report idle to DB and kill tmux sessions
    if !dead_cleanups.is_empty() {
        let api_port = shared.api_port;
        let provider = shared.settings.read().await.provider.clone();

        let mut cleaned_dead_sessions = 0usize;
        for dc in &dead_cleanups {
            let dispatch_protection = super::tmux_lifecycle::resolve_dispatch_tmux_protection(
                shared.db.as_ref(),
                &shared.token_hash,
                &provider,
                &dc.session_name,
                Some(&dc.channel_name),
            );
            let cleanup_plan = dead_session_cleanup_plan(dispatch_protection.is_some());

            if let Some(protection) = dispatch_protection {
            ) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ♻ tmux startup: preserving dispatch session {} — {}",
                    dc.session_name,
                    protection.log_reason()
                );
            }
            let cleanup_plan = dead_session_cleanup_plan(dispatch_protection.is_some());

            let tmux_name = provider.build_tmux_session_name(&dc.channel_name);
            let thread_channel_id =
                super::adk_session::parse_thread_channel_id_from_name(&dc.channel_name);
            let session_key = super::adk_session::build_namespaced_session_key(
                &shared.token_hash,
                &provider,
                &tmux_name,
            );
            let agent_id =
                resolve_role_binding(ChannelId::new(dc.channel_id), Some(&dc.channel_name))
                    .map(|binding| binding.role_id);

            if cleanup_plan.report_idle_status {
                super::adk_session::post_adk_session_status(
                    Some(&session_key),
                    Some(&dc.channel_name),
                    None,
                    "idle",
                    &provider,
                    None,
                    None,
                    None,
                    None,
                    thread_channel_id,
                    agent_id.as_deref(),
                    api_port,
                )
                .await;
            }

            if cleanup_plan.preserve_tmux_session {
                continue;
            }

            // Kill the dead tmux session
            let sess = dc.session_name.clone();
            let _ = tokio::task::spawn_blocking(move || {
                crate::services::termination_audit::record_termination_for_tmux(
                    &sess,
                    None,
                    "tmux_startup",
                    "startup_dead_session",
                    Some("startup cleanup: dead session"),
                    None,
                );
                record_tmux_exit_reason(&sess, "startup cleanup: dead session");
                crate::services::platform::tmux::kill_session(&sess);
            })
            .await;
            cleaned_dead_sessions += 1;
        }

        if cleaned_dead_sessions > 0 {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 Cleaned {} dead tmux session(s) on startup",
                cleaned_dead_sessions
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DeadSessionCleanupPlan, PROVIDER_OVERLOAD_RETRY_STATE, ProviderOverloadDecision,
        RestartHandoffScope, WatcherToolState, clear_provider_overload_retry_state,
        dead_session_cleanup_plan, detect_provider_overload_message, is_auth_error_message,
        is_prompt_too_long_message, load_restored_provider_session_id,
        normalized_retry_payload_text, process_watcher_lines, provider_overload_fingerprint,
        provider_overload_retry_delay, record_provider_overload_retry,
        resolve_dispatched_thread_dispatch_from_conn, resolve_restart_handoff_scope,
        watcher_ready_for_input_turn_completed,
    };
    use crate::services::discord::inflight::InflightTurnState;
    use crate::services::provider::{ProviderKind, ReadyForInputIdleTracker};
    use crate::services::session_backend::StreamLineState;
    use poise::serenity_prelude::ChannelId;

    fn sample_inflight_state() -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Claude,
            1479671298497183835,
            Some("adk-cc".to_string()),
            1,
            10,
            11,
            "restart me".to_string(),
            Some("session-123".to_string()),
            Some("AgentDesk-claude-adk-cc".to_string()),
            Some("/tmp/adk-cc.jsonl".to_string()),
            None,
            0,
        )
    }

    #[test]
    fn restart_handoff_prefers_exact_metadata_match() {
        let state = sample_inflight_state();
        let scope = resolve_restart_handoff_scope(
            &state,
            "AgentDesk-claude-adk-cc",
            "/tmp/other-output.jsonl",
        );
        assert_eq!(scope, RestartHandoffScope::ExactMetadata);
    }

    #[test]
    fn restart_handoff_allows_provider_channel_fallback_on_metadata_drift() {
        let state = sample_inflight_state();
        let scope = resolve_restart_handoff_scope(
            &state,
            "AgentDesk-claude-adk-cc-restarted",
            "/tmp/new-output.jsonl",
        );
        assert_eq!(scope, RestartHandoffScope::ProviderChannelScopedFallback);
    }

    #[test]
    fn watcher_dispatch_db_fallback_prefers_dispatched_thread_row() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE task_dispatches (
                id TEXT PRIMARY KEY,
                status TEXT,
                thread_id TEXT,
                created_at TEXT
            );
            CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                status TEXT,
                active_dispatch_id TEXT,
                created_at TEXT,
                last_heartbeat TEXT,
                thread_channel_id TEXT
            );
            INSERT INTO task_dispatches (id, status, thread_id, created_at)
            VALUES
                ('older-dispatch', 'dispatched', '1492091375422930966', '2026-04-11 00:15:42'),
                ('latest-dispatch', 'dispatched', '1492091375422930966', '2026-04-11 00:15:43');
            INSERT INTO sessions (status, active_dispatch_id, created_at, last_heartbeat, thread_channel_id)
            VALUES ('working', 'session-dispatch', '2026-04-11 00:15:40', '2026-04-11 00:24:21', '1492091375422930966');
            ",
        )
        .unwrap();

        let resolved =
            resolve_dispatched_thread_dispatch_from_conn(&conn, 1_492_091_375_422_930_966);
        assert_eq!(resolved.as_deref(), Some("latest-dispatch"));
    }

    #[test]
    fn watcher_dispatch_db_fallback_uses_session_when_thread_row_missing() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE task_dispatches (
                id TEXT PRIMARY KEY,
                status TEXT,
                thread_id TEXT,
                created_at TEXT
            );
            CREATE TABLE sessions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                status TEXT,
                active_dispatch_id TEXT,
                created_at TEXT,
                last_heartbeat TEXT,
                thread_channel_id TEXT
            );
            INSERT INTO sessions (status, active_dispatch_id, created_at, last_heartbeat, thread_channel_id)
            VALUES ('working', 'session-dispatch', '2026-04-11 00:15:40', '2026-04-11 00:24:21', '1492091380045189131');
            ",
        )
        .unwrap();

        let resolved =
            resolve_dispatched_thread_dispatch_from_conn(&conn, 1_492_091_380_045_189_131);
        assert_eq!(resolved.as_deref(), Some("session-dispatch"));
    }

    #[test]
    fn restored_live_tmux_session_loads_namespaced_provider_session_id() {
        let db = crate::db::test_db();
        let provider = ProviderKind::Codex;
        let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
            "tokenxyz",
            &provider,
            &provider.build_tmux_session_name("adk-cdx"),
        );
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions (session_key, provider, claude_session_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![session_key, provider.as_str(), "persisted-sid-1"],
            )
            .unwrap();

        assert_eq!(
            load_restored_provider_session_id(Some(&db), "tokenxyz", &provider, "adk-cdx")
                .as_deref(),
            Some("persisted-sid-1")
        );
    }

    #[test]
    fn restored_live_tmux_session_falls_back_to_legacy_session_key() {
        let db = crate::db::test_db();
        let provider = ProviderKind::Codex;
        let session_key = crate::services::discord::adk_session::build_legacy_session_key(
            &provider.build_tmux_session_name("adk-cdx"),
        );
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions (session_key, provider, claude_session_id) VALUES (?1, ?2, ?3)",
                rusqlite::params![session_key, provider.as_str(), "legacy-sid-1"],
            )
            .unwrap();

        assert_eq!(
            load_restored_provider_session_id(Some(&db), "tokenxyz", &provider, "adk-cdx")
                .as_deref(),
            Some("legacy-sid-1")
        );
    }

    #[test]
    fn watcher_ignores_assistant_text_that_mentions_stale_resume_phrase() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"The log contained No conversation found while I was debugging.\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(!outcome.stale_resume_detected);
        assert_eq!(
            full_response,
            "The log contained No conversation found while I was debugging."
        );
    }

    #[test]
    fn watcher_detects_structured_stale_resume_error_result() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"partial\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"errors\":[\"No conversation found\"]}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.stale_resume_detected);
        assert_eq!(full_response, "partial");
    }

    #[test]
    fn watcher_detects_provider_overload_from_structured_errors() {
        let mut buffer = concat!(
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"errors\":[\"Selected model is at capacity. Please try a different model.\"]}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_provider_overloaded);
        assert_eq!(
            outcome.provider_overload_message.as_deref(),
            Some("Selected model is at capacity. Please try a different model.")
        );
        assert!(full_response.is_empty());
    }

    #[test]
    fn watcher_detects_plain_text_provider_overload_line() {
        let mut buffer =
            "Selected model is at capacity. Please try a different model.\n".to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_provider_overloaded);
        assert_eq!(
            outcome.provider_overload_message.as_deref(),
            Some("Selected model is at capacity. Please try a different model.")
        );
        assert!(full_response.is_empty());
    }

    // ── #378 E2E: detect_provider_overload_message pattern coverage ──

    #[test]
    fn overload_detects_rate_limit_text() {
        assert!(detect_provider_overload_message("Rate limit exceeded").is_some());
        assert!(detect_provider_overload_message("rate limit reached for model").is_some());
    }

    #[test]
    fn overload_detects_too_many_requests() {
        assert!(detect_provider_overload_message("Too many requests").is_some());
        assert!(
            detect_provider_overload_message("429 Too Many Requests — please slow down").is_some()
        );
    }

    #[test]
    fn overload_detects_server_overloaded_variants() {
        assert!(detect_provider_overload_message("provider overloaded").is_some());
        assert!(detect_provider_overload_message("Server overloaded").is_some());
        assert!(detect_provider_overload_message("Service overloaded").is_some());
        assert!(detect_provider_overload_message("The API is overloaded right now").is_some());
    }

    #[test]
    fn overload_detects_please_try_again_later() {
        assert!(detect_provider_overload_message("Please try again later.").is_some());
    }

    #[test]
    fn overload_detects_at_capacity_with_model() {
        assert!(
            detect_provider_overload_message(
                "The selected model is at capacity. Please try a different model."
            )
            .is_some()
        );
        assert!(detect_provider_overload_message("model is at capacity").is_some());
        assert!(detect_provider_overload_message("This model is currently at capacity").is_some());
    }

    #[test]
    fn overload_ignores_empty_and_normal_text() {
        assert!(detect_provider_overload_message("").is_none());
        assert!(detect_provider_overload_message("   ").is_none());
        assert!(detect_provider_overload_message("Hello world").is_none());
        assert!(detect_provider_overload_message("Build succeeded").is_none());
        assert!(
            detect_provider_overload_message(
                "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"ok\"}]}}"
            )
            .is_none()
        );
    }

    #[test]
    fn overload_preserves_original_message_text() {
        let msg = "  Selected model is at capacity. Please try a different model.  ";
        let result = detect_provider_overload_message(msg).unwrap();
        assert_eq!(result, msg.trim());
    }

    // ── #378 E2E: is_prompt_too_long_message coverage ──

    #[test]
    fn prompt_too_long_detects_all_variants() {
        assert!(is_prompt_too_long_message("prompt is too long"));
        assert!(is_prompt_too_long_message("Error: prompt too long"));
        assert!(is_prompt_too_long_message("context_length_exceeded"));
        assert!(is_prompt_too_long_message("conversation too long"));
        assert!(is_prompt_too_long_message("exceeded context window"));
    }

    #[test]
    fn prompt_too_long_ignores_normal() {
        assert!(!is_prompt_too_long_message("everything is fine"));
        assert!(!is_prompt_too_long_message(""));
    }

    // ── #378 E2E: is_auth_error_message coverage ──

    #[test]
    fn auth_error_detects_all_variants() {
        assert!(is_auth_error_message("not logged in"));
        assert!(is_auth_error_message("Authentication error"));
        assert!(is_auth_error_message("Unauthorized"));
        assert!(is_auth_error_message("Please run /login first"));
        assert!(is_auth_error_message("OAuth token refresh failed"));
        assert!(is_auth_error_message("access token could not be refreshed"));
        assert!(is_auth_error_message("refresh token expired"));
        assert!(is_auth_error_message("Token expired"));
        assert!(is_auth_error_message("Invalid API key"));
        assert!(is_auth_error_message("API key is missing"));
        assert!(is_auth_error_message("API key expired"));
    }

    #[test]
    fn auth_error_ignores_normal() {
        assert!(!is_auth_error_message("Build succeeded"));
        assert!(!is_auth_error_message(""));
    }

    // ── #378 E2E: retry state machine ──

    #[test]
    fn retry_delay_is_exponential_backoff() {
        assert_eq!(provider_overload_retry_delay(1).as_secs(), 120); // 2min
        assert_eq!(provider_overload_retry_delay(2).as_secs(), 240); // 4min
        assert_eq!(provider_overload_retry_delay(3).as_secs(), 480); // 8min
    }

    #[test]
    fn retry_state_machine_escalates_then_exhausts() {
        // Use a unique channel ID to avoid test interference
        let channel = ChannelId::new(999_000_378_001);
        clear_provider_overload_retry_state(channel);

        let text = "── dispatch ──\nDISPATCH:abc test task";

        // Attempt 1
        let d1 = record_provider_overload_retry(channel, text);
        match &d1 {
            ProviderOverloadDecision::Retry { attempt, .. } => assert_eq!(*attempt, 1),
            _ => panic!("expected Retry, got {:?}", d1),
        }

        // Attempt 2
        let d2 = record_provider_overload_retry(channel, text);
        match &d2 {
            ProviderOverloadDecision::Retry { attempt, .. } => assert_eq!(*attempt, 2),
            _ => panic!("expected Retry, got {:?}", d2),
        }

        // Attempt 3
        let d3 = record_provider_overload_retry(channel, text);
        match &d3 {
            ProviderOverloadDecision::Retry { attempt, .. } => assert_eq!(*attempt, 3),
            _ => panic!("expected Retry, got {:?}", d3),
        }

        // Attempt 4 → Exhausted
        let d4 = record_provider_overload_retry(channel, text);
        assert_eq!(d4, ProviderOverloadDecision::Exhausted);

        // State should be cleared after exhaustion
        assert!(!PROVIDER_OVERLOAD_RETRY_STATE.contains_key(&channel.get()));
    }

    #[test]
    fn retry_state_resets_on_different_fingerprint() {
        let channel = ChannelId::new(999_000_378_002);
        clear_provider_overload_retry_state(channel);

        let text_a = "first task payload";
        let text_b = "totally different payload";

        let d1 = record_provider_overload_retry(channel, text_a);
        match &d1 {
            ProviderOverloadDecision::Retry { attempt, .. } => assert_eq!(*attempt, 1),
            _ => panic!("expected Retry"),
        }

        // Different text → fingerprint mismatch → resets to attempt 1
        let d2 = record_provider_overload_retry(channel, text_b);
        match &d2 {
            ProviderOverloadDecision::Retry { attempt, .. } => assert_eq!(*attempt, 1),
            _ => panic!("expected Retry after fingerprint change"),
        }

        clear_provider_overload_retry_state(channel);
    }

    #[test]
    fn clear_retry_state_removes_entry() {
        let channel = ChannelId::new(999_000_378_003);
        record_provider_overload_retry(channel, "some text");
        assert!(PROVIDER_OVERLOAD_RETRY_STATE.contains_key(&channel.get()));
        clear_provider_overload_retry_state(channel);
        assert!(!PROVIDER_OVERLOAD_RETRY_STATE.contains_key(&channel.get()));
    }

    // ── #378 E2E: normalized_retry_payload_text strips retry headers ──

    #[test]
    fn normalized_payload_strips_retry_header() {
        let input = "⚠️ 자동 재시도 (2/3)\n\noriginal user message";
        assert_eq!(
            normalized_retry_payload_text(input),
            "original user message"
        );
    }

    #[test]
    fn normalized_payload_strips_history_restore_header() {
        let input = "📋 이전 대화 복원 중...\n\nactual prompt text";
        assert_eq!(normalized_retry_payload_text(input), "actual prompt text");
    }

    #[test]
    fn normalized_payload_keeps_plain_text() {
        let input = "just a normal message";
        assert_eq!(normalized_retry_payload_text(input), input);
    }

    // ── #378 E2E: fingerprint consistency ──

    #[test]
    fn fingerprint_stable_for_same_input() {
        let a = provider_overload_fingerprint("hello world");
        let b = provider_overload_fingerprint("hello world");
        assert_eq!(a, b);
    }

    #[test]
    fn fingerprint_differs_for_different_input() {
        let a = provider_overload_fingerprint("task A");
        let b = provider_overload_fingerprint("task B");
        assert_ne!(a, b);
    }

    #[test]
    fn fingerprint_normalizes_retry_headers() {
        let raw = "original message";
        let with_header = "⚠️ 자동 재시도 (1/3)\n\noriginal message";
        assert_eq!(
            provider_overload_fingerprint(raw),
            provider_overload_fingerprint(with_header)
        );
    }

    // ── #378 E2E: process_watcher_lines integration — overload does NOT leak into full_response ──

    #[test]
    fn overload_in_structured_result_does_not_populate_response() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"working...\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"errors\":[\"Too many requests\"]}\n"
        ).to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_provider_overloaded);
        // full_response should NOT contain the overload error
        assert!(
            !full_response.contains("Too many requests"),
            "overload error should not leak into full_response, got: {full_response}"
        );
    }

    #[test]
    fn overload_in_plain_text_does_not_populate_response() {
        let mut buffer = "Server overloaded, please retry later\n".to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_provider_overloaded);
        assert!(full_response.is_empty());
    }

    // ── #378 E2E: overload flag does NOT interfere with other error types ──

    #[test]
    fn prompt_too_long_error_is_not_flagged_as_overload() {
        let mut buffer =
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"prompt is too long\"}\n"
                .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_prompt_too_long);
        assert!(!outcome.is_provider_overloaded);
    }

    #[test]
    fn auth_error_is_not_flagged_as_overload() {
        let mut buffer =
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"result\":\"not logged in\"}\n"
                .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_auth_error);
        assert_eq!(outcome.auth_error_message.as_deref(), Some("not logged in"));
        assert!(!outcome.is_provider_overloaded);
    }

    #[test]
    fn plain_text_auth_error_is_detected_and_preserved() {
        let mut buffer = "access token could not be refreshed\n".to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_auth_error);
        assert_eq!(
            outcome.auth_error_message.as_deref(),
            Some("access token could not be refreshed")
        );
        assert!(full_response.is_empty());
    }

    // ── #378 E2E: mixed error + overload in errors array ──

    #[test]
    fn mixed_auth_and_overload_errors_sets_both_flags() {
        let mut buffer =
            "{\"type\":\"result\",\"subtype\":\"error_during_execution\",\"is_error\":true,\"errors\":[\"not logged in\",\"server overloaded\"]}\n"
                .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.is_auth_error);
        assert!(outcome.is_provider_overloaded);
    }

    // ── #378 E2E: normal success result is not flagged ──

    #[test]
    fn normal_success_result_has_no_error_flags() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Here is the answer.\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\"}\n"
        ).to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(!outcome.is_prompt_too_long);
        assert!(!outcome.is_auth_error);
        assert!(!outcome.is_provider_overloaded);
        assert!(!outcome.stale_resume_detected);
        assert_eq!(full_response, "Here is the answer.");
    }

    #[test]
    fn watcher_tracks_previous_tool_status_for_two_line_trail() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Read\",\"input\":{\"file_path\":\"src/config.rs\"}}]}}\n",
            "{\"type\":\"content_block_stop\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{\"command\":\"cargo build\"}}]}}\n"
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(!outcome.found_result);
        assert_eq!(
            tool_state.prev_tool_status.as_deref(),
            Some("✓ Read: src/config.rs")
        );
        assert_eq!(
            tool_state.current_tool_line.as_deref(),
            Some("⚙ Bash: `cargo build`")
        );
    }

    #[test]
    fn dead_session_cleanup_plan_preserves_tmux_but_still_reports_idle() {
        let plan = dead_session_cleanup_plan(true);

        assert_eq!(
            plan,
            DeadSessionCleanupPlan {
                preserve_tmux_session: true,
                report_idle_status: true,
            }
        );
    }

    #[test]
    fn dead_session_cleanup_plan_kills_unprotected_sessions_and_reports_idle() {
        let plan = dead_session_cleanup_plan(false);

        assert_eq!(
            plan,
            DeadSessionCleanupPlan {
                preserve_tmux_session: false,
                report_idle_status: true,
            }
        );
    }

    #[test]
    fn watcher_ready_for_input_completion_requires_stable_idle_prompt_after_output() {
        let mut tracker = ReadyForInputIdleTracker::default();
        let start = std::time::Instant::now();

        assert!(!watcher_ready_for_input_turn_completed(
            &mut tracker,
            100,
            100,
            true,
            start
        ));

        tracker.record_output();
        assert!(!watcher_ready_for_input_turn_completed(
            &mut tracker,
            100,
            120,
            true,
            start
        ));
        assert!(!watcher_ready_for_input_turn_completed(
            &mut tracker,
            100,
            120,
            true,
            start + std::time::Duration::from_secs(10)
        ));
        assert!(watcher_ready_for_input_turn_completed(
            &mut tracker,
            100,
            120,
            true,
            start + std::time::Duration::from_secs(16)
        ));
    }
}
