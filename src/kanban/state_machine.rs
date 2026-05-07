//! Central kanban state machine.
//!
//! ALL card status transitions MUST go through the Postgres transition helpers.
//! This ensures hooks fire, auto-queue syncs, and notifications are sent.
//!
//! ## Pipeline-Driven Transitions (#106 P5)
//!
//! All transition rules, gates, hooks, clocks, and timeouts are defined in
//! `policies/default-pipeline.yaml`. No hardcoded state names exist in this module.
//! See the YAML file for the complete state machine specification.
//!
//! Custom pipelines can override the default via repo or agent-level overrides
//! (3-level inheritance: default → repo → agent).

use anyhow::Result;

pub(crate) async fn resolve_pipeline_with_pg(
    pg_pool: &sqlx::PgPool,
    repo_id: Option<&str>,
    agent_id: Option<&str>,
) -> Result<crate::pipeline::PipelineConfig> {
    let repo_override = if let Some(repo_id) = repo_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config
             FROM github_repos
             WHERE id = $1",
        )
        .bind(repo_id)
        .fetch_optional(pg_pool)
        .await
        .map_err(|error| anyhow::anyhow!("load repo pipeline override for {repo_id}: {error}"))?
        .flatten()
        .map(|json| crate::pipeline::parse_override(&json))
        .transpose()
        .map_err(|error| anyhow::anyhow!("parse repo pipeline override for {repo_id}: {error}"))?
        .flatten()
    } else {
        None
    };

    let agent_override = if let Some(agent_id) = agent_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config
             FROM agents
             WHERE id = $1",
        )
        .bind(agent_id)
        .fetch_optional(pg_pool)
        .await
        .map_err(|error| anyhow::anyhow!("load agent pipeline override for {agent_id}: {error}"))?
        .flatten()
        .map(|json| crate::pipeline::parse_override(&json))
        .transpose()
        .map_err(|error| anyhow::anyhow!("parse agent pipeline override for {agent_id}: {error}"))?
        .flatten()
    } else {
        None
    };

    Ok(crate::pipeline::resolve(
        repo_override.as_ref(),
        agent_override.as_ref(),
    ))
}
