use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::de::DeserializeOwned;

use super::super::OpenClawMigrateArgs;
use super::super::plan::ImportPlan;
use super::{
    ApplyAgentState, ApplyPhaseState, ApplyResult, ApplyTaskState, Manifest, ManifestAgent,
    RestoredAuditState, ResumeState, SessionMapEntry,
};

pub(super) fn build_initial_apply_agents(
    plan: &ImportPlan,
    args: &OpenClawMigrateArgs,
) -> BTreeMap<String, ApplyAgentState> {
    plan.agents
        .iter()
        .map(|agent| {
            let mut tasks = BTreeMap::new();
            let live_enabled = agent.eligible_for_v1;
            tasks.insert(
                "workspace_copy".to_string(),
                ApplyTaskState {
                    status: if !live_enabled || args.no_workspace {
                        "skipped".to_string()
                    } else {
                        "pending".to_string()
                    },
                    outputs: Vec::new(),
                },
            );
            tasks.insert(
                "prompt_write".to_string(),
                ApplyTaskState {
                    status: if !live_enabled || args.no_prompts {
                        "skipped".to_string()
                    } else {
                        "pending".to_string()
                    },
                    outputs: Vec::new(),
                },
            );
            tasks.insert(
                "memory_import".to_string(),
                ApplyTaskState {
                    status: if !live_enabled || args.no_memory {
                        "skipped".to_string()
                    } else {
                        "pending".to_string()
                    },
                    outputs: Vec::new(),
                },
            );
            tasks.insert(
                "session_import".to_string(),
                ApplyTaskState {
                    status: if live_enabled && args.with_sessions {
                        "pending".to_string()
                    } else {
                        "skipped".to_string()
                    },
                    outputs: Vec::new(),
                },
            );
            tasks.insert(
                "org_agent_write".to_string(),
                ApplyTaskState {
                    status: if live_enabled && args.write_org {
                        "pending".to_string()
                    } else {
                        "skipped".to_string()
                    },
                    outputs: Vec::new(),
                },
            );
            tasks.insert(
                "db_upsert".to_string(),
                ApplyTaskState {
                    status: if live_enabled && args.write_db {
                        "pending".to_string()
                    } else {
                        "skipped".to_string()
                    },
                    outputs: Vec::new(),
                },
            );
            (agent.final_role_id.clone(), ApplyAgentState { tasks })
        })
        .collect()
}

pub(super) fn build_initial_phase_status(
    plan: &ImportPlan,
    args: &OpenClawMigrateArgs,
) -> BTreeMap<String, ApplyPhaseState> {
    let now = Utc::now().to_rfc3339();
    plan.phases
        .iter()
        .map(|phase| {
            let status = match phase.phase {
                "scan" | "map" | "prompt" | "memory" | "policy_discord" => "completed",
                "sessions" if args.with_sessions => "pending",
                "apply_org" if args.write_org => "pending",
                "apply_bot_settings" if args.write_bot_settings => "pending",
                "apply_db" if args.write_db => "pending",
                "apply_org" | "apply_bot_settings" | "apply_db" | "sessions" => "skipped",
                _ => "pending",
            };
            (
                phase.phase.to_string(),
                ApplyPhaseState {
                    status: status.to_string(),
                    started_at: if status == "completed" {
                        now.clone()
                    } else {
                        String::new()
                    },
                    ended_at: if status == "completed" {
                        now.clone()
                    } else {
                        String::new()
                    },
                    error: None,
                },
            )
        })
        .collect()
}

pub(super) fn mark_phase_running(phases: &mut BTreeMap<String, ApplyPhaseState>, phase: &str) {
    if let Some(entry) = phases.get_mut(phase) {
        entry.status = "running".to_string();
        entry.started_at = Utc::now().to_rfc3339();
        entry.ended_at.clear();
        entry.error = None;
    }
}

pub(super) fn mark_phase_completed(phases: &mut BTreeMap<String, ApplyPhaseState>, phase: &str) {
    if let Some(entry) = phases.get_mut(phase) {
        if entry.started_at.is_empty() {
            entry.started_at = Utc::now().to_rfc3339();
        }
        entry.status = "completed".to_string();
        entry.ended_at = Utc::now().to_rfc3339();
        entry.error = None;
    }
}

pub(super) fn mark_first_running_phase_failed(
    phases: &mut BTreeMap<String, ApplyPhaseState>,
    error: &str,
) {
    if let Some((_, entry)) = phases
        .iter_mut()
        .find(|(_, phase)| phase.status == "running")
    {
        if entry.started_at.is_empty() {
            entry.started_at = Utc::now().to_rfc3339();
        }
        entry.status = "failed".to_string();
        entry.ended_at = Utc::now().to_rfc3339();
        entry.error = Some(error.to_string());
        return;
    }
    if let Some(entry) = phases.get_mut("finalize") {
        entry.status = "failed".to_string();
        entry.started_at = Utc::now().to_rfc3339();
        entry.ended_at = Utc::now().to_rfc3339();
        entry.error = Some(error.to_string());
    }
}

pub(super) fn update_task_state(
    state: &mut ApplyAgentState,
    key: &str,
    status: &'static str,
    outputs: Vec<String>,
) {
    state.tasks.insert(
        key.to_string(),
        ApplyTaskState {
            status: status.to_string(),
            outputs,
        },
    );
}

pub(super) fn load_restored_audit_state(
    audit_root: &Path,
    source_fingerprint: &str,
    args: &OpenClawMigrateArgs,
) -> Result<Option<RestoredAuditState>, String> {
    if args.resume.is_none() {
        return Ok(None);
    }

    let resume_state: ResumeState =
        match read_json_if_exists(&audit_root.join("resume-state.json"))? {
            Some(value) => value,
            None => return Ok(None),
        };
    if resume_state.source_fingerprint != source_fingerprint {
        return Err(format!(
            "Resume fingerprint mismatch for '{}'. Expected '{}', found '{}'.",
            audit_root.display(),
            source_fingerprint,
            resume_state.source_fingerprint
        ));
    }

    let manifest: Option<Manifest> = read_json_if_exists(&audit_root.join("manifest.json"))?;
    let apply_result: Option<ApplyResult> =
        read_json_if_exists(&audit_root.join("apply-result.json"))?;
    let session_map: Option<Vec<SessionMapEntry>> =
        read_json_if_exists(&audit_root.join("session-map.json"))?;

    Ok(Some(RestoredAuditState {
        manifest_agents: manifest
            .as_ref()
            .map(|value| value.agents.clone())
            .unwrap_or_default(),
        written_paths: manifest
            .as_ref()
            .map(|value| value.written_paths.clone())
            .unwrap_or_default(),
        warnings: merge_strings(
            manifest
                .as_ref()
                .map(|value| value.warnings.clone())
                .unwrap_or_default(),
            apply_result
                .as_ref()
                .map(|value| value.warnings.clone())
                .unwrap_or_default(),
        ),
        apply_agents: apply_result
            .as_ref()
            .map(|value| value.agents.clone())
            .unwrap_or_default(),
        phase_status: apply_result
            .as_ref()
            .map(|value| value.phases.clone())
            .unwrap_or_default(),
        session_map: session_map.unwrap_or_default(),
    }))
}

fn read_json_if_exists<T: DeserializeOwned>(path: &Path) -> Result<Option<T>, String> {
    if !path.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(path)
        .map_err(|e| format!("Failed to read '{}': {e}", path.display()))?;
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|e| format!("Failed to parse '{}': {e}", path.display()))
}

pub(super) fn merge_strings(mut left: Vec<String>, right: Vec<String>) -> Vec<String> {
    left.extend(right);
    left.sort();
    left.dedup();
    left
}

pub(super) fn normalize_resumable_state(
    apply_agents: &mut BTreeMap<String, ApplyAgentState>,
    phase_status: &mut BTreeMap<String, ApplyPhaseState>,
) {
    for phase in phase_status.values_mut() {
        if phase.status == "running" || phase.status == "failed" {
            phase.status = "pending".to_string();
            phase.error = None;
            phase.started_at.clear();
            phase.ended_at.clear();
        }
    }

    for agent in apply_agents.values_mut() {
        for task in agent.tasks.values_mut() {
            if task.status == "running" || task.status == "failed" {
                task.status = "pending".to_string();
            }
        }
    }
}

pub(super) fn phase_needs_apply(phases: &BTreeMap<String, ApplyPhaseState>, phase: &str) -> bool {
    phases
        .get(phase)
        .map(|state| state.status != "completed" && state.status != "skipped")
        .unwrap_or(true)
}

pub(super) fn task_is_finished(
    apply_agents: &BTreeMap<String, ApplyAgentState>,
    role_id: &str,
    task_key: &str,
) -> bool {
    apply_agents
        .get(role_id)
        .and_then(|state| state.tasks.get(task_key))
        .map(|task| task.status == "completed" || task.status == "skipped")
        .unwrap_or(false)
}

pub(super) fn manifest_agent_lookup<'a>(
    manifest_agents: &'a [ManifestAgent],
    role_id: &str,
) -> Option<&'a ManifestAgent> {
    manifest_agents
        .iter()
        .find(|entry| entry.role_id == role_id)
}

pub(super) fn upsert_manifest_agent(manifest_agents: &mut Vec<ManifestAgent>, next: ManifestAgent) {
    if let Some(existing) = manifest_agents
        .iter_mut()
        .find(|entry| entry.role_id == next.role_id)
    {
        *existing = next;
    } else {
        manifest_agents.push(next);
    }
}

pub(super) fn collect_existing_tree_files(root: &Path) -> Result<Vec<PathBuf>, String> {
    if !root.exists() {
        return Ok(Vec::new());
    }
    if root.is_file() {
        return Ok(vec![root.to_path_buf()]);
    }

    let mut outputs = Vec::new();
    let mut queue = VecDeque::from([root.to_path_buf()]);
    while let Some(dir) = queue.pop_front() {
        let entries =
            fs::read_dir(&dir).map_err(|e| format!("Failed to read '{}': {e}", dir.display()))?;
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                queue.push_back(path);
                continue;
            }
            if file_type.is_file() {
                outputs.push(path);
            }
        }
    }
    outputs.sort();
    Ok(outputs)
}
