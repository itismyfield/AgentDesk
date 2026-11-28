//! A boot snapshot authorizes a local channel host; every raw effect still checks PG.
use super::OwnershipGate;
use crate::services::cluster::intake_router_hook::owner_record::OwnerIdentity;
use crate::services::tui_o::channel_policy::Candidate;
use serde_json::Value;
use sqlx::PgPool;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionRef {
    Fresh,
    Bound(Value),
}

pub struct OwnerPermit {
    pub pool: PgPool,
    pub identity: OwnerIdentity,
    pub executor: String,
    pub generation: i64,
    #[cfg(any(test, feature = "test-support"))]
    pub session_ref: SessionRef,
    #[cfg(not(any(test, feature = "test-support")))]
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
    pub fn gate(&self) -> Arc<OwnershipGate> {
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
    #[cfg(any(test, feature = "test-support"))]
    pub fn same_execution_for_test(&self, other: &Self) -> bool {
        self.same_execution(other)
    }

    pub(crate) fn same_execution(&self, other: &Self) -> bool {
        self.identity == other.identity
            && self.executor == other.executor
            && self.generation == other.generation
            && self.session_ref == other.session_ref
    }
}

#[derive(Clone, Debug)]
pub struct View {
    pub permit: Result<Arc<OwnerPermit>, String>,
    #[cfg(any(test, feature = "test-support"))]
    pub candidate: Candidate,
    #[cfg(not(any(test, feature = "test-support")))]
    pub(crate) candidate: Candidate,
    pub(crate) retained: Option<Arc<OwnerPermit>>,
}

impl View {
    pub(crate) fn held(reason: String) -> Self {
        Self {
            permit: Err(reason),
            candidate: Candidate::new(crate::services::tui_o::channel_policy::Adoption::Held),
            retained: None,
        }
    }
    pub fn verified(&self) -> Option<Arc<OwnerPermit>> {
        self.permit
            .as_ref()
            .ok()
            .cloned()
            .or_else(|| self.retained.clone())
    }
}

pub struct ReadError {
    pub reason: String,
    pub temporary: bool,
    /// The other node PG names as the channel's active execution owner.
    pub owner: Option<String>,
}
impl ReadError {
    pub fn temporary(reason: impl ToString) -> Self {
        Self {
            reason: reason.to_string(),
            temporary: true,
            owner: None,
        }
    }
    fn identity(reason: impl ToString) -> Self {
        Self {
            reason: reason.to_string(),
            temporary: false,
            owner: None,
        }
    }
}

/// None denotes an unmanaged channel; errors must install a held view, never Legacy.
#[cfg(any(test, feature = "test-support"))]
pub async fn read(
    pool: &PgPool,
    provider: &str,
    channel: u64,
    local: &str,
    legacy_present: impl FnOnce() -> Result<bool, String>,
) -> Result<Option<Arc<OwnerPermit>>, String> {
    read_observation(pool, provider, channel, local, legacy_present)
        .await
        .map_err(|e| e.reason)
}

pub async fn read_observation(
    pool: &PgPool,
    provider: &str,
    channel: u64,
    local: &str,
    legacy_present: impl FnOnce() -> Result<bool, String>,
) -> Result<Option<Arc<OwnerPermit>>, ReadError> {
    let identity = OwnerIdentity::new(provider, &channel.to_string());
    // Read all authority rows from one snapshot without waiting for effect custody.
    let mut tx = pool
        .begin_with("BEGIN ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .await
        .map_err(ReadError::temporary)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(ReadError::temporary)?;
    let managed: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM intake_outbox WHERE provider=$1 AND channel_id=$2 AND routing_receipt_id IS NOT NULL)")
        .bind(identity.provider()).bind(identity.raw_channel_id()).fetch_one(&mut *tx).await.map_err(ReadError::temporary)?;
    if !managed {
        return Ok(None);
    }
    let owner: Option<(String, i64)> = sqlx::query_as("SELECT owner_instance_id,generation FROM intake_session_owners WHERE provider=$1 AND raw_channel_id=$2 AND status='active'")
        .bind(identity.provider()).bind(identity.raw_channel_id()).fetch_optional(&mut *tx).await.map_err(ReadError::temporary)?;
    let (executor, generation) = owner
        .ok_or_else(|| ReadError::identity("managed channel has no active execution owner"))?;
    if executor != local {
        return Err(ReadError {
            owner: Some(executor.clone()),
            ..ReadError::identity(format!("channel execution owner is {executor}"))
        });
    }
    let binding: Option<Value> = sqlx::query_scalar("SELECT r.replay_preserved #> '{projection,binding}' FROM intake_outbox d JOIN intake_outbox r ON r.id=d.routing_receipt_id WHERE d.provider=$1 AND d.channel_id=$2 AND d.owner_instance_id=$3 AND d.owner_generation=$4 AND r.replay_preserved #> '{projection,binding}' IS NOT NULL ORDER BY d.id DESC LIMIT 1")
        .bind(identity.provider()).bind(identity.raw_channel_id()).bind(local).bind(generation).fetch_optional(&mut *tx).await.map_err(ReadError::temporary)?;
    let session_ref = match binding {
        Some(binding) => {
            validate_local(provider, channel, local, &binding).map_err(|error| ReadError {
                temporary: matches!(error, NativeRefFailure::Unavailable),
                reason: format!("original native incarnation ref: {error:?}"),
                owner: None,
            })?;
            SessionRef::Bound(binding)
        }
        None if legacy_present().map_err(ReadError::temporary)? => {
            return Err(ReadError::identity(
                "local live session requires committed adoption",
            ));
        }
        None => SessionRef::Fresh,
    };
    tx.rollback().await.map_err(ReadError::temporary)?;
    Ok(Some(Arc::new(OwnerPermit {
        pool: pool.clone(),
        identity,
        executor,
        generation,
        session_ref,
        gate: Arc::new(OwnershipGate::for_execution(generation)),
    })))
}

#[derive(Debug)]
pub enum NativeRefFailure {
    Malformed,
    Unavailable,
    Changed,
}

pub fn validate_local(
    provider: &str,
    channel: u64,
    local: &str,
    binding: &Value,
) -> Result<(), NativeRefFailure> {
    use crate::services::tui_prompt_dedupe::binding_context::{self, SpawnNonceMarker};
    let owner = binding["owner"]
        .as_str()
        .ok_or(NativeRefFailure::Malformed)?;
    let nonce = binding["execution_nonce"]
        .as_str()
        .ok_or(NativeRefFailure::Malformed)?;
    if !matches!(binding["host"].as_str(), Some("tmux" | "process" | "herdr")) {
        return Err(NativeRefFailure::Malformed);
    }
    if binding_context::observe_spawn_nonce_marker(owner) != SpawnNonceMarker::Known(nonce.into()) {
        return Err(
            if binding_context::observe_spawn_nonce_marker(owner) == SpawnNonceMarker::Unreadable {
                NativeRefFailure::Unavailable
            } else {
                NativeRefFailure::Changed
            },
        );
    }
    let context = binding_context::execution_context(provider, nonce)
        .map_err(|_| NativeRefFailure::Unavailable)?;
    if context.tmux_session != owner
        || context.channel_id != Some(channel)
        || context.host.as_deref() != Some(local)
        || binding["root"] != context.owner_runtime_root
        || context.owner_runtime_root != crate::services::tmux_common::current_tmux_owner_marker()
    {
        return Err(NativeRefFailure::Changed);
    }
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
pub mod tests;
