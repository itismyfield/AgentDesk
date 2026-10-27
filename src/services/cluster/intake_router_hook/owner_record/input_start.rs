//! A committed source begin and a move-only owner guard for the first raw input.
use super::{OwnerIdentity, custody};
use crate::db::replay_disposition::write::{self, BeginFrom, EffectProjection, ExactAttempt};
use crate::services::discord::replay_policy::permit::{EffectTarget, StartPermit};
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::future::Future;

pub(crate) struct StartRequest<'a> {
    pub delivery_id: i64,
    pub executor: &'a str,
    pub claim_owner: &'a str,
    pub incarnation: &'a str,
    pub frame: &'a str,
    pub native_binding: &'a serde_json::Value,
}

pub(crate) struct OwnerStartPermit {
    pool: PgPool,
    delivery: i64,
    identity: OwnerIdentity,
    executor: String,
    generation: i64,
    claim_owner: String,
    incarnation: String,
    target: EffectTarget,
    source: StartPermit,
    binding: serde_json::Value,
}

#[derive(sqlx::FromRow)]
struct Ready {
    provider: String,
    channel_id: String,
    owner_generation: i64,
    routing_receipt_id: i64,
    replay_episode_nonce: String,
}

fn error(value: impl std::fmt::Display) -> String {
    value.to_string()
}
fn hash(frame: &str) -> String {
    hex::encode(Sha256::digest(frame.as_bytes()))
}

/// The frame and exact native binding must already be prepared; this commits begin before raw IO.
pub(crate) async fn prepare(
    pool: &PgPool,
    request: StartRequest<'_>,
) -> Result<OwnerStartPermit, String> {
    let row: Ready = sqlx::query_as("SELECT d.provider,d.channel_id,d.owner_generation,d.routing_receipt_id,r.replay_episode_nonce FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id JOIN intake_session_owners o ON o.provider=d.provider AND o.raw_channel_id=d.channel_id AND o.owner_instance_id=d.owner_instance_id AND o.generation=d.owner_generation AND o.status='active' WHERE d.id=$1 AND d.routing_state='admitted' AND d.status='spawned' AND d.target_instance_id=$2 AND d.claim_owner=$3 AND r.replay_disposition='registered_not_started'")
        .bind(request.delivery_id).bind(request.executor).bind(request.claim_owner).fetch_one(pool).await.map_err(error)?;
    let target = EffectTarget::ProviderStart {
        provider: row.provider.clone(),
        channel: row.channel_id.clone(),
    };
    let target_key = target.key();
    let input_hash = hash(request.frame);
    let projection = EffectProjection {
        effect_target: &target_key,
        input_hash: &input_hash,
        binding: request.native_binding,
    };
    let ack = write::begin(
        pool,
        row.routing_receipt_id,
        BeginFrom::Registered {
            nonce: &row.replay_episode_nonce,
        },
        &projection,
        request.incarnation,
    )
    .await
    .map_err(error)?;
    let source = StartPermit::seal(ack, &target).map_err(|e| format!("{e:?}"))?;
    Ok(OwnerStartPermit {
        pool: pool.clone(),
        delivery: request.delivery_id,
        identity: OwnerIdentity::new(&row.provider, &row.channel_id),
        executor: request.executor.to_owned(),
        generation: row.owner_generation,
        claim_owner: request.claim_owner.to_owned(),
        incarnation: request.incarnation.to_owned(),
        target,
        source,
        binding: request.native_binding.clone(),
    })
}

impl OwnerStartPermit {
    /// Move this entire future into the physical writer task; the guard lasts through its raw IO.
    pub(crate) async fn submit<R, F: Future<Output = R>>(
        self,
        frame: &str,
        native_binding: &serde_json::Value,
        raw: impl FnOnce(ExactAttempt) -> F,
    ) -> Result<R, String> {
        if native_binding != &self.binding {
            return Err("native binding changed".into());
        }
        let mut tx = self.pool.begin().await.map_err(error)?;
        sqlx::query("SET LOCAL idle_in_transaction_session_timeout='65s'")
            .execute(&mut *tx)
            .await
            .map_err(error)?;
        custody::lock(&mut tx, &self.identity)
            .await
            .map_err(error)?;
        let valid: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id JOIN intake_session_owners o ON o.provider=d.provider AND o.raw_channel_id=d.channel_id AND o.owner_instance_id=d.owner_instance_id AND o.generation=d.owner_generation AND o.status='active' WHERE d.id=$1 AND d.routing_state='admitted' AND d.status='spawned' AND d.owner_instance_id=$2 AND d.owner_generation=$3 AND d.claim_owner=$4 AND r.id=$5 AND r.replay_episode_nonce=$6 AND r.replay_owner_incarnation=$7 AND r.replay_disposition='started_unclassified')")
            .bind(self.delivery).bind(&self.executor).bind(self.generation).bind(&self.claim_owner)
            .bind(self.source.attempt().receipt_id()).bind(self.source.attempt().nonce()).bind(&self.incarnation)
            .fetch_one(&mut *tx).await.map_err(error)?;
        if !valid {
            return Err("owner or source no longer current".into());
        }
        let future = self
            .source
            .consume(&self.target, &hash(frame), raw)
            .map_err(|e| format!("{e:?}"))?;
        let result = tokio::time::timeout(std::time::Duration::from_secs(60), future)
            .await
            .map_err(error)?;
        // Raw acknowledgement is independent of releasing the read-only serialization guard.
        tx.rollback().await.map_err(error)?;
        Ok(result)
    }
}

pub(crate) async fn has_custody(
    pool: &PgPool,
    input: &write::CanonicalInput,
) -> Result<bool, sqlx::Error> {
    custody::lookup_source(pool, input)
        .await
        .map(|row| row.is_some())
}
