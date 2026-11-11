//! Agent defaults select a node only after existing session ownership is resolved.
use sqlx::PgPool;

pub(crate) use crate::domain::cluster_policy::AgentExecutionNode;

pub(crate) async fn get(
    pool: &PgPool,
    agent: &str,
) -> Result<Option<AgentExecutionNode>, sqlx::Error> {
    let node: Option<Option<String>> =
        sqlx::query_scalar("SELECT default_execution_node_id FROM agents WHERE id=$1")
            .bind(agent)
            .fetch_optional(pool)
            .await?;
    Ok(node.map(|default_node_id| AgentExecutionNode { default_node_id }))
}

pub(crate) async fn node_registered(pool: &PgPool, node: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM worker_nodes WHERE instance_id=$1)")
        .bind(node)
        .fetch_one(pool)
        .await
}

pub(crate) async fn set(
    pool: &PgPool,
    agent: &str,
    policy: &AgentExecutionNode,
) -> Result<bool, sqlx::Error> {
    Ok(
        sqlx::query("UPDATE agents SET default_execution_node_id=$2, updated_at=NOW() WHERE id=$1")
            .bind(agent)
            .bind(&policy.default_node_id)
            .execute(pool)
            .await?
            .rows_affected()
            > 0,
    )
}

pub(crate) async fn for_channel(
    pool: &PgPool,
    channel: &str,
) -> Result<Option<String>, sqlx::Error> {
    let value: Option<Option<String>> = sqlx::query_scalar(
        "SELECT default_execution_node_id FROM agents WHERE discord_channel_id=$1
         OR discord_channel_alt=$1 OR discord_channel_cc=$1 OR discord_channel_cdx=$1 LIMIT 1",
    )
    .bind(channel)
    .fetch_optional(pool)
    .await?;
    Ok(value.flatten())
}

pub(crate) async fn channel_is_bound(pool: &PgPool, channel: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM agents WHERE discord_channel_id=$1
         OR discord_channel_alt=$1 OR discord_channel_cc=$1 OR discord_channel_cdx=$1)",
    )
    .bind(channel)
    .fetch_one(pool)
    .await
}

pub(crate) async fn has_channel_policy(pool: &PgPool) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM agents WHERE default_execution_node_id IS NOT NULL
         OR execution_requirements <> '{}'::jsonb OR preferred_intake_node_labels <> '[]'::jsonb)",
    )
    .fetch_one(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_policy_paths_are_the_domain_items() {
        use crate::domain::cluster_policy::{
            AgentExecutionNode as DomainNode, ExecutionRequirements as DomainRequirements,
            supports_attachment_transfer,
        };
        use crate::services::cluster::{attachment_transfer, execution_requirements};

        let canonical_node = DomainNode {
            default_node_id: None,
        };
        let old_node: AgentExecutionNode = canonical_node;
        let canonical_node: DomainNode = old_node;
        assert!(canonical_node.validate().is_ok());
        let canonical = DomainRequirements::parse(serde_json::json!({})).unwrap();
        let old: execution_requirements::ExecutionRequirements = canonical;
        let canonical: DomainRequirements = old;
        assert!(canonical.is_empty());
        let old_supports: fn(&serde_json::Value) -> bool = attachment_transfer::supports;
        let canonical_supports: fn(&serde_json::Value) -> bool = supports_attachment_transfer;
        let node = serde_json::json!({"capabilities":{"intake_worker":{
            "features":["attachment_bundle_v1"]
        }}});
        assert!(old_supports(&node));
        assert!(canonical_supports(&node));
    }

    #[test]
    fn validates_stable_node_identifiers_and_explicit_reset() {
        for id in [
            None,
            Some("single-node"),
            Some("windows-worker-1"),
            Some("studio.arm64"),
        ] {
            assert!(
                AgentExecutionNode {
                    default_node_id: id.map(str::to_owned)
                }
                .validate()
                .is_ok()
            );
        }
        for id in ["", " node", "a/b", "a:b", "node\n"] {
            assert!(
                AgentExecutionNode {
                    default_node_id: Some(id.into())
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            serde_json::from_value::<AgentExecutionNode>(serde_json::json!({"role":"worker"}))
                .is_err()
        );
    }
}
