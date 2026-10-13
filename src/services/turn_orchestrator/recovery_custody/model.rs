//! The typed custody row. Phase strings and JSON shapes are tied to [`FORMAT_VERSION`]; a row in
//! any other shape decodes to [`Inspection::Unsupported`], which callers treat as held.

use serde::{Deserialize, Serialize};

use super::operation::ActiveOperation;
use crate::db::channel_recovery_custody::StoredCustody;

pub(crate) const FORMAT_VERSION: i32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    LegacyReserved,
    LegacyOwned,
    ScopedReserved,
    Installing,
    Installed,
    Held,
    Closed,
}

impl Phase {
    const ALL: [Self; 7] = [
        Self::LegacyReserved,
        Self::LegacyOwned,
        Self::ScopedReserved,
        Self::Installing,
        Self::Installed,
        Self::Held,
        Self::Closed,
    ];

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::LegacyReserved => "legacy_reserved",
            Self::LegacyOwned => "legacy_owned",
            Self::ScopedReserved => "scoped_reserved",
            Self::Installing => "installing",
            Self::Installed => "installed",
            Self::Held => "held",
            Self::Closed => "closed",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|phase| phase.as_str() == raw)
    }
}

/// The home fence a responsibility was admitted under. A no-home legacy channel has none; no
/// placeholder epoch stands in for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HomeIdentity {
    Absent,
    Present { epoch: i64 },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Namespace {
    pub(crate) provider: String,
    pub(crate) token_hash: String,
}

/// Who currently answers for the channel; never derived from a PID or a reference count.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Owner {
    pub(crate) node: String,
    pub(crate) process_incarnation: String,
    pub(crate) attempt_id: String,
}

/// An immutable install identity; freshness (current message, path, offsets) is kept apart.
/// `nonce: None` and `Some("")` are different identities.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "v", deny_unknown_fields)]
pub(crate) enum IssuedEpisode {
    #[serde(rename = "1")]
    V1 {
        provider: String,
        channel: u64,
        request_owner: u64,
        user_msg_id: u64,
        nonce: Option<String>,
        born_generation: u64,
        started_at: String,
    },
}

/// One canonical store as last observed by its owning node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum Observed {
    Present { identity: String, digest: String },
    Absent,
    Unreadable,
}

/// The four canonical stores with the node and namespace that own them; Marker and Queue are
/// sealed separately because a legacy dequeue publishes them in two writes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CanonicalSeal {
    pub(crate) owner_node: String,
    pub(crate) namespace: Namespace,
    pub(crate) queue: Observed,
    pub(crate) marker: Observed,
    pub(crate) inflight: Observed,
    pub(crate) placeholder: Observed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CustodyRecord {
    pub(crate) channel_id: String,
    pub(crate) revision: i64,
    pub(crate) namespace: Namespace,
    pub(crate) owner: Owner,
    pub(crate) home: HomeIdentity,
    pub(crate) activation_generation: Option<i64>,
    pub(crate) issued_episode: Option<IssuedEpisode>,
    pub(crate) phase: Phase,
    pub(crate) active_operation: Option<ActiveOperation>,
    pub(crate) canonical_seal: CanonicalSeal,
    /// Read back from the row; writes cannot clear it (the store ORs in scoped phases).
    pub(crate) ever_scoped: bool,
}

/// What a read found. `Unsupported` keeps the stored row untouched and counts as held.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Inspection {
    Absent,
    Row(CustodyRecord),
    Unsupported { revision: i64, detail: String },
}

impl Inspection {
    pub(crate) fn logical_phase(&self) -> Option<Phase> {
        match self {
            Self::Absent => None,
            Self::Row(record) => Some(record.phase),
            Self::Unsupported { .. } => Some(Phase::Held),
        }
    }
}

fn json<T: Serialize>(value: &T) -> Result<serde_json::Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}

fn from_json<T: for<'de> Deserialize<'de>>(
    field: &str,
    value: serde_json::Value,
) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| format!("{field}: {error}"))
}

impl CustodyRecord {
    pub(crate) fn decode(stored: StoredCustody) -> Inspection {
        let revision = stored.revision;
        match Self::decode_fields(stored) {
            Ok(record) => Inspection::Row(record),
            Err(detail) => Inspection::Unsupported { revision, detail },
        }
    }

    fn decode_fields(stored: StoredCustody) -> Result<Self, String> {
        if stored.format_version != FORMAT_VERSION {
            return Err(format!("format_version {}", stored.format_version));
        }
        let phase = Phase::parse(&stored.phase).ok_or(format!("phase {:?}", stored.phase))?;
        let home = match stored.home_epoch {
            Some(epoch) => HomeIdentity::Present { epoch },
            None => HomeIdentity::Absent,
        };
        Ok(Self {
            channel_id: stored.channel_id,
            revision: stored.revision,
            namespace: Namespace {
                provider: stored.provider,
                token_hash: stored.token_hash,
            },
            owner: Owner {
                node: stored.owner_node,
                process_incarnation: stored.process_incarnation,
                attempt_id: stored.attempt_id,
            },
            home,
            activation_generation: stored.activation_generation,
            issued_episode: stored
                .issued_episode
                .map(|value| from_json("issued_episode", value))
                .transpose()?,
            phase,
            active_operation: stored
                .active_operation
                .map(|value| from_json("active_operation", value))
                .transpose()?,
            canonical_seal: from_json("canonical_seal", stored.canonical_seal)?,
            ever_scoped: stored.ever_scoped,
        })
    }

    /// The row to store; `revision` and `ever_scoped` are the store's to compute.
    pub(crate) fn encode(&self) -> Result<StoredCustody, String> {
        if let Some(operation) = &self.active_operation {
            operation.check_bounds()?;
        }
        Ok(StoredCustody {
            channel_id: self.channel_id.clone(),
            revision: self.revision,
            provider: self.namespace.provider.clone(),
            token_hash: self.namespace.token_hash.clone(),
            owner_node: self.owner.node.clone(),
            process_incarnation: self.owner.process_incarnation.clone(),
            attempt_id: self.owner.attempt_id.clone(),
            home_epoch: match self.home {
                HomeIdentity::Present { epoch } => Some(epoch),
                HomeIdentity::Absent => None,
            },
            activation_generation: self.activation_generation,
            issued_episode: self.issued_episode.as_ref().map(json).transpose()?,
            phase: self.phase.as_str().to_string(),
            active_operation: self.active_operation.as_ref().map(json).transpose()?,
            canonical_seal: json(&self.canonical_seal)?,
            ever_scoped: self.ever_scoped,
            format_version: FORMAT_VERSION,
        })
    }
}
