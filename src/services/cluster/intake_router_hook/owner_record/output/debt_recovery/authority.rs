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

#[cfg(test)]
use crate::services::tui_o::ownership::{GatewayOwnership, OwnershipGate, execution::OwnerPermit};
#[cfg(test)]
use tokio::sync::watch;
#[cfg(test)]
pub(crate) trait Runtime: Send + Sync {
    fn channel(&self) -> u64;
    fn run(&self) -> Uuid;
    fn store_sha(&self) -> &str;
    fn bot(&self) -> u64;
    fn live(&self) -> bool;
}
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct Custodian {
    identity: Option<OwnerIdentity>,
    executor: String,
    gate: watch::Receiver<GatewayOwnership>,
}
#[cfg(test)]
impl Custodian {
    pub(crate) fn execution(permit: &OwnerPermit) -> Self {
        Self {
            identity: Some(permit.identity.clone()),
            executor: permit.executor.clone(),
            gate: permit.gate().subscribe(),
        }
    }
    pub(crate) fn gateway(gate: &OwnershipGate, boot: String) -> Self {
        Self {
            identity: None,
            executor: boot,
            gate: gate.subscribe(),
        }
    }
}
#[cfg(test)]
#[derive(Clone)]
pub(crate) struct Session {
    pub(super) scope: Scope,
    pub(super) binding: Binding,
    runtime: Arc<dyn Runtime>,
    custodian: Custodian,
}
#[cfg(test)]
impl Live for Session {
    fn permits(&self, binding: &Binding) -> bool {
        self.binding == *binding
            && self.runtime.live()
            && match *self.custodian.gate.borrow() {
                GatewayOwnership::Owned { epoch } => {
                    binding.domain == "gateway" && i64::try_from(epoch) == Ok(binding.revision)
                }
                GatewayOwnership::ExecutionOwned { generation, .. } => {
                    binding.domain == "execution" && generation == binding.revision
                }
                _ => false,
            }
    }
}
#[cfg(test)]
impl Scope {
    pub(crate) async fn open_debt_session(
        &self,
        runtime: Arc<dyn Runtime>,
        custodian: Custodian,
    ) -> Result<Session, String> {
        let channel = runtime.channel().to_string();
        let (domain, revision) = match *custodian.gate.borrow() {
            GatewayOwnership::Owned { epoch } if custodian.identity.is_none() => {
                ("gateway", i64::try_from(epoch).map_err(|e| e.to_string())?)
            }
            GatewayOwnership::ExecutionOwned { generation, .. }
                if custodian.identity.as_ref().is_some_and(|id| {
                    id.provider() == self.provider && id.raw_channel_id() == channel
                }) =>
            {
                ("execution", generation)
            }
            _ => return Err("debt custodian is not verified".into()),
        };
        if runtime.bot() == 0 || !digest(runtime.store_sha()) || !runtime.live() {
            return Err("debt runtime or store is not live".into());
        }
        let binding = Binding {
            provider: self.provider.clone(),
            channel: channel.clone(),
            custodian: custodian.executor.clone(),
            domain: domain.into(),
            revision,
            run: runtime.run(),
            store_sha: runtime.store_sha().into(),
            bot_id: runtime.bot(),
        };
        let session = Session {
            scope: self.clone(),
            binding,
            runtime,
            custodian,
        };
        let mut tx = begin(&self.pool).await.map_err(|e| e.to_string())?;
        crate::db::channel_custody_lock::lock(
            &mut tx,
            &OwnerIdentity::new(&self.provider, &channel),
        )
        .await
        .map_err(|e| e.to_string())?;
        let valid: bool = if domain == "execution" {
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND owner_instance_id=$3 AND generation=$4 AND status='active' AND routing_cleanup IS NULL)")
                .bind(&self.provider).bind(&channel).bind(&session.binding.custodian).bind(revision).fetch_one(&mut *tx).await.map_err(|e|e.to_string())?
        } else {
            sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND status='active') AND NOT EXISTS(SELECT 1 FROM intake_outbox WHERE provider=$1 AND channel_id=$2 AND routing_receipt_id IS NOT NULL)")
                .bind(&self.provider).bind(&channel).fetch_one(&mut *tx).await.map_err(|e|e.to_string())?
        };
        if !valid || !session.permits(&session.binding) {
            return Err("debt custody changed".into());
        }
        sqlx::query("INSERT INTO o_herdr_debt_runs(provider,channel_id,binding,active) VALUES($1,$2,$3,TRUE) ON CONFLICT(provider,channel_id) DO UPDATE SET binding=EXCLUDED.binding,active=TRUE")
            .bind(&self.provider).bind(&channel).bind(serde_json::to_value(&session.binding).map_err(|e|e.to_string())?).execute(&mut *tx).await.map_err(|e|e.to_string())?;
        sqlx::query("UPDATE o_herdr_debt_grants SET state='revoked' WHERE claim_core->>'provider'=$1 AND claim_core->>'channel'=$2 AND claim_core->'authority'->>'run'<>$3 AND state IN ('issued','reserved')")
            .bind(&self.provider).bind(&channel).bind(session.binding.run.to_string()).execute(&mut *tx).await.map_err(|e|e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(session)
    }
}
#[cfg(test)]
impl Session {
    pub(crate) async fn close(&self) -> Result<(), String> {
        let mut tx = begin(&self.scope.pool).await.map_err(|e| e.to_string())?;
        crate::db::channel_custody_lock::lock(
            &mut tx,
            &OwnerIdentity::new(&self.binding.provider, &self.binding.channel),
        )
        .await
        .map_err(|e| e.to_string())?;
        let binding = serde_json::to_value(&self.binding).map_err(|e| e.to_string())?;
        sqlx::query("UPDATE o_herdr_debt_runs SET active=FALSE WHERE provider=$1 AND channel_id=$2 AND binding=$3")
            .bind(&self.binding.provider).bind(&self.binding.channel).bind(&binding).execute(&mut *tx).await.map_err(|e|e.to_string())?;
        sqlx::query("UPDATE o_herdr_debt_grants SET state='revoked' WHERE claim_core->'authority'=$1 AND state IN ('issued','reserved')")
            .bind(binding).execute(&mut *tx).await.map_err(|e|e.to_string())?;
        tx.commit().await.map_err(|e| e.to_string())
    }
    pub(crate) async fn issue(
        &self,
        plan: &impl Registration,
        index: u32,
    ) -> Result<super::super::Artifact, String> {
        if !self.permits(&self.binding) {
            return Err("debt session is stopped".into());
        }
        self.scope
            .register_herdr_body_debt(plan, self.binding.bot_id)
            .await?;
        let doc = plan.registration(self.binding.bot_id)?;
        if doc["provider"] != self.binding.provider || doc["channel"] != self.binding.channel {
            return Err("debt session destination mismatch".into());
        }
        let piece = doc["effects"]
            .as_array()
            .and_then(|p| p.get(index as usize))
            .ok_or("debt fragment absent")?;
        let snapshot = doc["snapshots"]
            .as_object()
            .and_then(|s| s.keys().next())
            .ok_or("debt snapshot absent")?;
        let mut tx = begin(&self.scope.pool).await.map_err(|e| e.to_string())?;
        crate::db::channel_custody_lock::lock(
            &mut tx,
            &OwnerIdentity::new(&self.binding.provider, &self.binding.channel),
        )
        .await
        .map_err(|e| e.to_string())?;
        let effect:Uuid=sqlx::query_scalar("SELECT e.id FROM o_herdr_debt_effects e WHERE e.provider=$1 AND e.channel_id=$2 AND e.episode=$3 AND e.input_key=$4 AND e.coverage=$5 AND e.state='ready' AND e.receipt IS NULL AND (e.attempt_id IS NULL OR EXISTS(SELECT 1 FROM o_herdr_debt_attempts a WHERE a.id=e.attempt_id AND a.phase='no_effect' AND a.no_effect_witness->>'kind'='owning_not_polled' AND a.no_effect_witness->>'attempt'=a.id::text AND a.no_effect_witness->>'effect'=a.effect_id::text AND a.no_effect_witness->>'payload_sha'=a.payload_sha))")
            .bind(&self.binding.provider).bind(&self.binding.channel).bind(doc["episode"].as_str().ok_or("debt episode")?).bind(doc["input_key"].as_str().ok_or("debt input")?).bind(piece["coverage"].as_str().ok_or("debt coverage")?).fetch_one(&mut *tx).await.map_err(|e|e.to_string())?;
        let reference = Reference {
            version: 1,
            provider: self.binding.provider.clone(),
            channel: self.binding.channel.clone(),
            episode: doc["episode"].as_str().ok_or("debt episode")?.into(),
            input_key: doc["input_key"].as_str().ok_or("debt input")?.into(),
            coverage: piece["coverage"].as_str().ok_or("debt coverage")?.into(),
            snapshot_sha: snapshot.clone(),
            plan_sha: doc["plan_sha"].as_str().ok_or("debt plan")?.into(),
            payload_sha: piece["payload_sha"].as_str().ok_or("debt hash")?.into(),
            unit_key: serde_json::from_value(piece["unit_key"].clone())
                .map_err(|e| e.to_string())?,
            piece_index: index,
            effect_id: effect,
            grant_id: Uuid::new_v4(),
            authority: self.binding.clone(),
        };
        sqlx::query("INSERT INTO o_herdr_debt_grants(id,effect_id,episode,claim_core,state) VALUES($1,$2,$3,$4,'issued')")
            .bind(reference.grant_id).bind(effect).bind(&reference.episode).bind(reference.core().map_err(|e|e.to_string())?).execute(&mut *tx).await.map_err(|e|e.to_string())?;
        registry::check(&mut tx, &reference, "issued", "ready")
            .await
            .map_err(|e| e.to_string())?;
        if !self.permits(&self.binding) {
            return Err("debt session changed during issue".into());
        }
        tx.commit().await.map_err(|e| e.to_string())?;
        Ok(super::super::Artifact::Debt(Box::new(Artifact {
            attachment: Some(Arc::new(Attachment {
                live: Arc::new(self.clone()),
                grant: reference.grant_id,
                used: AtomicBool::new(false),
            })),
            debt_recovery: reference,
        })))
    }
}
#[cfg(test)]
impl EffectGuard {
    pub(crate) async fn cancel_unpolled(mut self) -> Result<(), String> {
        match (self.0.take(), self.1.take()) {
            (Some(tx), Some(attempt)) => super::no_request(tx, &attempt).await,
            (Some(tx), None) => tx.rollback().await.map_err(|e| e.to_string()),
            _ => Ok(()),
        }
    }
}
impl super::super::Artifact {
    pub(crate) fn debt_piece(
        &self,
        unit: &crate::services::tui_o::shadow::UnitKey,
        index: u32,
        payload: &str,
    ) -> bool {
        matches!(self,Self::Debt(debt) if debt.debt_recovery.piece_index==index && debt.debt_recovery.payload_sha==hash(payload)
            && serde_json::to_value(&debt.debt_recovery.unit_key).ok()==serde_json::to_value(unit).ok())
    }
}
#[cfg(test)]
impl Session {
    pub(crate) async fn register(&self, plan: &impl Registration) -> Result<(), String> {
        let key = plan.lookup();
        if key["provider"] != self.binding.provider || key["channel"] != self.binding.channel {
            return Err("debt registration destination mismatch".into());
        }
        self.scope
            .register_herdr_body_debt(plan, self.binding.bot_id)
            .await
            .map(|_| ())
    }
}
