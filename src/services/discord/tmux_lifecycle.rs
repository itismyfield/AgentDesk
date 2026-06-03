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

    resolve_dispatch_tmux_protection_legacy_fallback(
        db,
        provider,
        &thread_channel_id,
        &session_keys,
        &namespaced_session_key_prefix,
    )
}

// Production runs PostgreSQL-only (#3035 Phase 0): the legacy sqlite handle is
// always `None`, so after the PG path above prod has no DB fallback and returns
// `None` — preserving the historical `let db = db?;` short-circuit semantics.
fn resolve_dispatch_tmux_protection_legacy_fallback(
    _db: Option<&crate::db::Db>,
    _provider: &ProviderKind,
    _thread_channel_id: &str,
    _session_keys: &[String; 2],
    _namespaced_session_key_prefix: &str,
) -> Option<DispatchTmuxProtection> {
    None
}
