//! Session-lifetime hosted execution record in `sessions.hosted_execution`. NULL is legacy;
//! an unreadable payload is `Unknown` and keeps the row. Writes CAS the exact value last read.
#![cfg_attr(not(test), allow(dead_code))]

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Postgres, Row, Transaction};

use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityConflictKind, SessionIdentityKind,
    resolve_session_row_pg,
};

pub(crate) const HOSTED_EXECUTION_SCHEMA: u32 = 1;
const HERDR_HOST: &str = "herdr";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum HostedState {
    Pending,
    Bound,
    Retired,
}

/// Canonical owner; provider, token hash and channel must equal the row's identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostedOwner {
    pub provider: String,
    pub discord_token_hash: String,
    pub channel_id: String,
    pub logical_key: String,
    pub owner_node: String,
    pub runtime_root: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostedLocation {
    pub host: String,
    pub execution_node: String,
    pub endpoint_config_key: String,
    pub socket_addr: String,
    pub named_session: String,
    pub pane_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProcessStamp {
    pub pid: u32,
    pub start: String,
}

/// Execution evidence recorded by the launch that created the pane; later
/// observations are compared against it and never replace it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExpectedExecution {
    pub binding_provider: String,
    pub binding_nonce: String,
    pub root: ProcessStamp,
    pub provider_process: ProcessStamp,
    pub provenance: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceRef {
    pub runtime_root: String,
    pub channel: String,
    pub provider: String,
    pub logical_key: String,
    pub execution_nonce: String,
    #[serde(deserialize_with = "present")]
    pub initial_source: Option<String>,
    #[serde(deserialize_with = "present")]
    pub baseline_event_seq: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HostedExecution {
    pub schema: u32,
    pub state: HostedState,
    pub execution_nonce: String,
    pub owner: HostedOwner,
    #[serde(deserialize_with = "present")]
    pub location: Option<HostedLocation>,
    #[serde(deserialize_with = "present")]
    pub expected: Option<ExpectedExecution>,
    pub source_ref: SourceRef,
}

/// Optional fields must still be written as `null`; a missing key is field loss.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(de: D) -> Result<Option<T>, D::Error> {
    Option::<T>::deserialize(de)
}

fn filled(values: &[&str]) -> bool {
    values.iter().all(|value| !value.trim().is_empty())
}

impl HostedExecution {
    /// A new incarnation's first record: no pane or process evidence yet.
    pub(crate) fn pending(
        owner: HostedOwner,
        execution_nonce: String,
        source_ref: SourceRef,
    ) -> Self {
        Self {
            schema: HOSTED_EXECUTION_SCHEMA,
            state: HostedState::Pending,
            execution_nonce,
            owner,
            location: None,
            expected: None,
            source_ref,
        }
    }

    fn is_consistent(&self) -> bool {
        let owner = &self.owner;
        let source = &self.source_ref;
        let location_ok = self.location.as_ref().is_none_or(|l| {
            l.host == HERDR_HOST
                && filled(&[&l.execution_node, &l.endpoint_config_key, &l.socket_addr])
                && filled(&[&l.named_session, &l.pane_id])
        });
        let expected_ok = self.expected.as_ref().is_none_or(|e| {
            e.binding_provider == owner.provider
                && e.binding_nonce == self.execution_nonce
                && e.root.pid > 0
                && e.provider_process.pid > 0
                && filled(&[&e.root.start, &e.provider_process.start, &e.provenance])
        });
        let bound_ok = self.state != HostedState::Bound
            || (self.location.is_some() && self.expected.is_some());
        self.schema == HOSTED_EXECUTION_SCHEMA
            && filled(&[
                &self.execution_nonce,
                &owner.provider,
                &owner.discord_token_hash,
            ])
            && filled(&[&owner.channel_id, &owner.logical_key, &owner.owner_node])
            && filled(&[&owner.runtime_root])
            && source.execution_nonce == self.execution_nonce
            && source.provider == owner.provider
            && source.channel == owner.channel_id
            && source.logical_key == owner.logical_key
            && source.runtime_root == owner.runtime_root
            && source
                .initial_source
                .as_deref()
                .is_none_or(|s| filled(&[s]))
            && location_ok
            && expected_ok
            && bound_ok
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HostedRecord {
    /// SQL NULL: the row predates hosted execution and keeps its legacy meaning.
    Legacy,
    Known(HostedExecution),
    /// Future schema, lost or extra field, or inconsistent content, kept verbatim.
    Unknown(Value),
}

impl HostedRecord {
    pub(crate) fn decode(raw: Option<&Value>) -> Self {
        let Some(raw) = raw else {
            return Self::Legacy;
        };
        // serde reads a positional array into a struct; only an object can be a record.
        let known = raw
            .is_object()
            .then(|| serde_json::from_value::<HostedExecution>(raw.clone()).ok())
            .flatten()
            .filter(HostedExecution::is_consistent);
        known.map_or_else(|| Self::Unknown(raw.clone()), Self::Known)
    }

    /// Only a legacy row or a readable retired record may leave through ordinary cleanup.
    pub(crate) fn deletable(&self) -> bool {
        match self {
            Self::Legacy => true,
            Self::Known(record) => record.state == HostedState::Retired,
            Self::Unknown(_) => false,
        }
    }
}

/// One row's record as read; the raw value is the CAS expectation for the next write.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HostedObservation {
    session_id: i64,
    raw: Option<Value>,
    pub(crate) record: HostedRecord,
}

impl HostedObservation {
    pub(crate) fn session_id(&self) -> i64 {
        self.session_id
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum HostedLookupKey<'a> {
    /// Full primary or alias session key.
    SessionKey(&'a str),
    Canonical {
        provider: &'a str,
        identity: CanonicalSessionIdentity<'a>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HostedLookup {
    Found(HostedObservation),
    Missing,
    /// Incomplete key or database failure; never a legacy answer.
    Unknown(String),
    Conflict(SessionIdentityConflictKind),
}

/// Resolve exactly one sessions row by full key or exact canonical tuple, then read its record.
pub(crate) async fn load_hosted_execution_pg(
    pool: &PgPool,
    key: HostedLookupKey<'_>,
) -> HostedLookup {
    let resolved = match key {
        HostedLookupKey::SessionKey(session_key) if filled(&[session_key]) => {
            resolve_session_row_pg(pool, Some(session_key), None, None).await
        }
        HostedLookupKey::Canonical { provider, identity }
            if identity.kind == SessionIdentityKind::DiscordChannel
                && filled(&[provider, identity.discord_token_hash, identity.channel_id]) =>
        {
            resolve_session_row_pg(pool, None, Some(provider), Some(identity)).await
        }
        _ => return HostedLookup::Unknown("incomplete hosted execution lookup key".to_string()),
    };
    let session_id = match resolved {
        Ok(Some((id, _))) => id,
        Ok(None) => return HostedLookup::Missing,
        Err(error) => {
            return error.conflict_kind().map_or_else(
                || HostedLookup::Unknown(format!("{error:?}")),
                HostedLookup::Conflict,
            );
        }
    };
    let row = sqlx::query(
        "SELECT provider, identity_kind, discord_token_hash, channel_id, hosted_execution
         FROM sessions WHERE id = $1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await;
    let row = match row {
        Ok(Some(row)) => row,
        Ok(None) => return HostedLookup::Missing,
        Err(error) => return HostedLookup::Unknown(format!("load hosted execution: {error}")),
    };
    let decoded = (|| -> Result<_, sqlx::Error> {
        let identity: [Option<String>; 4] = [
            row.try_get("provider")?,
            row.try_get("identity_kind")?,
            row.try_get("discord_token_hash")?,
            row.try_get("channel_id")?,
        ];
        Ok((
            identity,
            row.try_get::<Option<Value>, _>("hosted_execution")?,
        ))
    })();
    let (identity, raw) = match decoded {
        Ok(decoded) => decoded,
        Err(error) => return HostedLookup::Unknown(format!("decode hosted execution: {error}")),
    };
    let record = HostedRecord::decode(raw.as_ref());
    if let HostedRecord::Known(known) = &record
        && !owner_matches_row(&known.owner, &identity)
    {
        return HostedLookup::Conflict(SessionIdentityConflictKind::OwnershipMismatch);
    }
    HostedLookup::Found(HostedObservation {
        session_id,
        raw,
        record,
    })
}

fn owner_matches_row(owner: &HostedOwner, identity: &[Option<String>; 4]) -> bool {
    let expected = [
        owner.provider.as_str(),
        SessionIdentityKind::DiscordChannel.as_str(),
        owner.discord_token_hash.as_str(),
        owner.channel_id.as_str(),
    ];
    identity
        .iter()
        .zip(expected)
        .all(|(column, value)| column.as_deref() == Some(value))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HostedCasOutcome {
    Written,
    /// The row, its canonical identity or its record changed since the observation.
    Stale,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum HostedTransitionError {
    UnknownRecord,
    NotAllowed(Option<HostedState>),
    NonceMismatch,
    OwnerMismatch,
    /// Stored location or expected execution differs from the value offered.
    ExpectedOverwrite,
    Incomplete,
    Database(String),
}

/// Install a new incarnation's Pending record over a legacy row or a retired older nonce.
pub(crate) async fn install_pending_pg(
    pool: &PgPool,
    observed: &HostedObservation,
    next: HostedExecution,
) -> Result<HostedCasOutcome, HostedTransitionError> {
    let next = plan_install(&observed.record, next)?;
    compare_and_set_pg(pool, observed, &next).await
}

/// Fill the pane location and launch evidence once, only for the same Pending nonce.
pub(crate) async fn record_launch_evidence_pg(
    pool: &PgPool,
    observed: &HostedObservation,
    owner: &HostedOwner,
    execution_nonce: &str,
    location: HostedLocation,
    expected: ExpectedExecution,
) -> Result<HostedCasOutcome, HostedTransitionError> {
    let current = same_execution(&observed.record, owner, execution_nonce)?;
    if current.state != HostedState::Pending {
        return Err(HostedTransitionError::NotAllowed(Some(current.state)));
    }
    let mut next = current.clone();
    fill_once(&mut next.location, location)?;
    fill_once(&mut next.expected, expected)?;
    compare_and_set_pg(pool, observed, &next).await
}

/// Pending with complete launch evidence becomes Bound for the same nonce.
pub(crate) async fn bind_pg(
    pool: &PgPool,
    observed: &HostedObservation,
    owner: &HostedOwner,
    execution_nonce: &str,
) -> Result<HostedCasOutcome, HostedTransitionError> {
    let next = plan_state_change(&observed.record, owner, execution_nonce, HostedState::Bound)?;
    compare_and_set_pg(pool, observed, &next).await
}

/// Retire the confirmed-ended execution of this nonce; any other nonce is refused.
pub(crate) async fn retire_pg(
    pool: &PgPool,
    observed: &HostedObservation,
    owner: &HostedOwner,
    execution_nonce: &str,
) -> Result<HostedCasOutcome, HostedTransitionError> {
    let next = plan_state_change(
        &observed.record,
        owner,
        execution_nonce,
        HostedState::Retired,
    )?;
    compare_and_set_pg(pool, observed, &next).await
}

fn plan_install(
    current: &HostedRecord,
    next: HostedExecution,
) -> Result<HostedExecution, HostedTransitionError> {
    match current {
        HostedRecord::Unknown(_) => return Err(HostedTransitionError::UnknownRecord),
        HostedRecord::Known(old) if old.state != HostedState::Retired => {
            return Err(HostedTransitionError::NotAllowed(Some(old.state)));
        }
        HostedRecord::Known(old) if old.execution_nonce == next.execution_nonce => {
            return Err(HostedTransitionError::NonceMismatch);
        }
        HostedRecord::Known(_) | HostedRecord::Legacy => {}
    }
    let fresh =
        next.state == HostedState::Pending && next.location.is_none() && next.expected.is_none();
    if !fresh || !next.is_consistent() {
        return Err(HostedTransitionError::Incomplete);
    }
    Ok(next)
}

fn same_execution<'a>(
    current: &'a HostedRecord,
    owner: &HostedOwner,
    execution_nonce: &str,
) -> Result<&'a HostedExecution, HostedTransitionError> {
    let current = match current {
        HostedRecord::Known(current) => current,
        HostedRecord::Legacy => return Err(HostedTransitionError::NotAllowed(None)),
        HostedRecord::Unknown(_) => return Err(HostedTransitionError::UnknownRecord),
    };
    if current.owner != *owner {
        return Err(HostedTransitionError::OwnerMismatch);
    }
    if current.execution_nonce != execution_nonce {
        return Err(HostedTransitionError::NonceMismatch);
    }
    Ok(current)
}

fn fill_once<T: PartialEq>(slot: &mut Option<T>, value: T) -> Result<(), HostedTransitionError> {
    match slot {
        Some(stored) if *stored != value => Err(HostedTransitionError::ExpectedOverwrite),
        Some(_) => Ok(()),
        None => {
            *slot = Some(value);
            Ok(())
        }
    }
}

fn plan_state_change(
    current: &HostedRecord,
    owner: &HostedOwner,
    execution_nonce: &str,
    target: HostedState,
) -> Result<HostedExecution, HostedTransitionError> {
    let current = same_execution(current, owner, execution_nonce)?;
    let allowed = match target {
        HostedState::Bound => current.state == HostedState::Pending,
        HostedState::Retired => current.state != HostedState::Retired,
        HostedState::Pending => false,
    };
    if !allowed {
        return Err(HostedTransitionError::NotAllowed(Some(current.state)));
    }
    let next = HostedExecution {
        state: target,
        ..current.clone()
    };
    if !next.is_consistent() {
        return Err(HostedTransitionError::Incomplete);
    }
    Ok(next)
}

/// Single-statement CAS on row id, canonical identity and the exact observed value.
async fn compare_and_set_pg(
    pool: &PgPool,
    observed: &HostedObservation,
    next: &HostedExecution,
) -> Result<HostedCasOutcome, HostedTransitionError> {
    let payload = serde_json::to_value(next).map_err(|error| {
        HostedTransitionError::Database(format!("encode hosted execution: {error}"))
    })?;
    let updated = sqlx::query(
        "UPDATE sessions SET hosted_execution = $2
         WHERE id = $1
           AND hosted_execution IS NOT DISTINCT FROM $3::JSONB
           AND identity_kind = 'discord_channel'
           AND provider = $4
           AND discord_token_hash = $5
           AND channel_id = $6",
    )
    .bind(observed.session_id)
    .bind(payload)
    .bind(observed.raw.clone())
    .bind(&next.owner.provider)
    .bind(&next.owner.discord_token_hash)
    .bind(&next.owner.channel_id)
    .execute(pool)
    .await
    .map_err(|error| HostedTransitionError::Database(format!("write hosted execution: {error}")))?;
    Ok(if updated.rows_affected() == 1 {
        HostedCasOutcome::Written
    } else {
        HostedCasOutcome::Stale
    })
}

/// Bulk cleanup of disconnected rows. Legacy rows go in one statement; each
/// hosted row is read, decoded and deleted only if its value is still the one read.
pub(crate) async fn delete_disconnected_sessions_pg(pool: &PgPool) -> Result<u64, String> {
    let mut deleted = sqlx::query(
        "DELETE FROM sessions WHERE status = 'disconnected' AND hosted_execution IS NULL",
    )
    .execute(pool)
    .await
    .map_err(|error| format!("{error}"))?
    .rows_affected();
    let hosted: Vec<(i64, Value)> = sqlx::query_as(
        "SELECT id, hosted_execution FROM sessions
         WHERE status = 'disconnected' AND hosted_execution IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .map_err(|error| format!("{error}"))?;
    for (id, raw) in hosted {
        if !HostedRecord::decode(Some(&raw)).deletable() {
            continue;
        }
        deleted += sqlx::query(
            "DELETE FROM sessions
             WHERE id = $1 AND status = 'disconnected' AND hosted_execution = $2::JSONB
               AND agentdesk_hosted_execution_deletable(hosted_execution)",
        )
        .bind(id)
        .bind(raw)
        .execute(pool)
        .await
        .map_err(|error| format!("{error}"))?
        .rows_affected();
    }
    Ok(deleted)
}

/// Explicit delete of a row already locked by the caller's locator resolution.
/// A live or unreadable record refuses the delete instead of reporting zero rows.
pub(crate) async fn delete_locked_session_pg(
    tx: &mut Transaction<'_, Postgres>,
    session_id: i64,
) -> Result<u64, String> {
    let raw: Option<Option<Value>> =
        sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE id = $1")
            .bind(session_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|error| format!("load session hosted execution: {error}"))?;
    let Some(raw) = raw else {
        return Ok(0);
    };
    if !HostedRecord::decode(raw.as_ref()).deletable() {
        return Err(format!(
            "session {session_id} keeps a live or unreadable hosted execution record"
        ));
    }
    sqlx::query(
        "DELETE FROM sessions
         WHERE id = $1 AND agentdesk_hosted_execution_deletable(hosted_execution)",
    )
    .bind(session_id)
    .execute(&mut **tx)
    .await
    .map(|result| result.rows_affected())
    .map_err(|error| format!("delete postgres session: {error}"))
}

#[cfg(test)]
#[path = "hosted_execution_tests.rs"]
pub(crate) mod tests;
