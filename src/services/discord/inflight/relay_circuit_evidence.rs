use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::InflightTurnState;
use crate::services::provider::ProviderKind;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub(in crate::services::discord) struct CircuitCoordinateToken(uuid::Uuid);

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(in crate::services::discord) enum CircuitCoordinateBinding {
    #[default]
    Unresolved,
    Bound {
        token: CircuitCoordinateToken,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::services::discord) struct CircuitSourceDescriptor {
    pub session_id: Option<String>,
    pub tmux_session_name: Option<String>,
    pub output_path: Option<String>,
    pub spawn_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_generation_mtime_ns: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_reset_incarnation: Option<u64>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::services::discord) struct CircuitEvidenceOrigin {
    pub episode_key: String,
    pub provider: String,
    pub channel_id: u64,
    pub offset_authority_channel_id: u64,
    pub open_obligation_id: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_baseline: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub admission_id: Option<uuid::Uuid>,
    pub source: CircuitSourceDescriptor,
    pub delivery_authority: String,
    #[serde(default)]
    pub binding: CircuitCoordinateBinding,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(in crate::services::discord) struct CircuitDeliveryEvidence {
    pub version: u32,
    pub evidence_id: String,
    pub origin: CircuitEvidenceOrigin,
    pub range_start: u64,
    pub confirmed_end_exclusive: u64,
    pub producer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirmed_at: Option<String>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub(in crate::services::discord) enum CircuitPendingEvidence {
    Known(Box<CircuitDeliveryEvidence>),
    Opaque(Value),
}

impl<'de> Deserialize<'de> for CircuitPendingEvidence {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Self, <D as serde::Deserializer<'de>>::Error> {
        let value = Value::deserialize(deserializer)?;
        if value.get("version").and_then(Value::as_u64) == Some(1)
            && let Ok(evidence) = serde_json::from_value::<CircuitDeliveryEvidence>(value.clone())
        {
            return Ok(Self::Known(Box::new(evidence)));
        }
        Ok(Self::Opaque(value))
    }
}

impl CircuitPendingEvidence {
    fn identity(&self) -> Option<&str> {
        match self {
            Self::Known(evidence) => Some(&evidence.evidence_id),
            Self::Opaque(value) => value.get("evidence_id").and_then(Value::as_str),
        }
    }
}

pub(in crate::services::discord) fn carry_forward(
    existing: Option<&InflightTurnState>,
    candidate: &mut InflightTurnState,
) -> Result<(), String> {
    for evidence in existing
        .into_iter()
        .flat_map(|row| &row.pending_circuit_evidence)
    {
        if let Some(present) =
            candidate
                .pending_circuit_evidence
                .iter()
                .find(|present| match evidence.identity() {
                    Some(identity) => present.identity() == Some(identity),
                    None => *present == evidence,
                })
        {
            if present != evidence {
                return Err("conflicting circuit evidence identity".into());
            }
        } else {
            candidate.pending_circuit_evidence.push(evidence.clone());
        }
    }
    Ok(())
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::services::discord) enum CircuitEvidenceHandoff {
    Journaled,
    Escrowed,
}

#[derive(Debug)]
enum EvidenceStoreError {
    Storage(String),
    IdentityConflict,
}

impl From<String> for EvidenceStoreError {
    fn from(detail: String) -> Self {
        Self::Storage(detail)
    }
}

impl From<&str> for EvidenceStoreError {
    fn from(detail: &str) -> Self {
        Self::Storage(detail.into())
    }
}

impl std::fmt::Display for EvidenceStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Storage(detail) => formatter.write_str(detail),
            Self::IdentityConflict => formatter.write_str("immutable circuit evidence collision"),
        }
    }
}

pub(in crate::services::discord) fn handoff(
    runtime: &Path,
    evidence: &CircuitDeliveryEvidence,
) -> Result<CircuitEvidenceHandoff, String> {
    match persist_evidence(runtime, "discord_circuit_evidence", evidence) {
        Ok(()) => Ok(CircuitEvidenceHandoff::Journaled),
        Err(EvidenceStoreError::IdentityConflict) => {
            Err("immutable circuit evidence collision".into())
        }
        Err(journal) => persist_evidence(runtime, "discord_circuit_evidence_escrow", evidence)
            .map(|()| CircuitEvidenceHandoff::Escrowed)
            .map_err(|escrow| {
                [
                    "circuit journal: ",
                    &journal.to_string(),
                    "; escrow: ",
                    &escrow.to_string(),
                ]
                .concat()
            }),
    }
}

fn persist_evidence(
    runtime: &Path,
    store: &str,
    evidence: &CircuitDeliveryEvidence,
) -> Result<(), EvidenceStoreError> {
    if !super::super::runtime_store::PARENT_DIR_FSYNC_FLUSHES {
        return Err("circuit evidence requires durable directory sync".into());
    }
    if evidence.version != 1
        || evidence.evidence_id.is_empty()
        || evidence.origin.episode_key.is_empty()
        || evidence.confirmed_end_exclusive <= evidence.range_start
    {
        return Err("unsupported or invalid circuit evidence; preserve its source".into());
    }
    let provider = ProviderKind::from_str(&evidence.origin.provider)
        .filter(|provider| provider.as_str() == evidence.origin.provider)
        .ok_or("invalid circuit evidence provider")?;
    let directory = runtime
        .join(store)
        .join(provider.as_str())
        .join(evidence.origin.channel_id.to_string());
    fs::create_dir_all(&directory).map_err(|error| error.to_string())?;
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.join("handoff.lock"))
        .map_err(|error| error.to_string())?;
    lock.lock().map_err(|error| error.to_string())?;
    let identity = blake3::hash(evidence.evidence_id.as_bytes()).to_hex();
    let path = directory.join(identity.as_str()).with_extension("json");
    match fs::read(&path) {
        Ok(bytes) => {
            let stored: CircuitDeliveryEvidence =
                serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
            if stored != *evidence {
                return Err(EvidenceStoreError::IdentityConflict);
            }
            fs::OpenOptions::new()
                .write(true)
                .open(&path)
                .and_then(|file| file.sync_all())
                .map_err(|error| error.to_string())?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let json = serde_json::to_string_pretty(evidence).map_err(|error| error.to_string())?;
            super::super::runtime_store::atomic_write(&path, &json)?;
        }
        Err(error) => return Err(error.to_string().into()),
    }
    // Flush the rename and directory chain through the runtime root.
    for child in path.ancestors().take(5) {
        super::super::runtime_store::fsync_parent_dir(child).map_err(|error| error.to_string())?;
    }
    Ok(())
}

pub(in crate::services::discord) fn episode_key(state: &InflightTurnState) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in [
        state.provider.as_bytes(),
        &state.channel_id.to_be_bytes(),
        state.channel_name.as_deref().unwrap_or("").as_bytes(),
        &state.request_owner_user_id.to_be_bytes(),
        &state.user_msg_id.to_be_bytes(),
        &state.current_msg_id.to_be_bytes(),
        &state.finalizer_turn_id.to_be_bytes(),
        state.started_at.as_bytes(),
        state.tmux_session_name.as_deref().unwrap_or("").as_bytes(),
        state.session_id.as_deref().unwrap_or("").as_bytes(),
        state.output_path.as_deref().unwrap_or("").as_bytes(),
        state.input_fifo_path.as_deref().unwrap_or("").as_bytes(),
        state
            .runtime_kind
            .map(|kind| kind.as_str())
            .unwrap_or("")
            .as_bytes(),
        state.effective_relay_owner_kind().as_str().as_bytes(),
        &state
            .turn_start_offset
            .unwrap_or(std::primitive::u64::MAX)
            .to_be_bytes(),
        &state.born_generation.to_be_bytes(),
        state.turn_nonce.as_deref().unwrap_or("").as_bytes(),
    ] {
        hasher.update(&(part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().to_hex().to_string()
}
