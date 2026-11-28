//! The session reads and writes of a session force-kill.

use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};

use futures::future::BoxFuture;
use serde_json::json;

use super::{RetryDispatchMeta, resolve_session_locator_pg};
use crate::db::agents::resolve_agent_channel_for_provider_pg;
use crate::db::session_agent_resolution::normalize_thread_channel_id;

/// The canonical dispatch status transition (`dispatch::set_dispatch_status_on_pg_async`); the
/// dispatch service owns it, so the force-kill caller hands it in.
pub(crate) type SetDispatchStatus = for<'a> fn(
    &'a PgPool,
    &'a str,
    &'a str,
    Option<&'a serde_json::Value>,
    &'a str,
    Option<&'a [&'a str]>,
    bool,
) -> BoxFuture<'a, anyhow::Result<usize>>;

/// Active dispatch, agent, runtime channel, stored provider and owner instance of a session.
type ForceKillSession = (
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// The row a primary key or alias names, with the row's current primary key: every later read,
/// write and tmux name must follow that key, never the tail of the key that was asked for.
pub async fn load_force_kill_session_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<Option<(String, ForceKillSession)>, String> {
    let selected = select(pool, session_key, |row| row.try_get("session_key")).await?;
    Ok(selected.map(|(session, current)| (current, session)))
}

/// The selected row's own key, identity columns (provider, identity kind, token hash, channel)
/// and hosted execution record, as the same read returned them.
#[cfg(unix)]
pub struct ForceKillHosted {
    pub session_key: String,
    pub identity: [Option<String>; 4],
    pub hosted: Option<serde_json::Value>,
}

/// [`load_force_kill_session_pg`]'s read with the selected row's hosted record.
#[cfg(unix)]
pub async fn load_force_kill_session_hosted_pg(
    pool: &PgPool,
    session_key: &str,
) -> Result<Option<(ForceKillSession, ForceKillHosted)>, String> {
    select(pool, session_key, |row| {
        Ok(ForceKillHosted {
            session_key: row.try_get("session_key")?,
            identity: [
                row.try_get("provider")?,
                row.try_get("identity_kind")?,
                row.try_get("discord_token_hash")?,
                row.try_get("channel_id")?,
            ],
            hosted: row.try_get("hosted_execution")?,
        })
    })
    .await
}

async fn select<R>(
    pool: &PgPool,
    session_key: &str,
    read: impl Fn(&PgRow) -> Result<R, sqlx::Error>,
) -> Result<Option<(ForceKillSession, R)>, String> {
    let resolved_session_key =
        resolve_session_locator_pg(pool, session_key, "force-kill session").await?;
    let Some(resolved_session_key) = resolved_session_key else {
        return Ok(None);
    };
    let row = sqlx::query(
        "SELECT active_dispatch_id, agent_id, thread_channel_id, provider, instance_id,
                session_key, identity_kind, discord_token_hash, channel_id, hosted_execution
         FROM sessions
         WHERE session_key = $1",
    )
    .bind(&resolved_session_key)
    .fetch_optional(pool)
    .await
    .map_err(|error| {
        format!(
            "load postgres session {session_key}: {}",
            crate::utils::redact::redact_known_secrets(&error.to_string())
        )
    })?;

    let Some(row) = row else {
        return Ok(None);
    };

    let active_dispatch_id: Option<String> = row
        .try_get("active_dispatch_id")
        .map_err(|error| format!("decode active_dispatch_id for {session_key}: {error}"))?;
    let agent_id: Option<String> = row
        .try_get("agent_id")
        .map_err(|error| format!("decode agent_id for {session_key}: {error}"))?;
    let thread_channel_id: Option<String> = row
        .try_get("thread_channel_id")
        .map_err(|error| format!("decode thread_channel_id for {session_key}: {error}"))?;
    let session_provider: Option<String> = row
        .try_get("provider")
        .map_err(|error| format!("decode provider for {session_key}: {error}"))?;
    let instance_id: Option<String> = row
        .try_get("instance_id")
        .map_err(|error| format!("decode instance_id for {session_key}: {error}"))?;
    let read = read(&row).map_err(|error| format!("decode row for {session_key}: {error}"))?;

    // The row's own provider, else what its current key records; the request names neither.
    let key_provider = crate::domain::session_identity::naming::recorded_provider(
        None,
        Some(&resolved_session_key),
    );
    let effective_provider = (session_provider.as_deref())
        .filter(|stored| !stored.trim().is_empty())
        .or(key_provider.as_ref().map(|provider| provider.as_str()));
    let runtime_channel_id =
        if let Some(channel_id) = normalize_thread_channel_id(thread_channel_id.as_deref()) {
            Some(channel_id)
        } else if let Some(agent_id) = agent_id.as_deref() {
            resolve_agent_channel_for_provider_pg(pool, agent_id, effective_provider)
            .await
            .map_err(|error| {
                format!(
                    "resolve postgres channel for session {session_key} / agent {agent_id}: {error}"
                )
            })?
            .and_then(|channel| normalize_thread_channel_id(Some(channel.as_str())))
        } else {
            None
        };

    let session = (
        active_dispatch_id,
        agent_id,
        runtime_channel_id,
        session_provider,
        instance_id,
    );
    Ok(Some((session, read)))
}

pub async fn disconnect_session_and_prepare_retry_pg(
    pool: &PgPool,
    session_key: &str,
    active_dispatch_id: Option<&str>,
    retry: bool,
    set_status: SetDispatchStatus,
) -> Result<Option<RetryDispatchMeta>, String> {
    // Session disconnect is independently durable. Dispatch failure still
    // goes through the canonical pipeline, either standalone or in the retry
    // transaction, and terminal dispatches remain authoritative.
    {
        let mut tx = pool
            .begin()
            .await
            .map_err(|error| format!("begin postgres force-kill transaction: {error}"))?;

        sqlx::query(
            "UPDATE sessions
             SET status = 'disconnected',
                 active_dispatch_id = NULL
             WHERE session_key = $1",
        )
        .bind(session_key)
        .execute(&mut *tx)
        .await
        .map_err(|error| format!("disconnect postgres session {session_key}: {error}"))?;

        tx.commit()
            .await
            .map_err(|error| format!("commit postgres force-kill transaction: {error}"))?;
    }
    prepare_force_kill_retry_pg(pool, session_key, active_dispatch_id, retry, set_status).await
}

/// Fails the force-killed session's dispatch or, with `retry`, reads what its retry copies; a
/// terminal dispatch is never rewritten. The session row is not touched here.
pub async fn prepare_force_kill_retry_pg(
    pool: &PgPool,
    session_key: &str,
    active_dispatch_id: Option<&str>,
    retry: bool,
    set_status: SetDispatchStatus,
) -> Result<Option<RetryDispatchMeta>, String> {
    let mut retry_meta = None;
    if let Some(dispatch_id) = active_dispatch_id {
        let current_status = sqlx::query_scalar::<_, Option<String>>(
            "SELECT status
             FROM task_dispatches
             WHERE id = $1",
        )
        .bind(dispatch_id)
        .fetch_optional(pool)
        .await
        .map_err(|error| format!("load postgres dispatch status {dispatch_id}: {error}"))?
        .flatten();

        let current_status_str = current_status.as_deref();
        // Terminal states: never overwrite. `cancelled` in particular must
        // remain `cancelled`; rewriting it to `failed` would corrupt
        // incident metrics (Finding 4).
        let is_terminal = matches!(
            current_status_str,
            Some("completed") | Some("cancelled") | Some("failed")
        );

        if retry && !is_terminal {
            retry_meta = sqlx::query(
                "SELECT
                    kanban_card_id,
                    to_agent_id,
                    dispatch_type,
                    title,
                    context,
                    COALESCE(retry_count, 0)::BIGINT AS retry_count
                 FROM task_dispatches
                 WHERE id = $1",
            )
            .bind(dispatch_id)
            .fetch_optional(pool)
            .await
            .map_err(|error| format!("load postgres retry metadata {dispatch_id}: {error}"))?
            .map(|row| {
                Ok(RetryDispatchMeta {
                    origin_dispatch_id: dispatch_id.to_string(),
                    card_id: row.try_get("kanban_card_id")?,
                    to_agent_id: row.try_get("to_agent_id")?,
                    dispatch_type: row.try_get("dispatch_type")?,
                    title: row.try_get("title")?,
                    context: row.try_get("context")?,
                    retry_count: row.try_get("retry_count")?,
                })
            })
            .transpose()
            .map_err(|error: sqlx::Error| {
                format!("decode postgres retry metadata {dispatch_id}: {error}")
            })?;
        } else if !is_terminal {
            let changed = fail_force_killed_dispatch_without_retry_pg(
                pool,
                dispatch_id,
                Some(session_key),
                set_status,
            )
            .await?;
            if changed == 0 {
                return Ok(None);
            }
        }

        // Retry-capable force-kill defers d1 failure to the replacement
        // transaction. If that transaction dies before commit, d1 failure is
        // rolled back too; the session disconnect above is already committed.
        // A normal returned error is handled by the standalone fallback.
    }

    Ok(retry_meta)
}

pub async fn fail_force_killed_dispatch_without_retry_pg(
    pool: &PgPool,
    dispatch_id: &str,
    session_key: Option<&str>,
    set_status: SetDispatchStatus,
) -> Result<usize, String> {
    let reason = json!({
        "reason": "force_kill_session",
        "session_key": session_key,
    });
    set_status(
        pool,
        dispatch_id,
        "failed",
        Some(&reason),
        "force_kill_session",
        Some(&["pending", "dispatched"]),
        true,
    )
    .await
    .map_err(|error| {
        format!("canonical fail postgres dispatch {dispatch_id} during force-kill: {error}")
    })
}
