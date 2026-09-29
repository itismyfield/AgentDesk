use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::services::provider::ProviderCatalogEntry;

use super::registry::{MigrationState, ProviderCliChannel, SmokeResult};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderDiagnostics {
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<ProviderCliChannel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<ProviderCliChannel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<ProviderCliChannel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoke_current: Option<SmokeResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub smoke_candidate: Option<SmokeResult>,
    #[serde(default)]
    pub evidence: HashMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MigrationDiagnostics {
    pub provider: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canary_agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    pub history_len: usize,
}

/// Response body for `GET /api/provider-cli`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderCliStatusResponse {
    pub catalog: Vec<ProviderCatalogEntry>,
    pub providers: Vec<ProviderDiagnostics>,
    pub migrations: Vec<MigrationDiagnostics>,
    pub generated_at: DateTime<Utc>,
}

pub fn migration_state_wire_value(state: &MigrationState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{state:?}"))
}

/// Request body for `PATCH /api/provider-cli/{provider}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderCliActionRequest {
    /// "confirm_promote" | "rollback" | "rollback_to_previous"
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}
