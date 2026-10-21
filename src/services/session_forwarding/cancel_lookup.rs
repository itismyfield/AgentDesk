//! The session an HTTP turn cancel targets: the one active turn row the channel names.

use sqlx::{PgPool, Row};

use super::CancelTurnSessionInfo;
use crate::services::service_error::{ErrorCode, ServiceError, ServiceResult};

pub(crate) async fn load_cancel_turn_session(
    pool: &PgPool,
    channel_id: &str,
) -> ServiceResult<Option<CancelTurnSessionInfo>> {
    Ok(select(pool, channel_id).await?.map(|(info, _)| info))
}

/// The selected row's identity columns (provider, identity kind, token hash, channel) and hosted
/// execution record, as the same read returned them.
#[derive(Debug)]
pub(crate) struct SelectedRow {
    pub(crate) identity: [Option<String>; 4],
    pub(crate) hosted: Option<serde_json::Value>,
}

/// [`load_cancel_turn_session`]'s selection with the chosen row as it was read.
pub(crate) async fn load_cancel_turn_hosted(
    pool: &PgPool,
    channel_id: &str,
) -> ServiceResult<Option<(CancelTurnSessionInfo, SelectedRow)>> {
    select(pool, channel_id).await
}

async fn select(
    pool: &PgPool,
    channel_id: &str,
) -> ServiceResult<Option<(CancelTurnSessionInfo, SelectedRow)>> {
    if channel_id.is_empty() || !channel_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(
            ServiceError::bad_request("channel_id must contain only decimal digits")
                .with_context("channel_id", channel_id),
        );
    }

    let rows = sqlx::query(
        "WITH channel_agent AS (
           SELECT id AS agent_id,
                  CASE
                    WHEN discord_channel_cc = $1 OR discord_channel_id = $1 THEN 'claude'
                    WHEN discord_channel_cdx = $1 OR discord_channel_alt = $1 THEN 'codex'
                    ELSE NULL
                  END AS requested_provider
           FROM agents
           WHERE discord_channel_id = $1
              OR discord_channel_alt = $1
              OR discord_channel_cc = $1
              OR discord_channel_cdx = $1
           LIMIT 1
         )
         SELECT s.session_key,
                s.active_dispatch_id,
                s.provider,
                s.agent_id,
                ca.requested_provider,
                s.instance_id,
                s.thread_channel_id,
                s.channel_id,
                s.last_heartbeat,
                s.identity_kind,
                s.discord_token_hash,
                s.hosted_execution
         FROM sessions s
         LEFT JOIN channel_agent ca ON s.agent_id = ca.agent_id
         WHERE s.status = 'turn_active'
           AND (
             COALESCE(s.thread_channel_id, '') = $1
             OR COALESCE(s.channel_id, '') = $1
             OR (s.thread_channel_id IS NULL AND s.channel_id IS NULL)
             OR ca.agent_id IS NOT NULL
           )
         ORDER BY s.last_heartbeat DESC NULLS LAST, s.session_key ASC",
    )
    .bind(channel_id)
    .fetch_all(pool)
    .await
    .map_err(|error| active_turn_error(error, channel_id))?;

    let mut selected: Option<(
        i64,
        Option<chrono::DateTime<chrono::Utc>>,
        CancelTurnSessionInfo,
        SelectedRow,
    )> = None;
    for row in rows {
        let (
            session_key,
            dispatch_id,
            provider_name,
            agent_id,
            requested_provider,
            owner_instance_id,
            thread_channel_id,
            runtime_channel_id,
            last_heartbeat,
        ): Columns =
            sqlx::FromRow::from_row(&row).map_err(|error| active_turn_error(error, channel_id))?;
        let read = (|| {
            Ok::<_, sqlx::Error>(SelectedRow {
                identity: [
                    provider_name.clone(),
                    row.try_get("identity_kind")?,
                    row.try_get("discord_token_hash")?,
                    runtime_channel_id.clone(),
                ],
                hosted: row.try_get("hosted_execution")?,
            })
        })()
        .map_err(|error| active_turn_error(error, channel_id))?;
        let match_rank = if thread_channel_id.as_deref() == Some(channel_id)
            || runtime_channel_id.as_deref() == Some(channel_id)
        {
            0
        } else if legacy_session_key_matches_channel(&session_key, channel_id)
            && thread_channel_id.is_none()
            && runtime_channel_id.is_none()
        {
            1
        } else if requested_provider.as_deref().is_some_and(|requested| {
            provider_name
                .as_deref()
                .is_some_and(|provider| provider == requested)
        }) {
            2
        } else {
            continue;
        };
        let info = CancelTurnSessionInfo {
            session_key,
            dispatch_id,
            provider_name,
            agent_id,
            requested_provider,
            owner_instance_id,
            match_rank,
        };
        let replace =
            selected
                .as_ref()
                .is_none_or(|(rank, heartbeat, current, _)| match match_rank.cmp(rank) {
                    std::cmp::Ordering::Less => true,
                    std::cmp::Ordering::Greater => false,
                    std::cmp::Ordering::Equal => match (last_heartbeat, *heartbeat) {
                        (Some(candidate), Some(existing)) if candidate != existing => {
                            candidate > existing
                        }
                        (Some(_), None) => true,
                        (None, Some(_)) => false,
                        _ => info.session_key < current.session_key,
                    },
                });
        if replace {
            selected = Some((match_rank, last_heartbeat, info, read));
        }
    }
    Ok(selected.map(|(_, _, info, read)| (info, read)))
}

type Columns = (
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<chrono::DateTime<chrono::Utc>>,
);

fn active_turn_error(error: sqlx::Error, channel_id: &str) -> ServiceError {
    ServiceError::internal(format!("load postgres active turn: {error}"))
        .with_code(ErrorCode::Database)
        .with_operation("cancel_turn.query_active_session_pg")
        .with_context("channel_id", channel_id)
}

pub(super) fn legacy_session_key_matches_channel(session_key: &str, channel_id: &str) -> bool {
    if channel_id.is_empty() || !channel_id.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }

    session_key
        .as_bytes()
        .windows(channel_id.len())
        .enumerate()
        .any(|(offset, window)| {
            window == channel_id.as_bytes()
                && offset > 0
                && matches!(
                    session_key.as_bytes()[offset - 1],
                    b'-' | b'_' | b':' | b'/'
                )
                && session_key
                    .as_bytes()
                    .get(offset + channel_id.len())
                    .is_none_or(|byte| !byte.is_ascii_digit())
        })
}
