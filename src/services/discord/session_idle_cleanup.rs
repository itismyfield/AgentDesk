use crate::services::discord::adk_session;
use crate::services::discord::mailbox_finish::mailbox_clear_channel_without_spawn;
use crate::services::discord::session_runtime::cleanup_git_worktree;
use crate::services::discord::{
    SESSION_CLEANUP_INTERVAL, SESSION_MAX_IDLE, SharedData, saturating_decrement_global_active,
};
use poise::serenity_prelude::{self as serenity, ButtonKind, ChannelId, MessageId};
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleSessionWatcherCleanup {
    ExpireSession,
    DeferToTmuxLiveness,
}

fn idle_session_watcher_cleanup(has_watcher: bool) -> IdleSessionWatcherCleanup {
    if has_watcher {
        IdleSessionWatcherCleanup::DeferToTmuxLiveness
    } else {
        IdleSessionWatcherCleanup::ExpireSession
    }
}

/// Periodically clean up idle sessions and their associated data.
/// Called from handle_event; uses a static Mutex to track the last cleanup time.
///
/// 호출부는 `router/intake_gate.rs` 하나뿐이고, 그 파일의 `use super::super::*`
/// 가 부모 `mod.rs` 의 재수출 바인딩을 집어간다. 따라서 `mod.rs` 쪽 `use` 는
/// 그 파일 안에서 호출되지 않더라도 load-bearing 이다 — 제거하면 호출부가
/// 이름을 해석하지 못한다.
pub(in crate::services::discord) async fn maybe_cleanup_sessions(shared: &Arc<SharedData>) {
    use std::sync::OnceLock;
    static LAST_CLEANUP: OnceLock<tokio::sync::Mutex<tokio::time::Instant>> = OnceLock::new();
    let last = LAST_CLEANUP.get_or_init(|| tokio::sync::Mutex::new(tokio::time::Instant::now()));
    let mut last_guard = last.lock().await;
    if last_guard.elapsed() < SESSION_CLEANUP_INTERVAL {
        return;
    }
    *last_guard = tokio::time::Instant::now();
    drop(last_guard);
    cleanup_expired_sessions(shared).await;
}

async fn cleanup_expired_sessions(shared: &Arc<SharedData>) {
    struct ExpiredSessionCleanup {
        channel_id: ChannelId,
        channel_name: Option<String>,
        session_key: Option<String>,
    }

    let provider = shared.settings.read().await.provider.clone();
    let expired: Vec<ExpiredSessionCleanup> = {
        let data = shared.core.lock().await;
        let now = tokio::time::Instant::now();
        data.sessions
            .iter()
            .filter(|(channel_id, s)| {
                now.duration_since(s.last_active) > SESSION_MAX_IDLE
                    && matches!(
                        idle_session_watcher_cleanup(shared.tmux_watchers.contains_key(channel_id)),
                        IdleSessionWatcherCleanup::ExpireSession
                    )
            })
            .map(|(ch, s)| ExpiredSessionCleanup {
                channel_id: *ch,
                channel_name: s.channel_name.clone(),
                session_key: None,
            })
            .collect()
    };
    let mut safe_expired = Vec::new();
    for mut candidate in expired {
        // A Herdr-configured channel keeps its session, returned input, mailbox and worktree.
        let configured = super::admin_host_guard::configured_refusal;
        if configured(candidate.channel_id.get()).is_some() {
            continue;
        }
        // A missing watcher/inflight record can be a relay failure while the
        // actual provider still works. Do not clear its mailbox or worktree.
        let Some(pool) = shared.pg_pool.as_ref() else {
            continue;
        };
        if super::mailbox_has_active_turn(shared, candidate.channel_id).await {
            continue;
        }
        // The channel's canonical row is checked before a tmux name is built from its name.
        let tmux_name = |name: &String| provider.build_tmux_session_name(name);
        let host =
            crate::services::tmux_turn_liveness::cleanup_host::confirm_legacy_tmux_channel_pg(
                pool,
                provider.as_str(),
                &shared.token_hash,
                &candidate.channel_id.get().to_string(),
                || {
                    let tmux_name = tmux_name(candidate.channel_name.as_ref()?);
                    let key = adk_session::build_namespaced_session_key(
                        &shared.token_hash,
                        &provider,
                        &tmux_name,
                    );
                    Some(key)
                },
            )
            .await;
        let key = match host {
            Ok(key) => key,
            Err(refusal) => {
                tracing::info!(
                    channel_id = candidate.channel_id.get(),
                    preserved_reason = refusal.reason(),
                    "idle cleanup preserved session: not a legacy tmux session"
                );
                continue;
            }
        };
        if !crate::services::tmux_turn_liveness::idle_cleanup_session_is_unoccupied(pool, &key)
            .await
        {
            continue;
        }
        let Some(tmux_name) = candidate.channel_name.as_ref().map(tmux_name) else {
            continue;
        };
        candidate.session_key = Some(key);
        let safe = tokio::task::spawn_blocking(move || {
            use crate::services::platform::tmux::{SessionPresence, session_presence};
            match session_presence(&tmux_name) {
                SessionPresence::Missing => true,
                SessionPresence::Present => {
                    crate::services::tmux_turn_liveness::provider_session_is_proven_idle(&tmux_name)
                }
                SessionPresence::ProbeFailed => false,
            }
        })
        .await
        .unwrap_or(false);
        if safe {
            safe_expired.push(candidate);
        }
    }
    let mut expired = safe_expired;
    if expired.is_empty() {
        return;
    }
    {
        let mut data = shared.core.lock().await;
        expired.retain(|candidate| {
            !shared.tmux_watchers.contains_key(&candidate.channel_id)
                && data
                    .sessions
                    .get(&candidate.channel_id)
                    .is_some_and(|session| session.last_active.elapsed() > SESSION_MAX_IDLE)
        });
        for expired_session in &expired {
            let ch = expired_session.channel_id;
            // Clean up worktree if session had one
            if let Some(session) = data.sessions.get(&ch) {
                if let Some(ref wt) = session.worktree {
                    cleanup_git_worktree(shared.pg_pool.as_ref(), wt);
                }
            }
            data.sessions.remove(&ch);
        }
    }
    // #3588: idle 정리는 in-memory/worktree 메모리 회수만 수행하고 provider
    // session(claude resume id)은 DB에 보존한다. 다음 턴에서
    // `fetch_provider_session_id`로 복원되어 `--resume`으로 transcript가 이어진다.
    // retry_context(session_retry_context_key) kv는 의도적으로 저장하지 않는다 —
    // 같은 키를 `take_session_retry_context`가 다음 턴에 무조건 take/주입하므로,
    // resume이 성공하는 idle 경로에서 저장하면 transcript 중복 + "새 세션 시작"
    // 레이블 오표시가 발생한다. (#3591에서 100턴 세션 리셋도 제거되어 reset 기반
    // 저장 경로는 없다; resume 실패 복구만 auto_retry_with_history가 별도로 저장한다.)
    // 명시적 세션 초기화는 !clear 또는 /clear 명령으로 한다.
    for expired_session in &expired {
        let channel_id = expired_session.channel_id;
        let cleared = mailbox_clear_channel_without_spawn(shared, &provider, channel_id).await;
        if cleared.removed_token.is_some() {
            saturating_decrement_global_active(shared);
        }
        shared.api_timestamps.remove(&expired_session.channel_id);
    }
    // Record termination audit for cleaned-up sessions
    for expired_session in &expired {
        if let Some(session_key) = expired_session.session_key.as_deref() {
            let should_record =
                mark_session_disconnected_for_idle_cleanup(shared.pg_pool.as_ref(), session_key)
                    .await;
            if !should_record {
                continue;
            }

            crate::services::termination_audit::record_termination_with_handles(
                shared.pg_pool.as_ref(),
                session_key,
                None,
                "cleanup",
                "idle_session_expiry",
                Some("in-memory session expired due to idle timeout"),
                None,
                None,
                None,
            );
        }
    }
    tracing::info!("  [cleanup] Removed {} idle session(s)", expired.len());
}

pub(in crate::services::discord) async fn mark_session_disconnected_for_idle_cleanup(
    pg_pool: Option<&sqlx::PgPool>,
    session_key: &str,
) -> bool {
    let Some(pool) = pg_pool else {
        return false;
    };
    // A row that gained a hosted record after the host check keeps its status and dispatch.
    let prior_status = sqlx::query_scalar::<_, String>(
        "SELECT status FROM sessions WHERE session_key = $1 AND hosted_execution IS NULL",
    )
    .bind(session_key)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();

    let _ = sqlx::query(
        "UPDATE sessions
         SET status = 'disconnected', active_dispatch_id = NULL
         WHERE session_key = $1 AND hosted_execution IS NULL",
    )
    .bind(session_key)
    .execute(pool)
    .await;

    prior_status.is_some_and(|status| status != "disconnected")
}

// Unix-gated: the test installs a fake `tmux` on PATH, which needs `PermissionsExt`.
#[cfg(all(test, unix))]
#[path = "session_idle_cleanup_tests.rs"]
mod tests;

// Old cards keep their captured-id delete and pointer CAS until the next user turn.
const IDLE_RECAP_CLEAR_BUTTON_PREFIX: &str = "idle_recap:clear:";

pub(super) fn content_looks_like_idle_recap_card(content: &str) -> bool {
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
    #[cfg(test)]
    if recap_cleanup_mutant("bypass_card_guard") {
        return true;
    }
    content_looks_like_idle_recap_card(&message.content)
        && message.components.iter().any(|row| {
            row.components
                .iter()
                .any(component_is_idle_recap_clear_button)
        })
}

pub(crate) async fn delete_previous_card(http: &serenity::Http, channel_id: u64, message_id: u64) {
    #[cfg(test)]
    if recap_cleanup_mutant("skip_delete") {
        return;
    }
    let channel = ChannelId::new(channel_id);
    let message = MessageId::new(message_id);
    match http.get_message(channel, message).await {
        Ok(current) if message_is_idle_recap_card(&current) => {
            let result = super::http::delete_channel_message(http, channel, message).await;
            crate::services::observability::emit_relay_delete_result(
                "",
                channel_id,
                message_id,
                "idle_recap_previous_card",
                "delete_nonterminal",
                &result,
            );
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

pub(crate) async fn clear_recap_pointer(
    pool: &PgPool,
    session_key: &str,
    expected_message_id: u64,
) -> Result<bool, sqlx::Error> {
    #[cfg(test)]
    if recap_cleanup_mutant("skip_pointer_clear") {
        return Ok(false);
    }
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

async fn clear_specific_idle_recap_card(
    http: &serenity::Http,
    pool: &sqlx::PgPool,
    session_key: &str,
    channel_id: u64,
    message_id: u64,
) {
    delete_previous_card(http, channel_id, message_id).await;
    let _ = clear_recap_pointer(pool, session_key, message_id).await;
}

/// Test-only cleanup mutations; production retains the claim and card guards.
#[cfg(test)]
pub(in crate::services::discord) fn recap_cleanup_mutant(name: &str) -> bool {
    std::env::var("ADK_RECAP_CLEANUP_MUTANT").ok().as_deref() == Some(name)
}

#[cfg(test)]
pub(in crate::services::discord) mod recap_cleanup_tests {
    use super::*;
    use axum::{
        Json, Router,
        http::{Method, StatusCode},
        response::IntoResponse,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    pub(in crate::services::discord) async fn discord(
        content: &str,
        with_button: bool,
    ) -> (
        Arc<serenity::Http>,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<()>,
    ) {
        let deletes = Arc::new(AtomicUsize::new(0));
        let observed = deletes.clone();
        let content = content.to_owned();
        let app = Router::new().fallback(move |method: Method| {
            let observed = observed.clone();
            let content = content.clone();
            async move {
                if method == Method::DELETE {
                    observed.fetch_add(1, Ordering::SeqCst);
                    return StatusCode::NO_CONTENT.into_response();
                }
                assert_eq!(method, Method::GET, "cleanup must never create or edit a message");
                let components = if with_button {
                    serde_json::json!([{"type":1,"components":[{"type":2,"style":2,"label":"새 세션 시작","custom_id":"idle_recap:clear:4242"}]}])
                } else { serde_json::json!([]) };
                Json(serde_json::json!({
                    "id":"4242", "channel_id":"777", "content":content,
                    "author":{"id":"1","username":"test","discriminator":"0001","avatar":null},
                    "timestamp":"2026-09-27T00:00:00+00:00", "edited_timestamp":null,
                    "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[],
                    "attachments":[], "embeds":[], "pinned":false, "type":0,
                    "components":components
                })).into_response()
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http = Arc::new(
            serenity::HttpBuilder::new("test-token")
                .proxy(format!("http://{}", listener.local_addr().unwrap()))
                .ratelimiter_disabled(true)
                .build(),
        );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (http, deletes, server)
    }

    async fn seed(pool: &PgPool, message: i64) {
        seed_for_channel(pool, message, 777).await;
    }

    pub(in crate::services::discord) async fn seed_for_channel(
        pool: &PgPool,
        message: i64,
        channel: u64,
    ) {
        sqlx::query("INSERT INTO sessions (session_key, provider, idle_recap_message_id, idle_recap_channel_id) VALUES ('recap-cleanup', 'claude', $1, $2)")
            .bind(message).bind(channel as i64).execute(pool).await.unwrap();
    }

    pub(in crate::services::discord) async fn assert_cleared(
        pool: &PgPool,
        deletes: &AtomicUsize,
        boundary: &str,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while pointer(pool).await.is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{boundary} must clear the recorded idle-card pointer"));
        assert_eq!(
            deletes.load(Ordering::SeqCst),
            1,
            "{boundary} must delete the captured idle card exactly once"
        );
    }

    pub(in crate::services::discord) async fn pointer(pool: &PgPool) -> Option<i64> {
        sqlx::query_scalar(
            "SELECT idle_recap_message_id FROM sessions WHERE session_key = 'recap-cleanup'",
        )
        .fetch_one(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn next_claim_deletes_recorded_idle_card_and_clears_pointer_pg() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "recap_cleanup",
            "last idle card cleanup",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        seed(&pool, 4242).await;
        let (http, deletes, server) = discord("📦 응답 완료 · 이어서 질문 가능", true).await;
        spawn_clear_captured_idle_recap_for_channel(http, pool.clone(), 777).await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while pointer(&pool).await.is_some() {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the next claim must clear its recorded idle-card pointer");
        assert_eq!(
            deletes.load(Ordering::SeqCst),
            1,
            "the next claim must delete its recorded idle card exactly once"
        );
        let channel: Option<i64> = sqlx::query_scalar(
            "SELECT idle_recap_channel_id FROM sessions WHERE session_key = 'recap-cleanup'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(channel, None);
        server.abort();
        pool.close().await;
        db.drop().await;
    }

    #[tokio::test]
    async fn cleanup_preserves_normal_messages_and_a_successor_pointer_pg() {
        let db = crate::dispatch::test_support::DispatchPostgresTestDb::create(
            "recap_cleanup_guard",
            "idle card deletion boundary",
        )
        .await;
        let pool = db.connect_and_migrate().await;
        seed(&pool, 9999).await;
        for (content, with_button, expected_deletes) in [
            ("ordinary assistant answer", true, 0),
            ("📦 quoted ordinary answer", false, 0),
            ("📦 응답 완료 · 이어서 질문 가능", true, 1),
        ] {
            let (http, deletes, server) = discord(content, with_button).await;
            clear_specific_idle_recap_card(&http, &pool, "recap-cleanup", 777, 4242).await;
            assert_eq!(
                deletes.load(Ordering::SeqCst),
                expected_deletes,
                "only a recorded idle card with both markers may be deleted"
            );
            assert_eq!(
                pointer(&pool).await,
                Some(9999),
                "a captured old card must never clear a successor pointer"
            );
            server.abort();
        }
        pool.close().await;
        db.drop().await;
    }
}
