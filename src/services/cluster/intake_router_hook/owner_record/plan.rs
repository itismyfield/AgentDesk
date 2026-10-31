//! A staged plan preserves its observed home and owner; promotion must recheck both.
use super::{OwnerIdentity, custody, fallback::Target, snapshot};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Postgres, Transaction};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Plan {
    pub version: u8,
    pub home: String,
    pub target: String,
    pub home_proof: Value,
    pub expected: Option<(String, i64, String)>,
}

fn owner(snapshot: &snapshot::Shared) -> Option<(String, i64, String)> {
    snapshot
        .owner
        .as_ref()
        .map(|o| (o.owner_instance_id.clone(), o.generation, o.status.clone()))
}

pub(crate) async fn save_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
    delivery: i64,
    observation: &snapshot::Shared,
) -> Result<Option<Plan>, sqlx::Error> {
    custody::lock(tx, id).await?;
    let Target::Selected(target) = observation.target() else {
        return Ok(None);
    };
    let fresh = snapshot::read_on_tx(tx, id, &observation.context).await?;
    let proof_matches = fresh.proof == observation.proof;
    #[cfg(test)]
    let proof_matches = proof_matches || super::cleanup::tests::mutant("plan-home");
    if !proof_matches
        || owner(&fresh) != owner(observation)
        || fresh.target() != Target::Selected(target)
    {
        return Ok(None);
    }
    let plan = Plan {
        version: 1,
        home: observation
            .home
            .clone()
            .ok_or_else(|| sqlx::Error::Protocol("plan has no home".into()))?,
        target: target.into(),
        home_proof: observation.proof.clone(),
        expected: owner(observation),
    };
    let changed = sqlx::query("UPDATE intake_outbox d SET routing_plan=$4,target_instance_id=$5 WHERE d.id=$1 AND d.provider=$2 AND d.channel_id=$3 AND d.routing_state='held' AND d.status='unknown' AND d.claim_owner IS NULL AND d.routing_home_revision=$6 AND d.routing_wait_owner_generation IS NOT DISTINCT FROM $7::BIGINT AND EXISTS(SELECT 1 FROM intake_outbox r WHERE r.id=d.routing_receipt_id AND r.replay_disposition='registered_not_started') AND NOT EXISTS(SELECT 1 FROM intake_outbox h WHERE h.provider=d.provider AND h.channel_id=d.channel_id AND h.routing_state='held' AND h.id<d.id)")
        .bind(delivery).bind(id.provider()).bind(id.raw_channel_id()).bind(sqlx::types::Json(&plan))
        .bind(target).bind(observation.raw_revision).bind(observation.owner.as_ref().map(|o|o.generation))
        .execute(&mut **tx).await?.rows_affected();
    Ok((changed == 1).then_some(plan))
}

pub(crate) async fn read_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
    delivery: i64,
) -> Result<Option<Plan>, sqlx::Error> {
    let stored: Option<Option<Value>> = sqlx::query_scalar(
        "SELECT routing_plan FROM intake_outbox WHERE id=$1 AND provider=$2 AND channel_id=$3",
    )
    .bind(delivery)
    .bind(id.provider())
    .bind(id.raw_channel_id())
    .fetch_optional(&mut **tx)
    .await?;
    stored
        .flatten()
        .map(|value| {
            let plan: Plan =
                serde_json::from_value(value).map_err(|e| sqlx::Error::Protocol(e.to_string()))?;
            if plan.version != 1 || plan.home.is_empty() || plan.target.is_empty() {
                return Err(sqlx::Error::Protocol("unknown routing plan".into()));
            }
            Ok(plan)
        })
        .transpose()
}
