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

/// The plan an observation implies; `None` when it selects no target.
pub(crate) fn build(observation: &snapshot::Shared) -> Result<Option<Plan>, sqlx::Error> {
    let Target::Selected(target) = observation.target() else {
        return Ok(None);
    };
    Ok(Some(Plan {
        version: 1,
        home: observation
            .home
            .clone()
            .ok_or_else(|| sqlx::Error::Protocol("plan has no home".into()))?,
        target: target.into(),
        home_proof: observation.proof.clone(),
        expected: owner(observation),
    }))
}

/// Planning is per held row; only promotion keeps the oldest-first order.
pub(crate) async fn save_on_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &OwnerIdentity,
    delivery: i64,
    observation: &snapshot::Shared,
) -> Result<Option<Plan>, sqlx::Error> {
    custody::lock(tx, id).await?;
    let Some(plan) = build(observation)? else {
        return Ok(None);
    };
    let target = plan.target.as_str();
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
    let changed = sqlx::query("UPDATE intake_outbox d SET routing_plan=$4,target_instance_id=$5 WHERE d.id=$1 AND d.provider=$2 AND d.channel_id=$3 AND d.routing_state='held' AND d.status='unknown' AND d.claim_owner IS NULL AND d.routing_home_revision=$6 AND d.routing_wait_owner_generation IS NOT DISTINCT FROM $7::BIGINT AND EXISTS(SELECT 1 FROM intake_outbox r WHERE r.id=d.routing_receipt_id AND r.replay_disposition='registered_not_started')")
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

/// Re-plans every held row from one fresh observation, refreshing rows a /node change or owner
/// switch left on an old revision; returns the head and its plan when a target is selected.
pub(crate) async fn replan_held(
    pool: &sqlx::PgPool,
    id: &OwnerIdentity,
    context: &snapshot::Context,
) -> Result<Option<(i64, Plan)>, sqlx::Error> {
    let mut tx = pool.begin().await?;
    custody::lock(&mut tx, id).await?;
    let fresh = snapshot::read_on_tx(&mut tx, id, context).await?;
    let Some(observed) = build(&fresh)? else {
        return Ok(None);
    };
    let held: Vec<(i64, i64, Option<i64>)> = sqlx::query_as("SELECT id,routing_home_revision,routing_wait_owner_generation FROM intake_outbox WHERE provider=$1 AND channel_id=$2 AND routing_state='held' ORDER BY id")
        .bind(id.provider()).bind(id.raw_channel_id()).fetch_all(&mut *tx).await?;
    let expected = fresh
        .owner
        .as_ref()
        .map(|o| (o.owner_instance_id.as_str(), o.generation));
    let mut head = None;
    for (index, (delivery, revision, wait)) in held.into_iter().enumerate() {
        #[cfg(test)]
        if index > 0 && super::cleanup::tests::mutant("plan-followers") {
            break;
        }
        let stale = revision != fresh.raw_revision || wait != expected.map(|(_, g)| g);
        #[cfg(test)]
        let stale = stale && !super::cleanup::tests::mutant("replan-refresh");
        if stale {
            custody::refresh_stage_on_tx(
                &mut tx,
                delivery,
                &observed.target,
                fresh.raw_revision,
                expected,
                0,
            )
            .await?;
        }
        let saved = save_on_tx(&mut tx, id, delivery, &fresh).await?;
        if index == 0 {
            head = saved.map(|plan| (delivery, plan));
        }
    }
    tx.commit().await?;
    Ok(head)
}

const RELEASED: &str = "보류 중이던 입력을 home 설정이 바뀐 뒤 다시 배정하지 못해 처리하지 않았습니다. 필요하면 다시 보내 주세요.";

/// Held custody left on an old revision is re-planned from its stored evidence; when fallback no
/// longer places this channel, the unstarted inputs are released with one notice each.
pub(crate) async fn replan_stale(
    pool: &sqlx::PgPool,
    id: &OwnerIdentity,
    proof: &Value,
) -> Result<Option<(i64, Plan)>, sqlx::Error> {
    let policy = proof["policy_channel"]
        .as_str()
        .unwrap_or(id.raw_channel_id());
    let (fallback, _) = crate::services::cluster::input_fallback_config::current();
    let Some(pair) = fallback.pair_for(id.raw_channel_id(), policy).cloned() else {
        return release_held(pool, id).await.map(|()| None);
    };
    let context = snapshot::Context {
        policy_channel: policy.into(),
        pair,
        gateway: Some(
            crate::services::cluster::node_registry::resolve_self_instance_id_without_config(),
        ),
        auth_profile: proof["auth_profile"].as_str().unwrap_or_default().into(),
        lease_seconds: i64::try_from(super::super::worker_heartbeat_lease_secs())
            .unwrap_or(i64::MAX),
    };
    replan_held(pool, id, &context).await
}

async fn release_held(pool: &sqlx::PgPool, id: &OwnerIdentity) -> Result<(), sqlx::Error> {
    #[cfg(test)]
    if super::cleanup::tests::mutant("release-skip") {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    custody::lock(&mut tx, id).await?;
    let held: Vec<(i64, i64)> = sqlx::query_as("SELECT d.id,d.routing_receipt_id FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id WHERE d.provider=$1 AND d.channel_id=$2 AND d.routing_state='held' AND d.routing_plan IS NOT NULL AND r.replay_disposition='registered_not_started' ORDER BY d.id FOR UPDATE OF d")
        .bind(id.provider()).bind(id.raw_channel_id()).fetch_all(&mut *tx).await?;
    let target = format!("channel:{}", id.raw_channel_id());
    for (delivery, receipt) in held {
        crate::db::replay_disposition::write::withhold_owner_loss_on_tx(&mut tx, receipt).await?;
        sqlx::query("UPDATE intake_outbox SET routing_state='superseded' WHERE id=$1")
            .bind(delivery)
            .execute(&mut *tx)
            .await?;
        let session = format!("routing-held:{delivery}");
        crate::services::message_outbox::enqueue_outbox_pg_returning_id_with_persistent_dedupe_on_tx(
            &mut tx,
            crate::services::message_outbox::OutboxMessage {
                target: &target,
                content: RELEASED,
                bot: super::event::BOT,
                source: "lifecycle_notifier",
                reason_code: Some("routing.held_released"),
                session_key: Some(&session),
            },
        )
        .await
        .map_err(|error| sqlx::Error::Protocol(format!("{error:?}")))?;
    }
    tx.commit().await
}
