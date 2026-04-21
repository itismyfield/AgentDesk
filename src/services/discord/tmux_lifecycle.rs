use crate::services::provider::{ProviderKind, parse_provider_and_channel_from_tmux_name};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DispatchTmuxProtection {
    SessionRow {
        dispatch_id: String,
        session_status: String,
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
            } => format!(
                "session row keeps active_dispatch_id={dispatch_id} (status={session_status})"
            ),
            Self::ThreadDispatch {
                dispatch_id,
                dispatch_status,
            } => {
                format!("thread dispatch {dispatch_id} is still active (status={dispatch_status})")
            }
        }
    }
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
                if let Some((dispatch_id, session_status)) = sqlx::query_as::<_, (String, String)>(
                    "SELECT s.active_dispatch_id, s.status
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
                         WHEN 'working' THEN 0
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
        "SELECT s.active_dispatch_id, s.status
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
             WHEN 'working' THEN 0
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

#[cfg(test)]
mod tests {
    use super::{DispatchTmuxProtection, resolve_dispatch_tmux_protection};
    use crate::services::provider::ProviderKind;

    fn sample_tmux_name() -> String {
        ProviderKind::Codex.build_tmux_session_name("adk-cdx-t1485506232256168011")
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
            })
        );
    }
}
