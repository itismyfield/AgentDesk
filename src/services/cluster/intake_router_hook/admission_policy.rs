//! One fresh row supplies both policies; ambiguous bindings keep their legacy lookup behavior.
use super::{ExecutionRequirements, IntakeRoutingMode};
use serde_json::Value;
use sqlx::PgPool;

pub(super) struct AdmissionPolicy {
    pub(super) requirements: ExecutionRequirements,
    pub(super) agent_default: Option<String>,
}

pub(super) async fn for_intake(
    pool: &PgPool,
    channel: &str,
    mode: IntakeRoutingMode,
) -> Result<AdmissionPolicy, String> {
    let rows: Result<Vec<(Value, Option<String>)>, _> = sqlx::query_as(
        "SELECT execution_requirements, default_execution_node_id FROM agents
         WHERE discord_channel_id=$1 OR discord_channel_alt=$1
            OR discord_channel_cc=$1 OR discord_channel_cdx=$1 LIMIT 2",
    )
    .bind(channel)
    .fetch_all(pool)
    .await;
    let snapshot = match rows {
        Ok(mut rows) if rows.len() <= 1 => {
            Some(rows.pop().unwrap_or((serde_json::json!({}), None)))
        }
        // A combined query must not change legacy LIMIT 1 selection or error precedence.
        _ => None,
    };
    let policy_error = |error| format!("execution policy lookup failed: {error}");
    let (requirements, default_snapshot) = match snapshot {
        Some((raw, node)) => (
            ExecutionRequirements::parse(raw).map_err(policy_error)?,
            Some(node),
        ),
        None => (
            super::super::execution_requirements::for_channel(pool, channel)
                .await
                .map_err(policy_error)?,
            None,
        ),
    };
    if !requirements.is_empty() && mode != IntakeRoutingMode::Enforce {
        return Err("hard execution requirements require enforce routing".into());
    }
    let agent_default = match default_snapshot {
        Some(node) => node,
        None => super::super::agent_execution_node::for_channel(pool, channel)
            .await
            .map_err(|error| format!("agent execution node lookup failed: {error}"))?,
    };
    if agent_default.is_some() && mode != IntakeRoutingMode::Enforce {
        return Err("agent default execution node requires enforce routing".into());
    }
    Ok(AdmissionPolicy {
        requirements,
        agent_default,
    })
}
