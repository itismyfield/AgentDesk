//! Extracted from `services::discord::health` (#3038 Phase A) — verbatim
//! move; behavior unchanged. `/api/discord/send` target parsing and
//! channel/agent/routine-thread target resolution.

use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum SendTargetResolutionError {
    BadRequest(&'static str),
    NotFound(String),
    Internal(String),
}

fn parse_channel_target_value(target: &str) -> Option<u64> {
    let trimmed = target.trim();
    trimmed
        .parse::<u64>()
        .ok()
        .or_else(|| crate::services::dispatches::outbox_route::resolve_channel_alias_pub(trimmed))
}

fn parse_agent_target(target: &str) -> Result<Option<&str>, SendTargetResolutionError> {
    let Some(agent_id_raw) = target.strip_prefix("agent:") else {
        return Ok(None);
    };
    let agent_id = agent_id_raw.trim();
    if agent_id.is_empty() {
        return Err(SendTargetResolutionError::BadRequest(
            "invalid target format (use channel:<id>, channel:<name>, or agent:<roleId>)",
        ));
    }
    Ok(Some(agent_id))
}

async fn resolve_agent_target_channel_id_pg(
    pg_pool: &PgPool,
    agent_id: &str,
) -> Result<u64, SendTargetResolutionError> {
    let bindings = crate::db::agents::load_agent_channel_bindings_pg(pg_pool, agent_id)
        .await
        .map_err(|e| {
            SendTargetResolutionError::Internal(format!("agent lookup failed for {agent_id}: {e}"))
        })?
        .ok_or_else(|| {
            SendTargetResolutionError::NotFound(format!("unknown agent target: {agent_id}"))
        })?;
    let channel_target = bindings.primary_channel().ok_or_else(|| {
        SendTargetResolutionError::NotFound(format!(
            "agent target has no primary channel: {agent_id}"
        ))
    })?;

    parse_channel_target_value(&channel_target).ok_or_else(|| {
        SendTargetResolutionError::Internal(format!(
            "agent target resolved to invalid channel: {channel_target}"
        ))
    })
}

pub(super) async fn routine_thread_parent_hint(
    pg_pool: Option<&PgPool>,
    thread_channel_id: ChannelId,
) -> Option<ChannelId> {
    let Some(pg_pool) = pg_pool else {
        return None;
    };

    let agent_id = match sqlx::query_scalar::<_, String>(
        r#"
        SELECT agent_id
          FROM routines
         WHERE discord_thread_id = $1
           AND agent_id IS NOT NULL
           AND status <> 'detached'
         ORDER BY updated_at DESC
         LIMIT 1
        "#,
    )
    .bind(thread_channel_id.get().to_string())
    .fetch_optional(pg_pool)
    .await
    {
        Ok(Some(agent_id)) => agent_id,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(
                "routine thread auth lookup failed for {}: {}",
                thread_channel_id.get(),
                error
            );
            return None;
        }
    };

    let bindings = match crate::db::agents::load_agent_channel_bindings_pg(pg_pool, &agent_id).await
    {
        Ok(Some(bindings)) => bindings,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(
                "routine thread auth failed to load agent bindings for {agent_id}: {error}"
            );
            return None;
        }
    };

    let Some(primary_channel) = bindings.primary_channel() else {
        return None;
    };
    let Some(parent_channel_id) = parse_channel_target_value(&primary_channel) else {
        tracing::warn!(
            "routine thread auth found invalid primary channel for {agent_id}: {primary_channel}"
        );
        return None;
    };
    Some(ChannelId::new(parent_channel_id))
}

fn resolve_channel_target(target: &str) -> Result<u64, SendTargetResolutionError> {
    let channel_target = target.strip_prefix("channel:").unwrap_or(target);
    parse_channel_target_value(channel_target).ok_or(SendTargetResolutionError::BadRequest(
        "invalid target format (use channel:<id>, channel:<name>, or agent:<roleId>)",
    ))
}

pub(super) async fn resolve_send_target_channel_id_with_backends(
    pg_pool: Option<&PgPool>,
    target: &str,
) -> Result<u64, SendTargetResolutionError> {
    match parse_agent_target(target)? {
        Some(agent_id) => {
            if let Some(pg_pool) = pg_pool {
                return resolve_agent_target_channel_id_pg(pg_pool, agent_id).await;
            }

            Err(SendTargetResolutionError::Internal(
                "postgres pool unavailable during agent lookup".to_string(),
            ))
        }
        None => resolve_channel_target(target),
    }
}

#[cfg(test)]
mod send_target_parse_tests {
    //! #3038 Phase A characterization tests — pin the send-target parsing
    //! branches (`parse_channel_target_value` / `parse_agent_target`) before
    //! the health.rs directory decomposition.

    use super::{SendTargetResolutionError, parse_agent_target, parse_channel_target_value};

    #[test]
    fn numeric_channel_target_parses_after_trimming() {
        assert_eq!(parse_channel_target_value("123456789"), Some(123456789));
        assert_eq!(parse_channel_target_value("  987654321  "), Some(987654321));
    }

    #[test]
    fn non_numeric_channel_target_falls_back_to_alias_resolution() {
        // The non-numeric branch consults the channel-alias config; a name no
        // role map contains resolves to `None` (callers then surface the 400
        // invalid-target error).
        assert_eq!(
            parse_channel_target_value("definitely-not-a-registered-alias-3038"),
            None
        );
    }

    #[test]
    fn target_without_agent_prefix_is_not_an_agent_target() {
        assert_eq!(parse_agent_target("channel:123"), Ok(None));
        assert_eq!(parse_agent_target("123"), Ok(None));
    }

    #[test]
    fn agent_target_with_empty_id_is_bad_request() {
        assert_eq!(
            parse_agent_target("agent:   "),
            Err(SendTargetResolutionError::BadRequest(
                "invalid target format (use channel:<id>, channel:<name>, or agent:<roleId>)",
            ))
        );
    }

    #[test]
    fn agent_target_with_role_id_parses_after_trimming() {
        assert_eq!(
            parse_agent_target("agent: backend-dev "),
            Ok(Some("backend-dev"))
        );
    }
}
