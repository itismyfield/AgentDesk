use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// A named channel snapshot for a single provider binary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCliChannel {
    /// Resolved binary path at registration time.
    pub path: String,
    /// Canonical (symlink-resolved) binary path.
    pub canonical_path: String,
    /// Version string returned by `<binary> --version`.
    pub version: String,
    /// Full version output (may differ from trimmed `version`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_output: Option<String>,
    /// Resolution source: env_override / current_path / login_shell_path / fallback_path.
    pub source: String,
    pub checked_at: DateTime<Utc>,
    /// Freeform evidence bag (e.g. smoke status, binary hash).
    #[serde(default)]
    pub evidence: HashMap<String, String>,
}

/// Per-provider channel map. Keys are "current", "candidate", "default", "previous".
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderChannels {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<ProviderCliChannel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate: Option<ProviderCliChannel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<ProviderCliChannel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous: Option<ProviderCliChannel>,
    /// Per-agent channel override. Key is agent_id; value is channel name.
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub agent_overrides: HashMap<String, String>,
}

/// Root registry file. Stored at `~/.adk/{env}/config/provider-cli-registry.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCliRegistry {
    pub schema_version: u32,
    /// Key is provider id: "codex", "claude", "gemini", "qwen".
    #[serde(default)]
    pub providers: HashMap<String, ProviderChannels>,
    pub updated_at: DateTime<Utc>,
}

impl Default for ProviderCliRegistry {
    fn default() -> Self {
        Self {
            schema_version: 1,
            providers: HashMap::new(),
            updated_at: Utc::now(),
        }
    }
}

impl ProviderCliRegistry {
    /// Returns the channel override for a specific agent within a provider, if any.
    pub fn agent_channel(&self, provider: &str, agent_id: &str) -> Option<&str> {
        self.providers
            .get(provider)?
            .agent_overrides
            .get(agent_id)
            .map(|s| s.as_str())
    }
}

/// Migration state for one provider.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MigrationState {
    Planned,
    CurrentSnapshotted,
    SmokeCurrentPassed,
    PreviousPreserved,
    UpgradePlanned,
    UpgradeSucceeded,
    CandidateDiscovered,
    SmokeCandidatePassed,
    CanarySelected,
    CanarySessionSafeEnding,
    CanarySessionRecreated,
    CanaryActive,
    CanaryPassed,
    AwaitingOperatorPromote,
    ProviderSessionsSafeEnding,
    ProviderSessionsRecreated,
    ProviderAgentsMigrated,
    RolledBack,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationHistoryEntry {
    pub from_state: MigrationState,
    pub to_state: MigrationState,
    pub transitioned_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// Persisted migration state file. Stored at
/// `~/.adk/{env}/state/provider-cli-migration.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCliMigrationState {
    pub schema_version: u32,
    pub provider: String,
    pub state: MigrationState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected_agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_channel: Option<ProviderCliChannel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidate_channel: Option<ProviderCliChannel>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback_target: Option<String>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default)]
    pub history: Vec<MigrationHistoryEntry>,
}

/// Smoke test result for a single provider binary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SmokeCheckStatus {
    Ok,
    Failed,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmokeChecks {
    pub version: SmokeCheckStatus,
    pub auth: SmokeCheckStatus,
    pub simple: SmokeCheckStatus,
    pub structured: SmokeCheckStatus,
    pub stream: SmokeCheckStatus,
    pub resume: SmokeCheckStatus,
    pub cancel: SmokeCheckStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmokeResult {
    pub provider: String,
    pub channel: String,
    pub candidate_path: String,
    pub canonical_path: String,
    pub checks: SmokeChecks,
    /// "ok" | "failed" | "partial"
    pub overall_status: String,
    #[serde(default)]
    pub evidence: HashMap<String, String>,
    pub checked_at: DateTime<Utc>,
}

/// Per-session launch artifact. Stored at
/// `~/.adk/{env}/runtime/provider-cli-launch/<session_key>.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LaunchArtifact {
    pub provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    /// Registry channel used at launch: "current", "candidate", "default".
    pub channel: String,
    pub cli_path: String,
    pub canonical_path: String,
    pub cli_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tmux_session: Option<String>,
    pub launched_at: DateTime<Utc>,
}

/// Allowlisted update strategy for a single provider.
/// Defined as compile-time constants only — not deserialized from files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProviderCliUpdateStrategy {
    pub provider: &'static str,
    pub install_source: &'static str,
    /// Full argv for the update command. No shell expansion.
    pub command_argv: &'static [&'static str],
    pub expected_binary_name: &'static str,
    /// True when upgrade overwrites the binary at the same canonical path.
    /// Requires previous binary to be preserved BEFORE running the upgrade.
    pub mutates_in_place: bool,
    /// True when the package manager is expected to move the canonical binary
    /// path during upgrade while the previous binary still needs preservation.
    pub allow_candidate_path_change: bool,
}

pub const PROVIDER_UPDATE_STRATEGIES: &[ProviderCliUpdateStrategy] = &[
    ProviderCliUpdateStrategy {
        provider: "codex",
        install_source: "npm-global",
        command_argv: &["npm", "install", "-g", "@openai/codex"],
        expected_binary_name: "codex",
        mutates_in_place: true,
        allow_candidate_path_change: false,
    },
    ProviderCliUpdateStrategy {
        provider: "claude",
        install_source: "npm-global",
        command_argv: &["npm", "install", "-g", "@anthropic-ai/claude-code"],
        expected_binary_name: "claude",
        mutates_in_place: true,
        allow_candidate_path_change: false,
    },
    ProviderCliUpdateStrategy {
        provider: "gemini",
        install_source: "npm-global",
        command_argv: &["npm", "install", "-g", "@google/gemini-cli"],
        expected_binary_name: "gemini",
        mutates_in_place: true,
        allow_candidate_path_change: false,
    },
    ProviderCliUpdateStrategy {
        provider: "opencode",
        install_source: "npm-global",
        command_argv: &["npm", "install", "-g", "opencode-ai"],
        expected_binary_name: "opencode",
        mutates_in_place: true,
        allow_candidate_path_change: false,
    },
    ProviderCliUpdateStrategy {
        provider: "qwen",
        install_source: "pip-user",
        command_argv: &["pip", "install", "--user", "--upgrade", "qwen-cli"],
        expected_binary_name: "qwen",
        mutates_in_place: true,
        allow_candidate_path_change: true,
    },
];

pub fn update_strategy_for(provider: &str) -> Option<&'static ProviderCliUpdateStrategy> {
    PROVIDER_UPDATE_STRATEGIES
        .iter()
        .find(|s| s.provider == provider)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_four_providers_have_update_strategies() {
        for id in ["codex", "claude", "gemini", "qwen"] {
            let strategy = update_strategy_for(id);
            assert!(strategy.is_some(), "missing strategy for {id}");
            let s = strategy.unwrap();
            assert_eq!(s.provider, id);
            assert!(!s.command_argv.is_empty());
        }
    }

    #[test]
    fn all_current_providers_mutate_in_place() {
        for id in ["codex", "claude", "gemini", "qwen"] {
            let s = update_strategy_for(id).unwrap();
            assert!(
                s.mutates_in_place,
                "{id} expected mutates_in_place=true (all current providers use npm/pip global)"
            );
        }
    }

    #[test]
    fn unknown_provider_returns_none() {
        assert!(update_strategy_for("unknown-provider").is_none());
    }

    #[test]
    fn migration_state_serializes() {
        let state = MigrationState::CandidateDiscovered;
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(json, "\"candidate_discovered\"");
        let decoded: MigrationState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, state);
    }

    #[test]
    fn registry_agent_channel_lookup() {
        let mut registry = ProviderCliRegistry::default();
        let mut channels = ProviderChannels::default();
        channels
            .agent_overrides
            .insert("codex-agent".to_string(), "candidate".to_string());
        registry.providers.insert("codex".to_string(), channels);

        assert_eq!(
            registry.agent_channel("codex", "codex-agent"),
            Some("candidate")
        );
        assert_eq!(registry.agent_channel("codex", "other-agent"), None);
        assert_eq!(registry.agent_channel("claude", "codex-agent"), None);
    }

    #[test]
    fn smoke_check_status_serializes() {
        assert_eq!(
            serde_json::to_string(&SmokeCheckStatus::Ok).unwrap(),
            "\"ok\""
        );
        assert_eq!(
            serde_json::to_string(&SmokeCheckStatus::Failed).unwrap(),
            "\"failed\""
        );
    }
}
