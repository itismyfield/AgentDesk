//! A deserialized claim has no live authority; attachment clones share one use.
use super::*;
use std::sync::atomic::{AtomicBool, Ordering};
pub(super) trait Live: Send + Sync {
    fn permits(&self, binding: &Binding) -> bool;
}
pub(super) struct Attachment {
    pub(super) live: Arc<dyn Live>,
    grant: Uuid,
    used: AtomicBool,
}
impl Attachment {
    pub(super) fn claim(&self, r: &Reference) -> Result<(), AcquireError> {
        if self
            .used
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
            || self.grant != r.grant_id
            || !self.live.permits(&r.authority)
        {
            return Err(denied("debt attachment is stopped, stale or spent"));
        }
        Ok(())
    }
}
// Only a validated snapshot and live custodian may create the nonserialized attachment.
#[cfg(test)]
pub(super) async fn issue(
    pool: &sqlx::PgPool,
    binding: &Binding,
    live: Arc<dyn Live>,
    mut r: Reference,
    provenance: Value,
    payload: &str,
) -> Result<super::super::Artifact, String> {
    let id = OwnerIdentity::new(&r.provider, &r.channel);
    let mut tx = begin(pool).await.map_err(|e| e.to_string())?;
    crate::db::channel_custody_lock::lock(&mut tx, &id)
        .await
        .map_err(|e| e.to_string())?;
    let candidate = serde_json::to_value(binding).map_err(|e| e.to_string())?;
    sqlx::query("INSERT INTO o_herdr_debt_runs(provider,channel_id,binding,active) VALUES($1,$2,$3,TRUE) ON CONFLICT(provider,channel_id) DO NOTHING")
        .bind(&r.provider).bind(&r.channel).bind(candidate).execute(&mut *tx).await.map_err(|e|e.to_string())?;
    let effect = Uuid::new_v4();
    sqlx::query("INSERT INTO o_herdr_debt_effects(id,provider,channel_id,episode,input_key,coverage,plan_sha,snapshots,provenance,payload,payload_sha,unit_key,piece_index,bot_id,state) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,'ready') ON CONFLICT(provider,channel_id,episode,input_key,coverage) DO NOTHING")
        .bind(effect).bind(&r.provider).bind(&r.channel).bind(&r.episode).bind(&r.input_key).bind(&r.coverage).bind(&r.plan_sha).bind(serde_json::json!([r.snapshot_sha])).bind(provenance).bind(payload).bind(&r.payload_sha).bind(serde_json::to_value(&r.unit_key).map_err(|e|e.to_string())?).bind(i64::from(r.piece_index)).bind(i64::try_from(binding.bot_id).map_err(|e|e.to_string())?).execute(&mut *tx).await.map_err(|e|e.to_string())?;
    let effect:Uuid=sqlx::query_scalar("SELECT id FROM o_herdr_debt_effects WHERE provider=$1 AND channel_id=$2 AND episode=$3 AND input_key=$4 AND coverage=$5")
        .bind(&r.provider).bind(&r.channel).bind(&r.episode).bind(&r.input_key).bind(&r.coverage).fetch_one(&mut *tx).await.map_err(|e|e.to_string())?;
    r.effect_id = effect;
    r.grant_id = Uuid::new_v4();
    sqlx::query("INSERT INTO o_herdr_debt_grants(id,effect_id,episode,claim_core,state) VALUES($1,$2,$3,$4,'issued')")
        .bind(r.grant_id).bind(effect).bind(&r.episode).bind(r.core().map_err(|e|e.to_string())?).execute(&mut *tx).await.map_err(|e|e.to_string())?;
    registry::check(&mut tx, &r, "issued", "ready")
        .await
        .map_err(|e| e.to_string())?;
    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(super::super::Artifact::Debt(Box::new(Artifact {
        attachment: Some(Arc::new(Attachment {
            live,
            grant: r.grant_id,
            used: AtomicBool::new(false),
        })),
        debt_recovery: r,
    })))
}
