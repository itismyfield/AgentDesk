use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::schema::{ObligationLedger, WholeCommit};
use super::state::{BlockedReason, SourceIdentityState};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::services::discord) struct LedgerDocument {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_schema: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ledger_protocol: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_frontier: Option<WholeCommit>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obligation_ledger: Option<ObligationLedger>,
    #[serde(flatten)]
    extensions: Map<String, Value>,
}

impl LedgerDocument {
    pub(in crate::services::discord) fn extensions(&self) -> &Map<String, Value> {
        &self.extensions
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum FenceState {
    Absent,
    Present,
    RecreateRequired,
    IncompleteClear,
}

/// Parsed snapshot; admission still requires durability and fence repair gates.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct LoadedLedger {
    pub document: LedgerDocument,
    pub identity: SourceIdentityState,
    pub fence: FenceState,
}

#[derive(Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum LedgerLoad {
    Loaded(Box<LoadedLedger>),
    Blocked(BlockedReason),
}

pub(super) fn read_optional(path: &Path) -> Result<Option<Vec<u8>>, BlockedReason> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(BlockedReason::Unavailable),
    }
}
