use crate::db::agents::{
    resolve_agent_channel_for_provider_pg, resolve_agent_dispatch_channel_pg,
    resolve_agent_primary_channel_pg,
};
use crate::dispatch::dispatch_destination_provider_override;
use sqlx::{PgPool, Row as SqlxRow};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DispatchDeliveryMetadata {
    pub(crate) dispatch_type: Option<String>,
    pub(crate) status: Option<String>,
    pub(crate) context: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CardIssueInfo {
    pub(crate) issue_url: Option<String>,
    pub(crate) issue_number: Option<i64>,
}

pub(crate) fn dispatch_context_value(dispatch_context: Option<&str>) -> Option<serde_json::Value> {
    dispatch_context.and_then(|ctx| serde_json::from_str::<serde_json::Value>(ctx).ok())
}

fn json_value_kind(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "bool",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

pub(crate) fn parse_pg_dispatch_context(
    dispatch_id: &str,
    raw_context: Option<&str>,
    warn_reason: &'static str,
) -> Option<serde_json::Value> {
    let raw_context = raw_context
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    let value = match serde_json::from_str::<serde_json::Value>(raw_context) {
        Ok(value) => value,
        Err(error) => {
            tracing::warn!(
                dispatch_id,
                %error,
                warn_reason,
                "[dispatch] invalid postgres dispatch context JSON"
            );
            return None;
        }
    };
    if !value.is_object() {
        tracing::warn!(
            dispatch_id,
            json_type = json_value_kind(&value),
            warn_reason,
            "[dispatch] postgres dispatch context is not an object"
        );
        return None;
    }
    Some(value)
}

pub(crate) async fn load_dispatch_delivery_metadata_pg(
    pool: &PgPool,
    dispatch_id: &str,
) -> Result<DispatchDeliveryMetadata, String> {
    let row = sqlx::query(
        "SELECT dispatch_type, status, context
         FROM task_dispatches
         WHERE id = $1",
    )
    .bind(dispatch_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres dispatch metadata for {dispatch_id}: {error}"))?;
    row.map(|row| {
        Ok::<DispatchDeliveryMetadata, String>(DispatchDeliveryMetadata {
            dispatch_type: row.try_get("dispatch_type").map_err(|error| {
                format!("read postgres dispatch_type for {dispatch_id}: {error}")
            })?,
            status: row
                .try_get("status")
                .map_err(|error| format!("read postgres status for {dispatch_id}: {error}"))?,
            context: row
                .try_get("context")
                .map_err(|error| format!("read postgres context for {dispatch_id}: {error}"))?,
        })
    })
    .transpose()?
    .ok_or_else(|| format!("dispatch {dispatch_id} not found"))
}

pub(crate) async fn load_card_issue_info_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<CardIssueInfo, String> {
    let row = sqlx::query(
        "SELECT github_issue_url, github_issue_number
         FROM kanban_cards
         WHERE id = $1",
    )
    .bind(card_id)
    .fetch_optional(pool)
    .await
    .map_err(|error| format!("load postgres card issue info for {card_id}: {error}"))?;
    row.map(|row| {
        Ok(CardIssueInfo {
            issue_url: row.try_get("github_issue_url").map_err(|error| {
                format!("read postgres github_issue_url for {card_id}: {error}")
            })?,
            issue_number: row
                .try_get::<Option<i64>, _>("github_issue_number")
                .map_err(|error| {
                    format!("read postgres github_issue_number for {card_id}: {error}")
                })?,
        })
    })
    .transpose()
    .map(|value| value.unwrap_or_default())
}

async fn latest_completed_review_provider_pg(
    pool: &PgPool,
    card_id: &str,
) -> Result<Option<String>, String> {
    let rows = sqlx::query(
        "SELECT id, context
         FROM task_dispatches
         WHERE kanban_card_id = $1
           AND dispatch_type = 'review'
           AND status = 'completed'
         ORDER BY COALESCE(completed_at, updated_at) DESC, updated_at DESC
         LIMIT 1",
    )
    .bind(card_id)
    .fetch_all(pool)
    .await
    .map_err(|error| format!("load postgres review provider for {card_id}: {error}"))?;

    for row in rows {
        let dispatch_id: String = row
            .try_get("id")
            .map_err(|error| format!("read postgres review dispatch id for {card_id}: {error}"))?;
        let context: Option<String> = match row.try_get("context") {
            Ok(context) => context,
            Err(error) => {
                tracing::warn!(
                    dispatch_id,
                    card_id,
                    %error,
                    "[dispatch] failed to decode postgres review context while loading provider"
                );
                continue;
            }
        };
        if let Some(provider) = parse_pg_dispatch_context(
            &dispatch_id,
            context.as_deref(),
            "latest_completed_review_provider_pg",
        )
        .and_then(|ctx| {
            ctx.get("from_provider")
                .and_then(|value| value.as_str())
                .map(str::to_string)
        }) {
            return Ok(Some(provider));
        }
    }

    Ok(None)
}

async fn resolve_agent_channel_with_provider_override_pg(
    pool: &PgPool,
    agent_id: &str,
    dispatch_type: Option<&str>,
    provider_override: Option<&str>,
) -> Result<Option<String>, String> {
    if let Some(provider) = provider_override.filter(|provider| !provider.trim().is_empty()) {
        if let Some(channel) = resolve_agent_channel_for_provider_pg(pool, agent_id, Some(provider))
            .await
            .map_err(|error| {
                format!("resolve postgres provider channel for {agent_id} ({provider}): {error}")
            })?
        {
            return Ok(Some(channel));
        }
    }

    resolve_agent_dispatch_channel_pg(pool, agent_id, dispatch_type)
        .await
        .map_err(|error| format!("resolve postgres dispatch channel for {agent_id}: {error}"))
}

pub(crate) async fn resolve_dispatch_delivery_channel_pg(
    pool: &PgPool,
    agent_id: &str,
    card_id: &str,
    dispatch_type: Option<&str>,
    dispatch_context: Option<&str>,
) -> Result<Option<String>, String> {
    let provider_override = if dispatch_type == Some("review") {
        dispatch_destination_provider_override(dispatch_type, dispatch_context)
    } else if dispatch_type == Some("review-decision") {
        match dispatch_destination_provider_override(dispatch_type, dispatch_context) {
            Some(provider) => Some(provider),
            None => latest_completed_review_provider_pg(pool, card_id).await?,
        }
    } else {
        None
    };

    resolve_agent_channel_with_provider_override_pg(
        pool,
        agent_id,
        dispatch_type,
        provider_override.as_deref(),
    )
    .await
}

pub(crate) async fn resolve_review_followup_channel_pg(
    pool: &PgPool,
    agent_id: &str,
) -> Result<Option<String>, String> {
    resolve_agent_primary_channel_pg(pool, agent_id)
        .await
        .map_err(|error| format!("resolve postgres primary review channel for {agent_id}: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_context_value_parses_json_object() {
        let value = dispatch_context_value(Some(r#"{"from_provider":"codex"}"#))
            .expect("context should parse");

        assert_eq!(value["from_provider"], "codex");
    }

    #[test]
    fn parse_pg_dispatch_context_rejects_non_object_context() {
        assert!(parse_pg_dispatch_context("dispatch-1", Some(r#""codex""#), "test").is_none());
    }

    #[test]
    fn parse_pg_dispatch_context_ignores_empty_context() {
        assert!(parse_pg_dispatch_context("dispatch-1", Some("   "), "test").is_none());
    }

    #[test]
    fn parse_pg_dispatch_context_rejects_malformed_json() {
        assert!(parse_pg_dispatch_context("dispatch-1", Some("{"), "test").is_none());
    }
}
