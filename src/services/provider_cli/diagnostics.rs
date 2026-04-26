use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use super::registry::{MigrationState, ProviderCliChannel, ProviderCliMigrationState, SmokeResult};

/// Top-level diagnostics snapshot for all providers.
///
/// Stored at `~/.adk/{env}/runtime/provider-cli-diagnostics/{timestamp_ms}.json`.
/// Optional consumers (skills, watchers) may read this file; AgentDesk core
/// migration does not depend on them reading it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DiagnosticsSnapshot {
    pub generated_at: DateTime<Utc>,
    pub providers: Vec<ProviderDiagnostics>,
    pub active_sessions: Vec<SessionDiagnostics>,
    pub migrations: Vec<MigrationDiagnostics>,
}

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
pub struct SessionDiagnostics {
    pub agent_id: String,
    pub provider: String,
    pub channel: String,
    pub cli_path: String,
    pub cli_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux_session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_id: Option<u32>,
    pub runtime_consistency: RuntimeConsistency,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeConsistency {
    /// Launch artifact path matches live process.
    Consistent,
    /// Launch artifact exists but process not detected.
    ProcessNotFound,
    /// No launch artifact for this session.
    NoArtifact,
    /// Path in launch artifact differs from live process.
    Mismatch,
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
    #[serde(default)]
    pub force_recreate_active: bool,
}

/// Build a `DiagnosticsSnapshot` from available in-memory data.
pub fn build_snapshot(
    provider_diagnostics: Vec<ProviderDiagnostics>,
    session_diagnostics: Vec<SessionDiagnostics>,
    migration_states: &[ProviderCliMigrationState],
) -> DiagnosticsSnapshot {
    let migrations = migration_states
        .iter()
        .map(|s| MigrationDiagnostics {
            provider: s.provider.clone(),
            state: migration_state_wire_value(&s.state),
            canary_agent_id: s.selected_agent_id.clone(),
            started_at: Some(s.started_at),
            updated_at: Some(s.updated_at),
            history_len: s.history.len(),
        })
        .collect();

    DiagnosticsSnapshot {
        generated_at: Utc::now(),
        providers: provider_diagnostics,
        active_sessions: session_diagnostics,
        migrations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_response_serializes() {
        let resp = ProviderCliStatusResponse {
            providers: vec![],
            migrations: vec![],
            generated_at: Utc::now(),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("providers"));
        assert!(json.contains("generated_at"));
    }

    #[test]
    fn action_request_roundtrip() {
        let req = ProviderCliActionRequest {
            action: "confirm_promote".to_string(),
            evidence: Some("operator approved".to_string()),
            force_recreate_active: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let decoded: ProviderCliActionRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.action, "confirm_promote");
    }

    #[test]
    fn build_snapshot_maps_migration_states() {
        use crate::services::provider_cli::registry::MigrationState;
        use chrono::Utc;

        let state = crate::services::provider_cli::registry::ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::CanaryActive,
            selected_agent_id: Some("codex-1".to_string()),
            current_channel: None,
            candidate_channel: None,
            rollback_target: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            history: vec![],
        };

        let snapshot = build_snapshot(vec![], vec![], &[state]);
        assert_eq!(snapshot.migrations.len(), 1);
        assert_eq!(snapshot.migrations[0].provider, "codex");
        assert_eq!(snapshot.migrations[0].state, "canary_active");
    }
}
