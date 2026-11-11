use serde_json::Value;
use sqlx::PgPool;

use crate::db::intake_outbox::pre_accept_sweep::{Preparation, prepare_locked};
use crate::db::intake_outbox::{FailedPreAcceptSweepOutcome, IntakeOutboxRow};
use crate::domain::cluster_policy::ExecutionRequirements;

pub(crate) async fn sweep_failed_pre_accept_once(
    pool: &PgPool,
    local_leader: &str,
    max_attempts_per_message: u32,
    worker_lease_ttl_secs: u64,
    retry_authorization_secs: Option<u64>,
) -> Result<FailedPreAcceptSweepOutcome, sqlx::Error> {
    let _ = local_leader;
    let tx = pool.begin().await?;
    let (tx, outcome) = match prepare_locked(
        tx,
        max_attempts_per_message,
        worker_lease_ttl_secs,
        retry_authorization_secs,
    )
    .await?
    {
        Preparation::Early { tx, outcome } => (tx, outcome),
        Preparation::Ready(prepared) => {
            let selected = select_target_index(prepared.source(), prepared.targets())?;
            (*prepared).finish_locked(selected).await?
        }
    };
    tx.commit().await?;
    Ok(outcome)
}

fn select_target_index(
    source: &IntakeOutboxRow,
    targets: &[Value],
) -> Result<Option<usize>, sqlx::Error> {
    let requirements = ExecutionRequirements::parse(source.execution_requirements.clone())
        .map_err(sqlx::Error::Protocol)?;
    let auth_profile = super::readiness::expected_auth_profile(
        &source.provider,
        &source.channel_id,
        &source.agent_id,
    );
    let selected = targets.iter().position(|node| {
        requirements.intake_reasons(node).is_empty()
            && (source.attachment_refs.as_array().is_some_and(Vec::is_empty)
                || crate::domain::cluster_policy::supports_attachment_transfer(node))
            && super::readiness::evaluate_declared(node, &source.provider, &auth_profile).eligible
    });
    // The first eligible node with a malformed ID remains NoCapableTarget.
    Ok(selected.filter(|index| targets[*index]["instance_id"].as_str().is_some()))
}

#[cfg(test)]
mod tests;
