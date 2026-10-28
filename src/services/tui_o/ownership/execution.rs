//! A boot snapshot authorizes a local channel host; every raw effect still checks PG.
use super::OwnershipGate;
use crate::services::cluster::intake_router_hook::owner_record::{OwnerIdentity, custody};
use crate::services::tui_o::channel_policy::Candidate;
use serde_json::Value;
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SessionRef {
    Fresh,
    Bound(Value),
}

pub struct OwnerPermit {
    pub(crate) pool: PgPool,
    pub(crate) identity: OwnerIdentity,
    pub(crate) executor: String,
    pub(crate) generation: i64,
    pub(crate) session_ref: SessionRef,
    gate: Arc<OwnershipGate>,
}

impl std::fmt::Debug for OwnerPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnerPermit")
            .field("identity", &self.identity)
            .field("executor", &self.executor)
            .field("generation", &self.generation)
            .field("session_ref", &self.session_ref)
            .finish_non_exhaustive()
    }
}

impl OwnerPermit {
    pub(crate) fn gate(&self) -> Arc<OwnershipGate> {
        Arc::clone(&self.gate)
    }
    pub(crate) fn first_binding(&self, other: &Self) -> Option<Arc<Self>> {
        if self.identity != other.identity
            || self.executor != other.executor
            || self.generation != other.generation
            || !matches!(self.session_ref, SessionRef::Fresh)
            || !matches!(other.session_ref, SessionRef::Bound(_))
        {
            return None;
        }
        Some(Arc::new(Self {
            pool: other.pool.clone(),
            identity: other.identity.clone(),
            executor: other.executor.clone(),
            generation: other.generation,
            session_ref: other.session_ref.clone(),
            gate: self.gate(),
        }))
    }
    pub(crate) fn same_execution(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.executor == other.executor
            && self.generation == other.generation
            && self.session_ref == other.session_ref
    }
}

#[derive(Clone, Debug)]
pub(crate) struct View {
    pub(crate) permit: Result<Arc<OwnerPermit>, String>,
    pub(crate) candidate: Candidate,
}

impl View {
    pub(crate) fn held(reason: String) -> Self {
        Self {
            permit: Err(reason),
            candidate: Candidate::new(crate::services::tui_o::channel_policy::Adoption::Held),
        }
    }
}

/// None denotes an unmanaged channel; errors must install a held view, never Legacy.
pub(crate) async fn read(
    pool: &PgPool,
    provider: &str,
    channel: u64,
    local: &str,
    legacy_present: impl FnOnce() -> Result<bool, String>,
) -> Result<Option<Arc<OwnerPermit>>, String> {
    let identity = OwnerIdentity::new(provider, &channel.to_string());
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;
    custody::lock(&mut tx, &identity)
        .await
        .map_err(|e| e.to_string())?;
    let managed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_outbox WHERE provider=$1 AND channel_id=$2 AND routing_receipt_id IS NOT NULL)")
        .bind(identity.provider()).bind(identity.raw_channel_id()).fetch_one(&mut *tx).await.map_err(|e|e.to_string())?;
    if !managed {
        return Ok(None);
    }
    let owner: Option<(String, i64)> = sqlx::query_as("SELECT owner_instance_id,generation FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND status='active'")
        .bind(identity.provider()).bind(identity.raw_channel_id()).fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
    let (executor, generation) = owner.ok_or("managed channel has no active execution owner")?;
    if executor != local {
        return Err(format!("channel execution owner is {executor}"));
    }
    let binding: Option<Value> = sqlx::query_scalar("SELECT r.replay_preserved #> '{projection,binding}' FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id WHERE d.provider=$1 AND d.channel_id=$2 AND d.owner_instance_id=$3 AND d.owner_generation=$4 AND r.replay_preserved #> '{projection,binding}' IS NOT NULL ORDER BY d.id DESC LIMIT 1")
        .bind(identity.provider()).bind(identity.raw_channel_id()).bind(local).bind(generation).fetch_optional(&mut *tx).await.map_err(|e|e.to_string())?;
    let session_ref = match binding {
        Some(binding) => {
            validate_local(provider, channel, local, &binding)?;
            SessionRef::Bound(binding)
        }
        None if legacy_present()? => {
            return Err("local live session requires committed adoption".into());
        }
        None => SessionRef::Fresh,
    };
    tx.rollback().await.map_err(|e| e.to_string())?;
    Ok(Some(Arc::new(OwnerPermit {
        pool: pool.clone(),
        identity,
        executor,
        generation,
        session_ref,
        gate: Arc::new(OwnershipGate::for_execution(generation)),
    })))
}

fn validate_local(
    provider: &str,
    channel: u64,
    local: &str,
    binding: &Value,
) -> Result<(), String> {
    use crate::services::tui_prompt_dedupe::binding_context::{self, SpawnNonceMarker};
    let owner = binding["owner"]
        .as_str()
        .ok_or("original native owner unknown")?;
    let nonce = binding["execution_nonce"]
        .as_str()
        .ok_or("original native nonce unknown")?;
    if !matches!(binding["host"].as_str(), Some("tmux" | "process" | "herdr")) {
        return Err("original native host unknown".into());
    }
    if binding_context::observe_spawn_nonce_marker(owner) != SpawnNonceMarker::Known(nonce.into()) {
        return Err("original local native incarnation unavailable".into());
    }
    let context = binding_context::execution_context(provider, nonce)?;
    if context.tmux_session != owner
        || context.channel_id != Some(channel)
        || context.host.as_deref() != Some(local)
        || binding["root"] != context.owner_runtime_root
        || context.owner_runtime_root != crate::services::tmux_common::current_tmux_owner_marker()
    {
        return Err("original native context is not local to this owner".into());
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
