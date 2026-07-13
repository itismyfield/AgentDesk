//! Extracted from `services::discord::health` (#3038 Phase A) — verbatim
//! move; behavior unchanged. `/api/discord/send` target parsing and
//! channel/agent/routine-thread target resolution.

use poise::serenity_prelude::ChannelId;
use sqlx::PgPool;

/// Canonical public contract for the routed `send` target grammar.
///
/// CLI help and resolver errors share this text so they cannot advertise
/// prefixes that the parser does not implement.
pub(crate) const SEND_TARGET_CONTRACT: &str = "Target must be channel:<id>, channel:<name>, or agent:<roleId>; bare channel IDs/names are accepted for compatibility.";

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
        return Err(SendTargetResolutionError::BadRequest(SEND_TARGET_CONTRACT));
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
    let channel_target = match target.strip_prefix("channel:") {
        Some(channel_target) => channel_target,
        None if target.contains(':') => {
            return Err(SendTargetResolutionError::BadRequest(SEND_TARGET_CONTRACT));
        }
        None => target,
    };
    parse_channel_target_value(channel_target)
        .ok_or(SendTargetResolutionError::BadRequest(SEND_TARGET_CONTRACT))
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

    use super::{
        SEND_TARGET_CONTRACT, SendTargetResolutionError, parse_agent_target,
        parse_channel_target_value, resolve_channel_target,
    };

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
            Err(SendTargetResolutionError::BadRequest(SEND_TARGET_CONTRACT))
        );
    }

    #[test]
    fn agent_target_with_role_id_parses_after_trimming() {
        assert_eq!(
            parse_agent_target("agent: backend-dev "),
            Ok(Some("backend-dev"))
        );
    }

    #[test]
    fn published_target_contract_matches_parser_prefixes() {
        let root_result = tempfile::tempdir();
        assert!(
            root_result.is_ok(),
            "create temporary AgentDesk root for target contract test: {:?}",
            root_result.as_ref().err()
        );
        let Some(root) = root_result.ok() else {
            return;
        };
        let _root_guard = crate::config::set_agentdesk_root_for_test(root.path());
        let config_dir = root.path().join("config");
        assert!(
            std::fs::create_dir_all(&config_dir).is_ok(),
            "create target contract test config directory"
        );
        assert!(
            std::fs::write(
                config_dir.join("role_map.json"),
                serde_json::json!({
                    "byChannelName": {
                        "supported-alias": { "channelId": "4225000" },
                        "user:4225": { "channelId": "4225001" },
                        "role:backend-dev": { "channelId": "4225002" }
                    },
                    "byChannelId": {}
                })
                .to_string(),
            )
            .is_ok(),
            "write alias-collision role map"
        );

        assert_eq!(resolve_channel_target("channel:123"), Ok(123));
        assert_eq!(resolve_channel_target("123"), Ok(123));
        assert_eq!(
            resolve_channel_target("channel:supported-alias"),
            Ok(4225000)
        );
        assert_eq!(resolve_channel_target("supported-alias"), Ok(4225000));
        assert_eq!(
            parse_agent_target("agent:backend-dev"),
            Ok(Some("backend-dev"))
        );

        for unsupported in ["user:4225", "role:backend-dev"] {
            assert_eq!(
                resolve_channel_target(unsupported),
                Err(SendTargetResolutionError::BadRequest(SEND_TARGET_CONTRACT)),
                "unsupported target prefix must not be advertised: {unsupported}"
            );
        }
        assert!(SEND_TARGET_CONTRACT.contains("channel:<id>"));
        assert!(SEND_TARGET_CONTRACT.contains("channel:<name>"));
        assert!(SEND_TARGET_CONTRACT.contains("agent:<roleId>"));
        assert!(!SEND_TARGET_CONTRACT.contains("user:<id>"));
        assert!(!SEND_TARGET_CONTRACT.contains("role:<name>"));
    }
}
