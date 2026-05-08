use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DispatchTmuxProtection {
    SessionRow {
        dispatch_id: String,
        session_status: String,
        dispatch_status: String,
    },
    ThreadDispatch {
        dispatch_id: String,
        dispatch_status: String,
    },
}

impl DispatchTmuxProtection {
    pub(super) fn log_reason(&self) -> String {
        match self {
            Self::SessionRow {
                dispatch_id,
                session_status,
                dispatch_status,
            } => format!(
                "session row keeps active_dispatch_id={dispatch_id} (session_status={session_status}, dispatch_status={dispatch_status})"
            ),
            Self::ThreadDispatch {
                dispatch_id,
                dispatch_status,
            } => {
                format!("thread dispatch {dispatch_id} is still active (status={dispatch_status})")
            }
        }
    }

    pub(super) fn active_dispatch_id(&self) -> Option<&str> {
        let (dispatch_id, dispatch_status) = match self {
            Self::SessionRow {
                dispatch_id,
                dispatch_status,
                ..
            }
            | Self::ThreadDispatch {
                dispatch_id,
                dispatch_status,
            } => (dispatch_id, dispatch_status),
        };
        matches!(dispatch_status.as_str(), "pending" | "dispatched").then_some(dispatch_id.as_str())
    }
}

pub(super) async fn fail_active_dispatch_for_dead_tmux_session(
    api_port: u16,
    protection: &DispatchTmuxProtection,
    tmux_session_name: &str,
    source: &str,
) -> bool {
    let Some(dispatch_id) = protection.active_dispatch_id() else {
        return false;
    };
    let reason = format!("tmux session died ({source}): session={tmux_session_name}");
    super::turn_bridge::fail_dispatch_tmux_session_died(api_port, Some(dispatch_id), &reason).await;
    true
}

pub(super) fn resolve_dispatch_tmux_protection(
    db: Option<&crate::db::Db>,
    pg_pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    tmux_session_name: &str,
    channel_name_hint: Option<&str>,
) -> Option<DispatchTmuxProtection> {
    let parsed_channel_name = parse_provider_and_channel_from_tmux_name(tmux_session_name)
        .and_then(|(parsed_provider, channel_name)| {
            (parsed_provider == *provider).then_some(channel_name)
        });
    let thread_channel_id = channel_name_hint
        .and_then(super::adk_session::parse_thread_channel_id_from_name)
        .or_else(|| {
            parsed_channel_name
                .as_deref()
                .and_then(super::adk_session::parse_thread_channel_id_from_name)
        })
        .map(|value| value.to_string())
        .unwrap_or_default();
    let namespaced_session_key_prefix = format!("{}/{}/%", provider.as_str(), token_hash);

    let session_keys =
        super::adk_session::build_session_key_candidates(token_hash, provider, tmux_session_name);
    if let Some(pg_pool) = pg_pool {
        let session_keys = session_keys.clone();
        let thread_channel_id = thread_channel_id.clone();
        let provider_name = provider.as_str().to_string();
        let namespaced_session_key_prefix = namespaced_session_key_prefix.clone();
        return crate::utils::async_bridge::block_on_pg_result(
            pg_pool,
            move |pool| async move {
                if let Some((dispatch_id, session_status, dispatch_status)) = sqlx::query_as::<
                    _,
                    (String, String, String),
                >(
                    "SELECT s.active_dispatch_id, s.status, td.status
                     FROM sessions s
                     JOIN task_dispatches td
                       ON td.id = s.active_dispatch_id
                     WHERE s.active_dispatch_id IS NOT NULL
                       AND s.status != 'disconnected'
                       AND (
                         s.session_key = $1
                         OR s.session_key = $2
                         OR (
                           $3 != ''
                           AND s.thread_channel_id = $3
                           AND s.provider = $4
                           AND s.session_key LIKE $5
                         )
                       )
                     ORDER BY
                       CASE s.status
                         WHEN 'turn_active' THEN 0 WHEN 'working' THEN 0 WHEN 'awaiting_bg' THEN 1
                         WHEN 'idle' THEN 1
                         ELSE 2
                       END,
                       COALESCE(s.last_heartbeat, s.created_at) DESC,
                       s.id DESC
                     LIMIT 1",
                )
                .bind(&session_keys[0])
                .bind(&session_keys[1])
                .bind(&thread_channel_id)
                .bind(&provider_name)
                .bind(&namespaced_session_key_prefix)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load pg tmux dispatch protection session row: {error}"))?
                {
                    return Ok(Some(DispatchTmuxProtection::SessionRow {
                        dispatch_id,
                        session_status,
                        dispatch_status,
                    }));
                }

                if thread_channel_id.is_empty() {
                    return Ok(None);
                }

                let protection = sqlx::query_as::<_, (String, String)>(
                    "SELECT td.id, td.status
                     FROM task_dispatches td
                     WHERE td.thread_id = $1
                       AND td.status IN ('pending', 'dispatched')
                       AND EXISTS (
                         SELECT 1
                         FROM sessions s
                         WHERE s.thread_channel_id = $1
                           AND s.provider = $2
                           AND s.session_key LIKE $3
                       )
                     ORDER BY
                       CASE td.status
                         WHEN 'dispatched' THEN 0
                         WHEN 'pending' THEN 1
                         ELSE 2
                       END,
                       td.created_at DESC,
                       td.id DESC
                     LIMIT 1",
                )
                .bind(&thread_channel_id)
                .bind(&provider_name)
                .bind(&namespaced_session_key_prefix)
                .fetch_optional(&pool)
                .await
                .map_err(|error| format!("load pg tmux dispatch protection thread row: {error}"))?
                .map(|(dispatch_id, dispatch_status)| {
                    DispatchTmuxProtection::ThreadDispatch {
                        dispatch_id,
                        dispatch_status,
                    }
                });
                Ok(protection)
            },
            |message| message,
        )
        .ok()
        .flatten();
    }

    let db = db?;
    let conn = db.read_conn().ok()?;
    if let Ok(protection) = conn.query_row(
        "SELECT s.active_dispatch_id, s.status, td.status
         FROM sessions s
         JOIN task_dispatches td
           ON td.id = s.active_dispatch_id
         WHERE s.active_dispatch_id IS NOT NULL
           AND s.status != 'disconnected'
           AND (
             s.session_key = ?1
             OR s.session_key = ?2
             OR (
               ?3 != ''
               AND s.thread_channel_id = ?3
               AND s.provider = ?4
               AND s.session_key LIKE ?5
             )
           )
         ORDER BY
           CASE s.status
             WHEN 'turn_active' THEN 0 WHEN 'working' THEN 0 WHEN 'awaiting_bg' THEN 1
             WHEN 'idle' THEN 1
             ELSE 2
           END,
           datetime(COALESCE(s.last_heartbeat, s.created_at)) DESC,
           s.rowid DESC
         LIMIT 1",
        [
            session_keys[0].as_str(),
            session_keys[1].as_str(),
            thread_channel_id.as_str(),
            provider.as_str(),
            namespaced_session_key_prefix.as_str(),
        ],
        |row| {
            Ok(DispatchTmuxProtection::SessionRow {
                dispatch_id: row.get::<_, String>(0)?,
                session_status: row.get::<_, String>(1)?,
                dispatch_status: row.get::<_, String>(2)?,
            })
        },
    ) {
        return Some(protection);
    }

    if thread_channel_id.is_empty() {
        return None;
    }
    conn.query_row(
        "SELECT td.id, td.status
         FROM task_dispatches td
         WHERE td.thread_id = ?1
           AND td.status IN ('pending', 'dispatched')
           AND EXISTS (
             SELECT 1
             FROM sessions s
             WHERE s.thread_channel_id = ?1
               AND s.provider = ?2
               AND s.session_key LIKE ?3
           )
         ORDER BY
           CASE td.status
             WHEN 'dispatched' THEN 0
             WHEN 'pending' THEN 1
             ELSE 2
           END,
           datetime(td.created_at) DESC,
           td.rowid DESC
         LIMIT 1",
        [
            thread_channel_id.as_str(),
            provider.as_str(),
            namespaced_session_key_prefix.as_str(),
        ],
        |row| {
            Ok(DispatchTmuxProtection::ThreadDispatch {
                dispatch_id: row.get::<_, String>(0)?,
                dispatch_status: row.get::<_, String>(1)?,
            })
        },
    )
    .ok()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::{DispatchTmuxProtection, resolve_dispatch_tmux_protection};
    use crate::engine::PolicyEngine;
    use crate::server::routes::AppState;
    use crate::services::provider::ProviderKind;
    use axum::{Router, routing::patch};
    use sqlx::Row;
    use std::sync::OnceLock;

    static TMUX_LIFECYCLE_PG_TEST_LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

    fn sample_tmux_name() -> String {
        ProviderKind::Codex.build_tmux_session_name("adk-cdx-t1485506232256168011")
    }

    fn test_engine_with_pg(pg_pool: sqlx::PgPool) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies");
        config.policies.hot_reload = false;
        PolicyEngine::new_with_pg(&config, Some(pg_pool)).unwrap()
    }

    struct TmuxLifecyclePgDatabase {
        _lifecycle: crate::db::postgres::PostgresTestLifecycleGuard,
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TmuxLifecyclePgDatabase {
        async fn create_or_skip() -> Option<Self> {
            let lifecycle = crate::db::postgres::lock_test_lifecycle();
            let admin_url = pg_test_admin_database_url();
            let database_name =
                format!("agentdesk_tmux_lifecycle_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", pg_test_base_database_url(), database_name);
            match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                crate::db::postgres::create_test_database(
                    &admin_url,
                    &database_name,
                    "tmux lifecycle pg",
                ),
            )
            .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) if postgres_unavailable_for_test(&error) => {
                    eprintln!(
                        "skipping tmux lifecycle PG test because Postgres is unavailable: {error}"
                    );
                    return None;
                }
                Ok(Err(error)) => panic!("create tmux lifecycle postgres test db: {error}"),
                Err(_) => {
                    eprintln!(
                        "skipping tmux lifecycle PG test because Postgres admin connection timed out"
                    );
                    return None;
                }
            }

            Some(Self {
                _lifecycle: lifecycle,
                admin_url,
                database_name,
                database_url,
            })
        }

        async fn migrate(&self) -> sqlx::PgPool {
            crate::db::postgres::connect_test_pool_and_migrate(
                &self.database_url,
                "tmux lifecycle pg",
            )
            .await
            .expect("connect + migrate tmux lifecycle postgres test db")
        }

        async fn drop(self) {
            crate::db::postgres::drop_test_database(
                &self.admin_url,
                &self.database_name,
                "tmux lifecycle pg",
            )
            .await
            .expect("drop tmux lifecycle postgres test db");
        }
    }

    fn pg_test_base_database_url() -> String {
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

    fn pg_test_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", pg_test_base_database_url(), admin_db)
    }

    fn postgres_unavailable_for_test(error: &str) -> bool {
        error.contains("pool timed out")
            || error.contains("Connection refused")
            || error.contains("connection refused")
            || error.contains("could not connect")
            || error.contains("No such file or directory")
    }

    async fn tmux_lifecycle_pg_test_guard() -> tokio::sync::MutexGuard<'static, ()> {
        TMUX_LIFECYCLE_PG_TEST_LOCK
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await
    }

    async fn spawn_dispatch_update_api(state: AppState) -> (u16, tokio::task::JoinHandle<()>) {
        let app = Router::new()
            .route(
                "/api/dispatches/{id}",
                patch(crate::server::routes::dispatches::update_dispatch),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (port, handle)
    }

    async fn seed_pg_dispatch_session(
        pool: &sqlx::PgPool,
        dispatch_id: &str,
        dispatch_status: &str,
    ) {
        let provider = ProviderKind::Codex;
        let tmux_name = sample_tmux_name();
        let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
            "tokenxyz", &provider, &tmux_name,
        );
        sqlx::query(
            "INSERT INTO agents (id, name, discord_channel_id, discord_channel_alt)
             VALUES ('agent-tmux-life', 'Tmux Lifecycle Agent', '111', '222')
             ON CONFLICT (id) DO NOTHING",
        )
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO kanban_cards
             (id, title, status, assigned_agent_id, created_at, updated_at)
             VALUES ($1, $2, 'in_progress', 'agent-tmux-life', NOW(), NOW())",
        )
        .bind(format!("card-{dispatch_id}"))
        .bind(format!("Card {dispatch_id}"))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO task_dispatches
             (id, kanban_card_id, to_agent_id, dispatch_type, status, title, created_at, updated_at)
             VALUES ($1, $2, 'agent-tmux-life', 'implementation', $3, $4, NOW(), NOW())",
        )
        .bind(dispatch_id)
        .bind(format!("card-{dispatch_id}"))
        .bind(dispatch_status)
        .bind(format!("Dispatch {dispatch_id}"))
        .execute(pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO sessions
             (session_key, agent_id, provider, status, active_dispatch_id, thread_channel_id, created_at, last_heartbeat)
             VALUES ($1, 'agent-tmux-life', $2, 'idle', $3, '1485506232256168011', NOW(), NOW())",
        )
        .bind(session_key)
        .bind(provider.as_str())
        .bind(dispatch_id)
        .execute(pool)
        .await
        .unwrap();
    }

    fn fresh_dispatch_lookup_db() -> crate::db::Db {
        let db = crate::db::test_db();
        db.lock()
            .unwrap()
            .execute_batch(
                "
                DROP TABLE IF EXISTS sessions;
                DROP TABLE IF EXISTS task_dispatches;
                CREATE TABLE sessions (
                    session_key TEXT,
                    provider TEXT,
                    status TEXT,
                    active_dispatch_id TEXT,
                    created_at TEXT,
                    last_heartbeat TEXT,
                    thread_channel_id TEXT
                );
                CREATE TABLE task_dispatches (
                    id TEXT PRIMARY KEY,
                    status TEXT,
                    thread_id TEXT,
                    created_at TEXT
                );
                ",
            )
            .unwrap();
        db
    }

    #[test]
    fn protects_session_rows_with_active_dispatch_even_when_idle() {
        let db = fresh_dispatch_lookup_db();
        let conn = db.lock().unwrap();

        let provider = ProviderKind::Codex;
        let tmux_name = sample_tmux_name();
        let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
            "tokenxyz", &provider, &tmux_name,
        );
        conn.execute(
            "INSERT INTO task_dispatches (id, status, thread_id, created_at)
             VALUES ('dispatch-495', 'dispatched', '1485506232256168011', datetime('now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO sessions
             (session_key, provider, status, active_dispatch_id, created_at, last_heartbeat, thread_channel_id)
             VALUES (?1, ?2, 'idle', 'dispatch-495', datetime('now'), datetime('now'), '1485506232256168011')",
            [session_key.as_str(), provider.as_str()],
        )
        .unwrap();
        drop(conn);

        let protection = resolve_dispatch_tmux_protection(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            Some("adk-cdx-t1485506232256168011"),
        );

        assert_eq!(
            protection,
            Some(DispatchTmuxProtection::SessionRow {
                dispatch_id: "dispatch-495".to_string(),
                session_status: "idle".to_string(),
                dispatch_status: "dispatched".to_string(),
            })
        );
    }

    #[test]
    fn ignores_thread_dispatches_when_current_namespace_session_row_is_missing() {
        let db = fresh_dispatch_lookup_db();
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "
            INSERT INTO task_dispatches (id, status, thread_id, created_at)
            VALUES
                ('dispatch-pending', 'pending', '1485506232256168011', '2026-04-14 01:00:00'),
                ('dispatch-dispatched', 'dispatched', '1485506232256168011', '2026-04-14 01:01:00');
            ",
        )
        .unwrap();
        drop(conn);

        let provider = ProviderKind::Codex;
        let tmux_name = sample_tmux_name();
        let protection = resolve_dispatch_tmux_protection(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            None,
        );

        assert_eq!(protection, None);
    }

    #[test]
    fn ignores_completed_dispatches_without_active_session_rows() {
        let db = fresh_dispatch_lookup_db();
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "
            INSERT INTO task_dispatches (id, status, thread_id, created_at)
            VALUES ('dispatch-done', 'completed', '1485506232256168011', '2026-04-14 01:01:00');
            ",
        )
        .unwrap();
        drop(conn);

        let provider = ProviderKind::Codex;
        let tmux_name = sample_tmux_name();
        let protection = resolve_dispatch_tmux_protection(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            None,
        );

        assert_eq!(protection, None);
    }

    #[test]
    fn protects_session_rows_with_completed_dispatch_ids_until_ttl() {
        let db = fresh_dispatch_lookup_db();
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "
            INSERT INTO task_dispatches (id, status, thread_id, created_at)
            VALUES ('dispatch-stale', 'completed', '1485506232256168011', '2026-04-14 01:01:00');
            ",
        )
        .unwrap();

        let provider = ProviderKind::Codex;
        let tmux_name = sample_tmux_name();
        let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
            "tokenxyz", &provider, &tmux_name,
        );
        conn.execute(
            "INSERT INTO sessions
             (session_key, provider, status, active_dispatch_id, created_at, last_heartbeat, thread_channel_id)
             VALUES (?1, ?2, 'idle', 'dispatch-stale', datetime('now'), datetime('now'), '1485506232256168011')",
            [session_key.as_str(), provider.as_str()],
        )
        .unwrap();
        drop(conn);

        let protection = resolve_dispatch_tmux_protection(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            Some("adk-cdx-t1485506232256168011"),
        );

        assert_eq!(
            protection,
            Some(DispatchTmuxProtection::SessionRow {
                dispatch_id: "dispatch-stale".to_string(),
                session_status: "idle".to_string(),
                dispatch_status: "completed".to_string(),
            })
        );
    }

    #[test]
    fn ignores_session_rows_with_missing_dispatch_ids() {
        let db = fresh_dispatch_lookup_db();
        let conn = db.lock().unwrap();

        let provider = ProviderKind::Codex;
        let tmux_name = sample_tmux_name();
        let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
            "tokenxyz", &provider, &tmux_name,
        );
        conn.execute(
            "INSERT INTO sessions
             (session_key, provider, status, active_dispatch_id, created_at, last_heartbeat, thread_channel_id)
             VALUES (?1, ?2, 'idle', 'dispatch-missing', datetime('now'), datetime('now'), '1485506232256168011')",
            [session_key.as_str(), provider.as_str()],
        )
        .unwrap();
        drop(conn);

        let protection = resolve_dispatch_tmux_protection(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            Some("adk-cdx-t1485506232256168011"),
        );

        assert_eq!(protection, None);
    }

    #[test]
    fn ignores_thread_channel_session_rows_from_other_provider() {
        let db = fresh_dispatch_lookup_db();
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "
            INSERT INTO task_dispatches (id, status, thread_id, created_at)
            VALUES ('dispatch-other-provider', 'dispatched', 'other-thread', '2026-04-14 01:01:00');
            ",
        )
        .unwrap();

        let provider = ProviderKind::Codex;
        let tmux_name = sample_tmux_name();
        conn.execute(
            "INSERT INTO sessions
             (session_key, provider, status, active_dispatch_id, created_at, last_heartbeat, thread_channel_id)
             VALUES ('foreign-session', 'claude', 'idle', 'dispatch-other-provider', datetime('now'), datetime('now'), '1485506232256168011')",
            [],
        )
        .unwrap();
        drop(conn);

        let protection = resolve_dispatch_tmux_protection(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            Some("adk-cdx-t1485506232256168011"),
        );

        assert_eq!(protection, None);
    }

    #[test]
    fn ignores_thread_channel_session_rows_from_other_token_namespace() {
        let db = fresh_dispatch_lookup_db();
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "
            INSERT INTO task_dispatches (id, status, thread_id, created_at)
            VALUES ('dispatch-other-token', 'completed', '1485506232256168011', '2026-04-14 01:01:00');
            ",
        )
        .unwrap();

        let provider = ProviderKind::Codex;
        let tmux_name = sample_tmux_name();
        let foreign_session_key =
            crate::services::discord::adk_session::build_namespaced_session_key(
                "othertoken",
                &provider,
                &ProviderKind::Codex.build_tmux_session_name("adk-cdx-shadow-t1485506232256168011"),
            );
        conn.execute(
            "INSERT INTO sessions
             (session_key, provider, status, active_dispatch_id, created_at, last_heartbeat, thread_channel_id)
             VALUES (?1, ?2, 'idle', 'dispatch-other-token', datetime('now'), datetime('now'), '1485506232256168011')",
            [foreign_session_key.as_str(), provider.as_str()],
        )
        .unwrap();
        drop(conn);

        let protection = resolve_dispatch_tmux_protection(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            Some("adk-cdx-t1485506232256168011"),
        );

        assert_eq!(protection, None);
    }

    #[test]
    fn ignores_thread_dispatches_from_other_token_namespace() {
        let db = fresh_dispatch_lookup_db();
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "
            INSERT INTO task_dispatches (id, status, thread_id, created_at)
            VALUES ('dispatch-other-runtime', 'dispatched', '1485506232256168011', '2026-04-14 01:01:00');
            ",
        )
        .unwrap();

        let provider = ProviderKind::Codex;
        let tmux_name = sample_tmux_name();
        let foreign_session_key =
            crate::services::discord::adk_session::build_namespaced_session_key(
                "othertoken",
                &provider,
                &ProviderKind::Codex.build_tmux_session_name("adk-cdx-shadow-t1485506232256168011"),
            );
        conn.execute(
            "INSERT INTO sessions
             (session_key, provider, status, active_dispatch_id, created_at, last_heartbeat, thread_channel_id)
             VALUES (?1, ?2, 'idle', NULL, datetime('now'), datetime('now'), '1485506232256168011')",
            [foreign_session_key.as_str(), provider.as_str()],
        )
        .unwrap();
        drop(conn);

        let protection = resolve_dispatch_tmux_protection(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            Some("adk-cdx-t1485506232256168011"),
        );

        assert_eq!(protection, None);
    }

    #[test]
    fn protects_thread_dispatches_when_current_namespace_owns_thread() {
        let db = fresh_dispatch_lookup_db();
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "
            INSERT INTO task_dispatches (id, status, thread_id, created_at)
            VALUES
                ('dispatch-pending', 'pending', '1485506232256168011', '2026-04-14 01:00:00'),
                ('dispatch-dispatched', 'dispatched', '1485506232256168011', '2026-04-14 01:01:00');
            ",
        )
        .unwrap();

        let provider = ProviderKind::Codex;
        let tmux_name = sample_tmux_name();
        let sidecar_session_key =
            crate::services::discord::adk_session::build_namespaced_session_key(
                "tokenxyz",
                &provider,
                &ProviderKind::Codex.build_tmux_session_name("adk-cdx-shadow-t1485506232256168011"),
            );
        conn.execute(
            "INSERT INTO sessions
             (session_key, provider, status, active_dispatch_id, created_at, last_heartbeat, thread_channel_id)
             VALUES (?1, ?2, 'idle', NULL, datetime('now'), datetime('now'), '1485506232256168011')",
            [sidecar_session_key.as_str(), provider.as_str()],
        )
        .unwrap();
        drop(conn);

        let protection = resolve_dispatch_tmux_protection(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            Some("adk-cdx-t1485506232256168011"),
        );

        assert_eq!(
            protection,
            Some(DispatchTmuxProtection::ThreadDispatch {
                dispatch_id: "dispatch-dispatched".to_string(),
                dispatch_status: "dispatched".to_string(),
            })
        );
    }

    #[test]
    fn protects_thread_channel_session_rows_with_same_token_namespace() {
        let db = fresh_dispatch_lookup_db();
        let conn = db.lock().unwrap();
        conn.execute_batch(
            "
            INSERT INTO task_dispatches (id, status, thread_id, created_at)
            VALUES ('dispatch-same-token', 'dispatched', '1485506232256168011', '2026-04-14 01:01:00');
            ",
        )
        .unwrap();

        let provider = ProviderKind::Codex;
        let tmux_name = sample_tmux_name();
        let namespaced_sidecar_key =
            crate::services::discord::adk_session::build_namespaced_session_key(
                "tokenxyz",
                &provider,
                &ProviderKind::Codex.build_tmux_session_name("adk-cdx-shadow-t1485506232256168011"),
            );
        conn.execute(
            "INSERT INTO sessions
             (session_key, provider, status, active_dispatch_id, created_at, last_heartbeat, thread_channel_id)
             VALUES (?1, ?2, 'idle', 'dispatch-same-token', datetime('now'), datetime('now'), '1485506232256168011')",
            [namespaced_sidecar_key.as_str(), provider.as_str()],
        )
        .unwrap();
        drop(conn);

        let protection = resolve_dispatch_tmux_protection(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            Some("adk-cdx-t1485506232256168011"),
        );

        assert_eq!(
            protection,
            Some(DispatchTmuxProtection::SessionRow {
                dispatch_id: "dispatch-same-token".to_string(),
                session_status: "idle".to_string(),
                dispatch_status: "dispatched".to_string(),
            })
        );
    }

    #[test]
    fn shared_db_wrapper_preserves_dispatch_session_lookup() {
        let db = crate::db::test_db();
        let provider = ProviderKind::Codex;
        let tmux_name = sample_tmux_name();
        let session_key = crate::services::discord::adk_session::build_namespaced_session_key(
            "tokenxyz", &provider, &tmux_name,
        );

        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO task_dispatches (id, status, thread_id, created_at, updated_at, dispatch_type, title)
                 VALUES (?1, 'dispatched', ?2, datetime('now'), datetime('now'), 'implementation', 'active dispatch')",
                ["dispatch-db-wrapper", "1485506232256168011"],
            )
            .unwrap();
        db.lock()
            .unwrap()
            .execute(
                "INSERT INTO sessions
                 (session_key, provider, status, active_dispatch_id, thread_channel_id, created_at, last_heartbeat)
                 VALUES (?1, ?2, 'idle', 'dispatch-db-wrapper', '1485506232256168011', datetime('now'), datetime('now'))",
                [session_key.as_str(), provider.as_str()],
            )
            .unwrap();

        let protection = super::resolve_dispatch_tmux_protection(
            Some(&db),
            None,
            "tokenxyz",
            &provider,
            &tmux_name,
            Some("adk-cdx-t1485506232256168011"),
        );

        assert_eq!(
            protection,
            Some(DispatchTmuxProtection::SessionRow {
                dispatch_id: "dispatch-db-wrapper".to_string(),
                session_status: "idle".to_string(),
                dispatch_status: "dispatched".to_string(),
            })
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dead_tmux_session_simulation_marks_dispatched_dispatch_failed_pg() {
        let _guard = tmux_lifecycle_pg_test_guard().await;
        let Some(pg_db) = TmuxLifecyclePgDatabase::create_or_skip().await else {
            return;
        };
        let pool = pg_db.migrate().await;
        let state = AppState::test_state_with_pg(
            crate::db::test_db(),
            test_engine_with_pg(pool.clone()),
            pool.clone(),
        );
        let (api_port, api_handle) = spawn_dispatch_update_api(state).await;
        super::super::internal_api::init(api_port, Some(pool.clone()));

        seed_pg_dispatch_session(&pool, "dispatch-tmux-died-active", "dispatched").await;
        let provider = ProviderKind::Codex;
        let tmux_name = sample_tmux_name();
        let protection = resolve_dispatch_tmux_protection(
            None::<&crate::db::Db>,
            Some(&pool),
            "tokenxyz",
            &provider,
            &tmux_name,
            Some("adk-cdx-t1485506232256168011"),
        )
        .expect("active dispatch session should be protected");

        assert!(
            super::fail_active_dispatch_for_dead_tmux_session(
                api_port,
                &protection,
                &tmux_name,
                "test_kill_simulation",
            )
            .await,
            "dead tmux session should trigger the dispatch failure path",
        );

        let row = sqlx::query(
            "SELECT status, result
             FROM task_dispatches
             WHERE id = 'dispatch-tmux-died-active'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let status: String = row.try_get("status").unwrap();
        let result: serde_json::Value = row.try_get("result").unwrap();
        assert_eq!(status, "failed");
        assert_eq!(
            result.get("error").and_then(|value| value.as_str()),
            Some("tmux_session_died")
        );
        assert!(
            result
                .get("message")
                .and_then(|value| value.as_str())
                .is_some_and(|message| message.contains("test_kill_simulation")),
            "failure result should keep the tmux death source: {result}",
        );
        let failed_events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM dispatch_events
             WHERE dispatch_id = 'dispatch-tmux-died-active'
               AND to_status = 'failed'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(failed_events, 1);

        api_handle.abort();
        pool.close().await;
        pg_db.drop().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dead_tmux_session_simulation_does_not_overwrite_completed_race_pg() {
        let _guard = tmux_lifecycle_pg_test_guard().await;
        let Some(pg_db) = TmuxLifecyclePgDatabase::create_or_skip().await else {
            return;
        };
        let pool = pg_db.migrate().await;
        let state = AppState::test_state_with_pg(
            crate::db::test_db(),
            test_engine_with_pg(pool.clone()),
            pool.clone(),
        );
        let (api_port, api_handle) = spawn_dispatch_update_api(state).await;
        super::super::internal_api::init(api_port, Some(pool.clone()));

        seed_pg_dispatch_session(&pool, "dispatch-tmux-died-race", "dispatched").await;
        let provider = ProviderKind::Codex;
        let tmux_name = sample_tmux_name();
        let stale_protection = resolve_dispatch_tmux_protection(
            None::<&crate::db::Db>,
            Some(&pool),
            "tokenxyz",
            &provider,
            &tmux_name,
            Some("adk-cdx-t1485506232256168011"),
        )
        .expect("dispatched session should be protected before the race");
        sqlx::query(
            "UPDATE task_dispatches
             SET status = 'completed',
                 result = '{\"summary\":\"completed before tmux watcher cleanup\"}'::jsonb,
                 completed_at = NOW(),
                 updated_at = NOW()
             WHERE id = 'dispatch-tmux-died-race'",
        )
        .execute(&pool)
        .await
        .unwrap();

        assert!(
            super::fail_active_dispatch_for_dead_tmux_session(
                api_port,
                &stale_protection,
                &tmux_name,
                "test_kill_simulation_race",
            )
            .await,
            "stale protection still attempts the tmux death failure path",
        );

        let row = sqlx::query(
            "SELECT status, result
             FROM task_dispatches
             WHERE id = 'dispatch-tmux-died-race'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let status: String = row.try_get("status").unwrap();
        let result: serde_json::Value = row.try_get("result").unwrap();
        assert_eq!(status, "completed");
        assert_eq!(
            result.get("summary").and_then(|value| value.as_str()),
            Some("completed before tmux watcher cleanup")
        );
        let failed_events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*)
             FROM dispatch_events
             WHERE dispatch_id = 'dispatch-tmux-died-race'
               AND to_status = 'failed'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(failed_events, 0);

        api_handle.abort();
        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn active_dispatch_id_only_returns_retryable_statuses() {
        let active = DispatchTmuxProtection::SessionRow {
            dispatch_id: "dispatch-active".to_string(),
            session_status: "idle".to_string(),
            dispatch_status: "dispatched".to_string(),
        };
        let completed = DispatchTmuxProtection::SessionRow {
            dispatch_id: "dispatch-completed".to_string(),
            session_status: "idle".to_string(),
            dispatch_status: "completed".to_string(),
        };

        assert_eq!(active.active_dispatch_id(), Some("dispatch-active"));
        assert_eq!(completed.active_dispatch_id(), None);
    }
}
