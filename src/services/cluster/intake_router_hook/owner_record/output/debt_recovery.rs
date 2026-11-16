//! A retained debt's permission never revives its original native admission.
use super::{AcquireError, EffectGuard, Scope, begin, hash};
use crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Postgres, Transaction};
use std::sync::Arc;
use uuid::Uuid;
mod authority;
mod receipt;
mod registry;
#[cfg(test)]
#[path = "debt_recovery/debt_recovery_tests.rs"]
mod tests;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Binding {
    provider: String,
    channel: String,
    custodian: String,
    domain: String,
    revision: i64,
    run: Uuid,
    store_sha: String,
    bot_id: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Unit {
    channel_id: u64,
    provider: String,
    native_key: String,
    kind: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Reference {
    version: u8,
    provider: String,
    channel: String,
    episode: String,
    input_key: String,
    coverage: String,
    snapshot_sha: String,
    plan_sha: String,
    payload_sha: String,
    unit_key: Unit,
    piece_index: u32,
    effect_id: Uuid,
    grant_id: Uuid,
    authority: Binding,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Artifact {
    debt_recovery: Reference,
    #[serde(skip)]
    attachment: Option<Arc<authority::Attachment>>,
}
impl std::fmt::Debug for Artifact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DebtRecovery")
            .field("reference", &self.debt_recovery)
            .finish_non_exhaustive()
    }
}
impl PartialEq for Artifact {
    fn eq(&self, other: &Self) -> bool {
        self.debt_recovery == other.debt_recovery
    }
}
impl Eq for Artifact {}
impl Artifact {
    pub(super) fn provider(&self) -> &str {
        &self.debt_recovery.provider
    }
}
fn denied(reason: &'static str) -> AcquireError {
    AcquireError::Withheld(reason)
}
fn digest(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())
}
impl Reference {
    fn core(&self) -> Result<Value, AcquireError> {
        let mut value =
            serde_json::to_value(self).map_err(|_| denied("debt reference encoding"))?;
        value
            .as_object_mut()
            .ok_or_else(|| denied("debt reference object"))?
            .remove("episode");
        Ok(value)
    }
    fn valid(&self, scope: &Scope, channel: &str, payload: &str) -> bool {
        self.version == 1
            && self.provider == scope.provider
            && self.channel == channel
            && self.unit_key.channel_id.to_string() == channel
            && self.unit_key.channel_id > 0
            && !self.unit_key.native_key.is_empty()
            && self.unit_key.kind == "body"
            && self.piece_index <= i32::MAX as u32
            && self.unit_key.provider == self.provider
            && matches!(self.provider.as_str(), "claude" | "codex")
            && self.authority.provider == self.provider
            && self.authority.channel == self.channel
            && !self.authority.custodian.trim().is_empty()
            && self.authority.revision >= 0
            && matches!(self.authority.domain.as_str(), "execution" | "gateway")
            && !self.authority.run.is_nil()
            && self.authority.bot_id > 0
            && digest(&self.authority.store_sha)
            && !self.effect_id.is_nil()
            && !self.grant_id.is_nil()
            && !self.coverage.is_empty()
            && digest(&self.episode)
            && digest(&self.input_key)
            && digest(&self.snapshot_sha)
            && digest(&self.plan_sha)
            && self.payload_sha == hash(payload)
    }
}
pub(super) async fn acquire(
    scope: &Scope,
    channel: &str,
    payload: &str,
    artifact: &Artifact,
) -> Result<EffectGuard, AcquireError> {
    let reference = &artifact.debt_recovery;
    if !reference.valid(scope, channel, payload) {
        return Err(denied("debt scope or payload changed"));
    }
    let attachment = artifact
        .attachment
        .as_ref()
        .ok_or_else(|| denied("debt token needs live revalidation"))?;
    attachment.claim(reference)?;
    let id = OwnerIdentity::new(&scope.provider, channel);
    let mut tx = begin(&scope.pool).await?;
    crate::db::channel_custody_lock::lock(&mut tx, &id).await?;
    let attempt = registry::reserve(&mut tx, reference).await?;
    tx.commit().await?;
    guarded(
        scope,
        Attempt {
            reference: reference.clone(),
            attachment: Arc::clone(attachment),
            id: attempt,
            started: false,
        },
    )
    .await
}
async fn guarded(scope: &Scope, attempt: Attempt) -> Result<EffectGuard, AcquireError> {
    let reference = &attempt.reference;
    let id = OwnerIdentity::new(&scope.provider, &reference.channel);
    // Reservation is durable before this guard; dropping either process never makes it fresh.
    let mut held = begin(&scope.pool).await?;
    crate::db::channel_custody_lock::lock(&mut held, &id).await?;
    if let Err(error) = registry::check(&mut held, reference, "reserved", "reserved").await {
        if matches!(error, AcquireError::Withheld(_)) {
            // This live acquisition owns the unpolled attempt; a restored token cannot attest it.
            no_request(held, &attempt)
                .await
                .map_err(|_| denied("debt unpolled proof could not be recorded"))?;
        }
        return Err(error);
    }
    let current: Option<Uuid> =
        sqlx::query_scalar("SELECT attempt_id FROM o_herdr_debt_effects WHERE id=$1")
            .bind(reference.effect_id)
            .fetch_one(&mut *held)
            .await?;
    if current != Some(attempt.id) {
        return Err(denied("debt attempt changed"));
    }
    Ok(EffectGuard(Some(held), Some(Box::new(attempt))))
}
pub(super) struct Attempt {
    reference: Reference,
    attachment: Arc<authority::Attachment>,
    id: Uuid,
    started: bool,
}
impl Attempt {
    pub(super) fn start(&mut self) -> bool {
        if self.started || !self.attachment.live.permits(&self.reference.authority) {
            return false;
        }
        self.started = true;
        true
    }
}
pub(super) async fn no_request(
    mut tx: Transaction<'static, Postgres>,
    attempt: &Attempt,
) -> Result<(), String> {
    if attempt.started {
        return Err("debt request was already polled".into());
    }
    // Only this owned request's pre-poll path can attest no effect for its exact attempt.
    let witness = serde_json::json!({"version":1,"kind":"owning_not_polled","attempt":attempt.id,"effect":attempt.reference.effect_id,"payload_sha":attempt.reference.payload_sha});
    let changed=sqlx::query("UPDATE o_herdr_debt_attempts SET phase='no_effect',no_effect_witness=$2 WHERE id=$1 AND effect_id=$3 AND grant_id=$4 AND phase='reserved'")
        .bind(attempt.id).bind(witness).bind(attempt.reference.effect_id).bind(attempt.reference.grant_id).execute(&mut *tx).await.map_err(|e|e.to_string())?;
    if changed.rows_affected() != 1 {
        return Err("debt no-effect witness no longer owns attempt".into());
    }
    let changed=sqlx::query("UPDATE o_herdr_debt_effects SET state='ready' WHERE id=$1 AND attempt_id=$2 AND state='reserved' AND receipt IS NULL")
        .bind(attempt.reference.effect_id).bind(attempt.id).execute(&mut *tx).await.map_err(|e|e.to_string())?;
    if changed.rows_affected() != 1 {
        return Err("debt reservation no longer matches witness".into());
    }
    sqlx::query("UPDATE o_herdr_debt_grants SET state='revoked' WHERE id=$1")
        .bind(attempt.reference.grant_id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())
}
