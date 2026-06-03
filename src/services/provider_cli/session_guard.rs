use std::collections::{HashMap, HashSet};
use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::io::load_launch_artifacts;
use super::registry::LaunchArtifact;

const DEFAULT_SAFE_END_TIMEOUT_SECONDS: u64 = 5 * 60;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMigrationGuard {
    pub provider: String,
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_migration_channel: Option<String>,
    pub target_channel: String,
    pub active_turn_state: String,
    pub safe_end_started_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub safe_end_completed_at: Option<DateTime<Utc>>,
    pub safe_end_timeout_seconds: u64,
    pub safe_to_recreate: bool,
    pub recreate_required: bool,
    #[serde(default)]
    pub evidence: HashMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionGuardEvaluation {
    pub provider: String,
    pub target_channel: String,
    pub guards: Vec<SessionMigrationGuard>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blockers: Vec<String>,
}

impl SessionGuardEvaluation {
    pub fn is_clear(&self) -> bool {
        self.blockers.is_empty()
    }

    pub fn evidence_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
    }
}

pub fn evaluate_session_migration_guards(
    root: &Path,
    provider: &str,
    target_agent_ids: &[String],
    target_channel: &str,
) -> SessionGuardEvaluation {
    let artifacts = match load_launch_artifacts(root, provider) {
        Ok(artifacts) => artifacts,
        Err(error) => {
            return SessionGuardEvaluation {
                provider: provider.to_string(),
                target_channel: target_channel.to_string(),
                guards: Vec::new(),
                blockers: vec![format!("failed to load launch artifacts: {error}")],
            };
        }
    };
    let mut agents: HashSet<String> = target_agent_ids.iter().cloned().collect();
    if agents.is_empty() {
        agents.extend(
            artifacts
                .iter()
                .filter_map(|artifact| artifact.agent_id.clone()),
        );
    }

    let mut guards = Vec::new();
    let blockers = Vec::new();

    for agent_id in agents {
        let agent_artifacts = artifacts_for_agent(&artifacts, &agent_id);
        if agent_artifacts.is_empty() {
            // Agent has never been launched — no active session to protect, nothing to recreate.
            let mut guard = build_guard(provider, &agent_id, target_channel);
            guard.safe_to_recreate = true;
            guard.recreate_required = false;
            guard.active_turn_state = "no_prior_launch".to_string();
            guard
                .evidence
                .insert("status".to_string(), "no_prior_launch".to_string());
            guards.push(guard);
            continue;
        }

        for artifact in agent_artifacts {
            let mut guard = build_guard(provider, &agent_id, target_channel);
            guard.session_key = artifact.session_key.clone();
            guard.pre_migration_channel = Some(artifact.channel.clone());
            guard.evidence.insert(
                "launch_artifact_channel".to_string(),
                artifact.channel.clone(),
            );
            guard.evidence.insert(
                "launch_artifact_cli_path".to_string(),
                artifact.canonical_path.clone(),
            );

            let active = artifact_active(&artifact, &mut guard.evidence);
            if artifact.channel == target_channel {
                guard.safe_to_recreate = true;
                guard.recreate_required = false;
                guard.safe_end_completed_at = Some(Utc::now());
                guard.evidence.insert(
                    "status".to_string(),
                    "already_on_target_channel".to_string(),
                );
            } else {
                guard.safe_to_recreate = true;
                guard.recreate_required = true;
                // Do not set safe_end_completed_at — no actual safe-end procedure was run;
                // active sessions are allowed with evidence rather than drained.
                guard.evidence.insert(
                    "status".to_string(),
                    if active {
                        "active_old_channel_session".to_string()
                    } else {
                        "old_channel_session_not_active".to_string()
                    },
                );
                if active {
                    guard.active_turn_state = "active_old_channel_session".to_string();
                    guard.evidence.insert(
                        "safe_end_skipped_reason".to_string(),
                        "active_session_auto_allowed".to_string(),
                    );
                } else {
                    guard.safe_end_completed_at = Some(Utc::now());
                }
            }

            guards.push(guard);
        }
    }

    SessionGuardEvaluation {
        provider: provider.to_string(),
        target_channel: target_channel.to_string(),
        guards,
        blockers,
    }
}

fn build_guard(provider: &str, agent_id: &str, target_channel: &str) -> SessionMigrationGuard {
    SessionMigrationGuard {
        provider: provider.to_string(),
        agent_id: agent_id.to_string(),
        session_key: None,
        pre_migration_channel: None,
        target_channel: target_channel.to_string(),
        active_turn_state: "unknown".to_string(),
        safe_end_started_at: Utc::now(),
        safe_end_completed_at: None,
        safe_end_timeout_seconds: DEFAULT_SAFE_END_TIMEOUT_SECONDS,
        safe_to_recreate: false,
        recreate_required: false,
        evidence: HashMap::new(),
    }
}

fn artifacts_for_agent(artifacts: &[LaunchArtifact], agent_id: &str) -> Vec<LaunchArtifact> {
    let mut matches = artifacts
        .iter()
        .filter(|artifact| artifact.agent_id.as_deref() == Some(agent_id))
        .cloned()
        .collect::<Vec<_>>();
    matches.sort_by_key(|artifact| artifact.launched_at);
    matches
}

fn artifact_active(artifact: &LaunchArtifact, evidence: &mut HashMap<String, String>) -> bool {
    let mut active = false;
    if let Some(pid) = artifact.process_id {
        let process_alive = crate::services::process::get_process_list()
            .iter()
            .any(|process| process.pid == pid as i32);
        evidence.insert("process_alive".to_string(), process_alive.to_string());
        active |= process_alive;
    }

    #[cfg(unix)]
    if let Some(tmux_session) = artifact.tmux_session.as_deref() {
        let tmux_alive =
            crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_session);
        evidence.insert("tmux_live_pane".to_string(), tmux_alive.to_string());
        active |= tmux_alive;
    }

    active
}
