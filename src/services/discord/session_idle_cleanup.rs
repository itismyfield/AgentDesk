use crate::services::discord::adk_session;
use crate::services::discord::session_runtime::cleanup_git_worktree;
use crate::services::discord::{
    SESSION_CLEANUP_INTERVAL, SESSION_MAX_IDLE, SharedData, mailbox_clear_channel,
    saturating_decrement_global_active,
};
use poise::serenity_prelude::ChannelId;
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

    struct ExpiredSessionCleanup {
        channel_id: ChannelId,
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
                session_key: s.channel_name.as_ref().map(|name| {
                    let tmux_name = provider.build_tmux_session_name(name);
                    adk_session::build_namespaced_session_key(
                        &shared.token_hash,
                        &provider,
                        &tmux_name,
                    )
                }),
            })
            .collect()
    };
    if expired.is_empty() {
        return;
    }
    {
        let mut data = shared.core.lock().await;
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
    // 명시적 세션 초기화는 idle recap의 `새 세션 시작` 버튼(idle_recap:clear)으로 한다.
    for expired_session in &expired {
        let cleared = mailbox_clear_channel(shared, &provider, expired_session.channel_id).await;
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

async fn mark_session_disconnected_for_idle_cleanup(
    pg_pool: Option<&sqlx::PgPool>,
    session_key: &str,
) -> bool {
    let Some(pool) = pg_pool else {
        return false;
    };
    let prior_status =
        sqlx::query_scalar::<_, String>("SELECT status FROM sessions WHERE session_key = $1")
            .bind(session_key)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    let _ = sqlx::query(
        "UPDATE sessions
         SET status = 'disconnected', active_dispatch_id = NULL
         WHERE session_key = $1",
    )
    .bind(session_key)
    .execute(pool)
    .await;

    prior_status.as_deref() != Some("disconnected")
}

#[cfg(test)]
mod idle_cleanup_selector_tests {
    use crate::services::discord::session_idle_cleanup::mark_session_disconnected_for_idle_cleanup;

    struct TestPostgresDb {
        _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Self {
            let lifecycle = crate::db::postgres::lock_test_lifecycle();
            let admin_url = postgres_admin_database_url();
            let database_name =
                format!("agentdesk_idle_selector_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            crate::db::postgres::create_test_database(
                &admin_url,
                &database_name,
                "idle selector tests",
            )
            .await
            .expect("create idle selector postgres test db");

            Self {
                _lifecycle: lifecycle,
                admin_url,
                database_name,
                database_url,
            }
        }

        async fn connect_and_migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "idle selector tests",
            )
            .await
            .expect("apply idle selector postgres migrations")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "idle selector tests",
            )
            .await
            .expect("drop idle selector postgres test db");
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", postgres_base_database_url(), admin_db)
    }

    #[tokio::test]
    async fn idle_cleanup_preserves_provider_selector_columns_pg() {
        let pg_db = TestPostgresDb::create().await;
        let pool = pg_db.connect_and_migrate().await;
        let session_key = "host:idle-selector-preserve";

        sqlx::query(
            "INSERT INTO sessions
             (session_key, status, active_dispatch_id, claude_session_id,
              raw_provider_session_id, created_at)
             VALUES ($1, 'idle', 'dispatch-1841', 'claude-selector-1841',
                     'raw-selector-1841', NOW())",
        )
        .bind(session_key)
        .execute(&pool)
        .await
        .unwrap();

        assert!(mark_session_disconnected_for_idle_cleanup(Some(&pool), session_key).await);

        let row = sqlx::query_as::<_, (String, Option<String>, Option<String>, Option<String>)>(
            "SELECT status, active_dispatch_id, claude_session_id, raw_provider_session_id
             FROM sessions
             WHERE session_key = $1",
        )
        .bind(session_key)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "disconnected");
        assert_eq!(row.1, None);
        assert_eq!(row.2.as_deref(), Some("claude-selector-1841"));
        assert_eq!(row.3.as_deref(), Some("raw-selector-1841"));

        pool.close().await;
        pg_db.drop().await;
    }
}
