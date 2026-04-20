//! Data-driven pipeline engine (#106 P1-P4).
//!
//! Loads pipeline definition from YAML and provides lookup methods
//! used by `kanban.rs` for transition validation.
//!
//! ## Hierarchy (#135)
//!
//! Pipeline configs form a three-level inheritance chain:
//!   **default** → **repo** → **agent**
//!
//! Each level can override specific sections (states, transitions, gates,
//! hooks, clocks, timeouts). Omitted sections inherit from the parent.
//! `resolve()` merges the chain into a single effective `PipelineConfig`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

/// Global singleton pipeline config (the default), loaded once at startup.
static PIPELINE: OnceLock<PipelineConfig> = OnceLock::new();

/// Load pipeline from YAML file. Called once during server startup.
pub fn load(path: &Path) -> Result<()> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let config: PipelineConfig =
        serde_yaml::from_str(&content).with_context(|| format!("parsing {}", path.display()))?;
    config.validate()?;
    PIPELINE
        .set(config)
        .map_err(|_| anyhow::anyhow!("pipeline already loaded"))?;
    Ok(())
}

/// Get the loaded pipeline config. Panics if not yet loaded.
pub fn get() -> &'static PipelineConfig {
    PIPELINE
        .get()
        .expect("pipeline not loaded — call pipeline::load() at startup")
}

/// Try to get the loaded pipeline config. Returns None if not yet loaded.
pub fn try_get() -> Option<&'static PipelineConfig> {
    PIPELINE.get()
}

/// Ensure the default pipeline is loaded. Loads from the standard path if not yet loaded.
/// Safe to call multiple times (idempotent). Used by tests and server startup.
pub fn ensure_loaded() {
    if PIPELINE.get().is_some() {
        return;
    }
    // Try standard paths in order
    let candidates = [
        std::path::PathBuf::from("policies/default-pipeline.yaml"),
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("policies/default-pipeline.yaml"),
    ];
    for path in &candidates {
        if path.exists() {
            if let Err(e) = load(path) {
                tracing::warn!("Failed to load pipeline from {}: {e}", path.display());
            } else {
                return;
            }
        }
    }
    tracing::warn!("No pipeline YAML found — pipeline features disabled");
}

/// Parse a pipeline override from JSON (stored in DB).
/// Returns None if the input is empty/null.
pub fn parse_override(json_str: &str) -> Result<Option<PipelineOverride>> {
    let trimmed = json_str.trim();
    if trimmed.is_empty() || trimmed == "null" || trimmed == "{}" {
        return Ok(None);
    }
    let ovr: PipelineOverride =
        serde_json::from_str(trimmed).with_context(|| "parsing pipeline override JSON")?;
    Ok(Some(ovr))
}

/// Resolve the effective pipeline for a given (repo, agent) combination.
///
/// Merges: default → repo_override → agent_override.
/// Each override only replaces the sections it explicitly provides.
/// Panics if the default pipeline has not been loaded.
pub fn resolve(
    repo_override: Option<&PipelineOverride>,
    agent_override: Option<&PipelineOverride>,
) -> PipelineConfig {
    let base = try_get()
        .expect("pipeline not loaded — call pipeline::ensure_loaded() before resolve()")
        .clone();
    let after_repo = match repo_override {
        Some(ovr) => base.merge(ovr),
        None => base,
    };
    match agent_override {
        Some(ovr) => after_repo.merge(ovr),
        None => after_repo,
    }
}

/// Resolve effective pipeline from DB, looking up repo and agent overrides.
pub fn resolve_for_card(
    conn: &libsql_rusqlite::Connection,
    repo_id: Option<&str>,
    agent_id: Option<&str>,
) -> PipelineConfig {
    let repo_ovr = repo_id
        .and_then(|rid| {
            conn.query_row(
                "SELECT pipeline_config FROM github_repos WHERE id = ?1",
                [rid],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        })
        .and_then(|json| parse_override(&json).ok().flatten());

    let agent_ovr = agent_id
        .and_then(|aid| {
            conn.query_row(
                "SELECT pipeline_config FROM agents WHERE id = ?1",
                [aid],
                |row| row.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
        })
        .and_then(|json| parse_override(&json).ok().flatten());

    resolve(repo_ovr.as_ref(), agent_ovr.as_ref())
}

pub async fn resolve_for_card_pg(
    pool: &PgPool,
    repo_id: Option<&str>,
    agent_id: Option<&str>,
) -> PipelineConfig {
    let repo_ovr = if let Some(rid) = repo_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config
             FROM github_repos
             WHERE id = $1",
        )
        .bind(rid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .and_then(|json| parse_override(&json).ok().flatten())
    } else {
        None
    };

    let agent_ovr = if let Some(aid) = agent_id {
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT pipeline_config::text AS pipeline_config
             FROM agents
             WHERE id = $1",
        )
        .bind(aid)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .and_then(|json| parse_override(&json).ok().flatten())
    } else {
        None
    };

    resolve(repo_ovr.as_ref(), agent_ovr.as_ref())
}

// ── Override Schema ──────────────────────────────────────────────

/// A partial pipeline config used for repo/agent-level overrides.
/// Only non-None fields replace the parent's values.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PipelineOverride {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub states: Option<Vec<StateConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transitions: Option<Vec<TransitionConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gates: Option<HashMap<String, GateConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<HashMap<String, HookBindings>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub events: Option<HashMap<String, Vec<String>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub clocks: Option<HashMap<String, ClockConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeouts: Option<HashMap<String, TimeoutConfig>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase_gate: Option<PhaseGateConfig>,
}

// ── Schema ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipelineConfig {
    pub name: String,
    pub version: u32,
    pub states: Vec<StateConfig>,
    pub transitions: Vec<TransitionConfig>,
    #[serde(default)]
    pub gates: HashMap<String, GateConfig>,
    #[serde(default)]
    pub hooks: HashMap<String, HookBindings>,
    /// Event hooks — lifecycle events not bound to state transitions.
    /// Key: event name (e.g. "on_dispatch_completed"), Value: list of hook names to fire.
    #[serde(default)]
    pub events: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub clocks: HashMap<String, ClockConfig>,
    #[serde(default)]
    pub timeouts: HashMap<String, TimeoutConfig>,
    #[serde(default)]
    pub phase_gate: PhaseGateConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateConfig {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub terminal: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransitionConfig {
    pub from: String,
    pub to: String,
    #[serde(rename = "type")]
    pub transition_type: TransitionType,
    #[serde(default)]
    pub gates: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransitionType {
    Free,
    Gated,
    ForceOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GateConfig {
    #[serde(rename = "type")]
    pub gate_type: String,
    #[serde(default)]
    pub check: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookBindings {
    #[serde(default)]
    pub on_enter: Vec<String>,
    #[serde(default)]
    pub on_exit: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClockConfig {
    pub set: String,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    pub duration: String,
    pub clock: String,
    #[serde(default)]
    pub max_retries: Option<u32>,
    #[serde(default)]
    pub on_exhaust: Option<String>,
    #[serde(default)]
    pub condition: Option<String>,
}

fn default_phase_gate_dispatch_to() -> String {
    "self".to_string()
}

fn default_phase_gate_dispatch_type() -> String {
    "phase-gate".to_string()
}

fn default_phase_gate_pass_verdict() -> String {
    "phase_gate_passed".to_string()
}

fn default_phase_gate_checks() -> Vec<String> {
    vec![
        "merge_verified".to_string(),
        "issue_closed".to_string(),
        "build_passed".to_string(),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhaseGateConfig {
    #[serde(default = "default_phase_gate_dispatch_to")]
    pub dispatch_to: String,
    #[serde(default = "default_phase_gate_dispatch_type")]
    pub dispatch_type: String,
    #[serde(default = "default_phase_gate_pass_verdict")]
    pub pass_verdict: String,
    #[serde(default = "default_phase_gate_checks")]
    pub checks: Vec<String>,
}

impl Default for PhaseGateConfig {
    fn default() -> Self {
        Self {
            dispatch_to: default_phase_gate_dispatch_to(),
            dispatch_type: default_phase_gate_dispatch_type(),
            pass_verdict: default_phase_gate_pass_verdict(),
            checks: default_phase_gate_checks(),
        }
    }
}

// ── Merge ────────────────────────────────────────────────────────

impl PipelineConfig {
    /// Merge an override into this config, returning the result.
    /// Override fields replace base fields entirely when present.
    pub fn merge(&self, ovr: &PipelineOverride) -> PipelineConfig {
        PipelineConfig {
            name: self.name.clone(),
            version: self.version,
            states: ovr
                .states
                .as_ref()
                .cloned()
                .unwrap_or_else(|| self.states.clone()),
            transitions: ovr
                .transitions
                .as_ref()
                .cloned()
                .unwrap_or_else(|| self.transitions.clone()),
            gates: ovr
                .gates
                .as_ref()
                .cloned()
                .unwrap_or_else(|| self.gates.clone()),
            hooks: ovr
                .hooks
                .as_ref()
                .cloned()
                .unwrap_or_else(|| self.hooks.clone()),
            events: ovr
                .events
                .as_ref()
                .cloned()
                .unwrap_or_else(|| self.events.clone()),
            clocks: ovr
                .clocks
                .as_ref()
                .cloned()
                .unwrap_or_else(|| self.clocks.clone()),
            timeouts: ovr
                .timeouts
                .as_ref()
                .cloned()
                .unwrap_or_else(|| self.timeouts.clone()),
            phase_gate: ovr
                .phase_gate
                .clone()
                .unwrap_or_else(|| self.phase_gate.clone()),
        }
    }

    /// Serialize to JSON (for API responses / DB storage).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::json!({}))
    }
}

// ── Lookup methods ───────────────────────────────────────────────

impl PipelineConfig {
    /// Find transition rule for from → to.
    pub fn find_transition(&self, from: &str, to: &str) -> Option<&TransitionConfig> {
        self.transitions
            .iter()
            .find(|t| t.from == from && t.to == to)
    }

    /// Check if a state is terminal (no outbound transitions allowed).
    pub fn is_terminal(&self, state: &str) -> bool {
        self.states.iter().any(|s| s.id == state && s.terminal)
    }

    /// Check if a state is valid.
    pub fn is_valid_state(&self, state: &str) -> bool {
        self.states.iter().any(|s| s.id == state)
    }

    /// Get clock field to set when entering a state.
    pub fn clock_for_state(&self, state: &str) -> Option<&ClockConfig> {
        self.clocks.get(state)
    }

    /// Get hook bindings for a state.
    pub fn hooks_for_state(&self, state: &str) -> Option<&HookBindings> {
        self.hooks.get(state)
    }

    /// Get event hook names for a lifecycle event (e.g. "on_dispatch_completed").
    pub fn event_hooks(&self, event: &str) -> Option<&Vec<String>> {
        self.events.get(event)
    }

    /// Get the initial state (first non-terminal state in the pipeline).
    /// This is the state new cards start in.
    pub fn initial_state(&self) -> &str {
        self.states
            .iter()
            .find(|s| !s.terminal)
            .map(|s| s.id.as_str())
            .unwrap_or("backlog")
    }

    /// Get states that are dispatchable (have gated outbound transitions).
    /// These are states where cards are "ready to be dispatched".
    pub fn dispatchable_states(&self) -> Vec<&str> {
        self.states
            .iter()
            .filter(|s| {
                !s.terminal
                    && self.transitions.iter().any(|t| {
                        t.from == s.id && t.transition_type == TransitionType::Gated
                    })
                    // Must not have gated inbound transitions (free is ok).
                    && !self.transitions.iter().any(|t| {
                        t.to == s.id && t.transition_type == TransitionType::Gated
                    })
            })
            .map(|s| s.id.as_str())
            .collect()
    }

    /// Resolve the kickoff state for a card currently in `current_state`.
    ///
    /// 1. If there is a gated transition FROM `current_state`, return its target.
    /// 2. Otherwise fall back to the first gated transition from any dispatchable state.
    ///
    /// Returns `None` only when the pipeline has no gated transitions at all.
    pub fn kickoff_for(&self, current_state: &str) -> Option<String> {
        // Prefer the concrete gated transition from the card's actual state
        self.transitions
            .iter()
            .find(|t| t.from == current_state && t.transition_type == TransitionType::Gated)
            .map(|t| t.to.clone())
            .or_else(|| {
                // Fallback: first dispatchable state's gated target
                let dispatchable = self.dispatchable_states();
                self.transitions
                    .iter()
                    .find(|t| {
                        t.transition_type == TransitionType::Gated
                            && dispatchable.contains(&t.from.as_str())
                    })
                    .map(|t| t.to.clone())
            })
    }

    /// Walk free transitions from `from` to the nearest dispatchable state,
    /// returning every intermediate step so callers can replay each transition
    /// individually (preserving clock/audit/review-state for each hop).
    ///
    /// Returns `None` if already dispatchable or no free path exists.
    /// Returns `Some(vec!["triage", "ready"])` for a `backlog → triage → ready` path.
    pub fn free_path_to_dispatchable(&self, from: &str) -> Option<Vec<String>> {
        let dispatchable = self.dispatchable_states();
        if dispatchable.contains(&from) {
            return None; // already dispatchable
        }
        // BFS over free transitions, tracking parent for path reconstruction
        let mut visited = std::collections::HashSet::new();
        let mut parent: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(from.to_string());
        visited.insert(from.to_string());
        let mut target: Option<String> = None;
        while let Some(cur) = queue.pop_front() {
            for t in &self.transitions {
                if t.from == cur
                    && t.transition_type == TransitionType::Free
                    && !visited.contains(&t.to)
                {
                    parent.insert(t.to.clone(), cur.clone());
                    if dispatchable.contains(&t.to.as_str()) {
                        target = Some(t.to.clone());
                        break;
                    }
                    visited.insert(t.to.clone());
                    queue.push_back(t.to.clone());
                }
            }
            if target.is_some() {
                break;
            }
        }
        // Reconstruct path from `from` to `target` (excluding `from`)
        let target = target?;
        let mut path = vec![target.clone()];
        let mut cur = target;
        while let Some(prev) = parent.get(&cur) {
            if prev == from {
                break;
            }
            path.push(prev.clone());
            cur = prev.clone();
        }
        path.reverse();
        Some(path)
    }

    /// Returns the free-only path from `from` to a specific target state.
    ///
    /// Returns `None` if the target is not reachable via free transitions.
    /// Returns `Some(vec!["triage", "ready"])` for a `backlog → triage → ready` path.
    pub fn free_path_to_state(&self, from: &str, target: &str) -> Option<Vec<String>> {
        if from == target {
            return Some(Vec::new());
        }
        if !self.is_valid_state(from) || !self.is_valid_state(target) {
            return None;
        }

        let mut visited = std::collections::HashSet::new();
        let mut parent: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(from.to_string());
        visited.insert(from.to_string());
        let mut found = false;

        while let Some(cur) = queue.pop_front() {
            for t in &self.transitions {
                if t.from == cur
                    && t.transition_type == TransitionType::Free
                    && !visited.contains(&t.to)
                {
                    parent.insert(t.to.clone(), cur.clone());
                    if t.to == target {
                        found = true;
                        break;
                    }
                    visited.insert(t.to.clone());
                    queue.push_back(t.to.clone());
                }
            }
            if found {
                break;
            }
        }

        if !found {
            return None;
        }

        let mut path = vec![target.to_string()];
        let mut cur = target.to_string();
        while let Some(prev) = parent.get(&cur) {
            if prev == from {
                break;
            }
            path.push(prev.clone());
            cur = prev.clone();
        }
        path.reverse();
        Some(path)
    }

    /// Check if a state requires a gated inbound transition (dispatch-entry states).
    /// These states should only be entered via dispatch API, not direct PATCH.
    #[allow(dead_code)]
    pub fn requires_dispatch_entry(&self, state: &str) -> bool {
        self.transitions
            .iter()
            .any(|t| t.to == state && t.transition_type == TransitionType::Gated)
            && !self
                .transitions
                .iter()
                .any(|t| t.to == state && t.transition_type == TransitionType::Free)
    }

    /// Check if a state is a dispatch kickoff state — the first gated target
    /// reachable from a dispatchable state. Only these should be blocked from
    /// direct PATCH (must use POST /api/dispatches instead).
    #[allow(dead_code)]
    pub fn is_dispatch_kickoff(&self, state: &str) -> bool {
        let dispatchable = self.dispatchable_states();
        self.transitions.iter().any(|t| {
            t.to == state
                && t.transition_type == TransitionType::Gated
                && dispatchable.contains(&t.from.as_str())
        })
    }

    /// Validate internal consistency.
    pub fn validate(&self) -> Result<()> {
        let state_ids: Vec<&str> = self.states.iter().map(|s| s.id.as_str()).collect();

        // All transition from/to must reference valid states
        for t in &self.transitions {
            if !state_ids.contains(&t.from.as_str()) {
                anyhow::bail!("transition from unknown state: {}", t.from);
            }
            if !state_ids.contains(&t.to.as_str()) {
                anyhow::bail!("transition to unknown state: {}", t.to);
            }
        }

        // All gate references must exist in gates map
        for t in &self.transitions {
            for g in &t.gates {
                if !self.gates.contains_key(g) {
                    anyhow::bail!(
                        "transition {}→{} references unknown gate: {}",
                        t.from,
                        t.to,
                        g
                    );
                }
            }
        }

        // Clock fields must reference valid states
        for state in self.clocks.keys() {
            if !state_ids.contains(&state.as_str()) {
                anyhow::bail!("clock for unknown state: {}", state);
            }
        }

        // Hook bindings must reference valid states
        for state in self.hooks.keys() {
            if !state_ids.contains(&state.as_str()) {
                anyhow::bail!("hook binding for unknown state: {}", state);
            }
        }

        // Timeout entries: state-keyed timeouts must reference valid states.
        // Condition-based timeouts (e.g. awaiting_dod) are pseudo-state timeouts
        // that manage their own clock columns — skip all cross-reference checks.
        let known_clock_fields: Vec<&str> = self.clocks.values().map(|c| c.set.as_str()).collect();
        for (key, timeout) in &self.timeouts {
            // Condition-based timeouts are self-contained; skip validation
            if timeout.condition.is_some() {
                continue;
            }
            if !state_ids.contains(&key.as_str()) {
                anyhow::bail!("timeout for unknown state: {}", key);
            }
            if !self.clocks.contains_key(&timeout.clock)
                && !known_clock_fields.contains(&timeout.clock.as_str())
            {
                anyhow::bail!(
                    "timeout '{}' references unknown clock: {}",
                    key,
                    timeout.clock
                );
            }
        }

        if self.phase_gate.dispatch_to.trim().is_empty() {
            anyhow::bail!("phase_gate.dispatch_to must not be empty");
        }
        if self.phase_gate.dispatch_type.trim().is_empty() {
            anyhow::bail!("phase_gate.dispatch_type must not be empty");
        }
        if self.phase_gate.pass_verdict.trim().is_empty() {
            anyhow::bail!("phase_gate.pass_verdict must not be empty");
        }
        if self.phase_gate.checks.is_empty() {
            anyhow::bail!("phase_gate.checks must not be empty");
        }

        Ok(())
    }

    /// Produce a graph representation of the pipeline for dashboard visualization.
    /// Returns states as nodes and transitions as edges with their gate/type info.
    pub fn to_graph(&self) -> serde_json::Value {
        let nodes: Vec<serde_json::Value> = self
            .states
            .iter()
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "label": s.label,
                    "terminal": s.terminal,
                    "has_hooks": self.hooks.contains_key(&s.id),
                    "has_clock": self.clocks.contains_key(&s.id),
                    "has_timeout": self.timeouts.contains_key(&s.id),
                })
            })
            .collect();

        let edges: Vec<serde_json::Value> = self
            .transitions
            .iter()
            .map(|t| {
                serde_json::json!({
                    "from": t.from,
                    "to": t.to,
                    "type": format!("{:?}", t.transition_type).to_lowercase(),
                    "gates": t.gates,
                })
            })
            .collect();

        serde_json::json!({
            "nodes": nodes,
            "edges": edges,
        })
    }
}

// ── Tests ────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::PgPool;

    fn minimal_pipeline() -> PipelineConfig {
        PipelineConfig {
            name: "test".into(),
            version: 1,
            states: vec![
                StateConfig {
                    id: "backlog".into(),
                    label: "Backlog".into(),
                    terminal: false,
                },
                StateConfig {
                    id: "in_progress".into(),
                    label: "In Progress".into(),
                    terminal: false,
                },
                StateConfig {
                    id: "done".into(),
                    label: "Done".into(),
                    terminal: true,
                },
            ],
            transitions: vec![
                TransitionConfig {
                    from: "backlog".into(),
                    to: "in_progress".into(),
                    transition_type: TransitionType::Free,
                    gates: vec![],
                },
                TransitionConfig {
                    from: "in_progress".into(),
                    to: "done".into(),
                    transition_type: TransitionType::Gated,
                    gates: vec!["review_passed".into()],
                },
            ],
            gates: {
                let mut m = HashMap::new();
                m.insert(
                    "review_passed".into(),
                    GateConfig {
                        gate_type: "builtin".into(),
                        check: Some("review_verdict_pass".into()),
                        description: None,
                    },
                );
                m
            },
            hooks: HashMap::new(),
            events: HashMap::new(),
            clocks: HashMap::new(),
            timeouts: HashMap::new(),
            phase_gate: PhaseGateConfig::default(),
        }
    }

    struct TestPostgresDb {
        admin_url: String,
        database_name: String,
        database_url: String,
    }

    impl TestPostgresDb {
        async fn create() -> Option<Self> {
            let admin_url = postgres_admin_url();
            let database_name = format!("agentdesk_pipeline_{}", uuid::Uuid::new_v4().simple());
            let database_url = format!("{}/{}", postgres_base_database_url(), database_name);
            let admin_pool = match sqlx::PgPool::connect(&admin_url).await {
                Ok(pool) => pool,
                Err(error) => {
                    eprintln!("skipping postgres pipeline test: admin connect failed: {error}");
                    return None;
                }
            };
            if let Err(error) = sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
                .execute(&admin_pool)
                .await
            {
                eprintln!("skipping postgres pipeline test: create database failed: {error}");
                admin_pool.close().await;
                return None;
            }
            admin_pool.close().await;
            Some(Self {
                admin_url,
                database_name,
                database_url,
            })
        }

        async fn migrate(&self) -> Option<PgPool> {
            let pool = match sqlx::PgPool::connect(&self.database_url).await {
                Ok(pool) => pool,
                Err(error) => {
                    eprintln!("skipping postgres pipeline test: db connect failed: {error}");
                    return None;
                }
            };
            if let Err(error) = crate::db::postgres::migrate(&pool).await {
                eprintln!("skipping postgres pipeline test: migrate failed: {error}");
                pool.close().await;
                return None;
            }
            Some(pool)
        }

        async fn drop(self) {
            let Ok(admin_pool) = sqlx::PgPool::connect(&self.admin_url).await else {
                return;
            };
            let _ = sqlx::query(
                "SELECT pg_terminate_backend(pid)
                 FROM pg_stat_activity
                 WHERE datname = $1
                   AND pid <> pg_backend_pid()",
            )
            .bind(&self.database_name)
            .execute(&admin_pool)
            .await;
            let _ = sqlx::query(&format!(
                "DROP DATABASE IF EXISTS \"{}\"",
                self.database_name
            ))
            .execute(&admin_pool)
            .await;
            admin_pool.close().await;
        }
    }

    fn postgres_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }
        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "127.0.0.1".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgres://{user}:{password}@{host}:{port}"),
            None => format!("postgres://{user}@{host}:{port}"),
        }
    }

    fn postgres_admin_url() -> String {
        if let Ok(url) = std::env::var("POSTGRES_TEST_ADMIN_URL") {
            let trimmed = url.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
        format!("{}/postgres", postgres_base_database_url())
    }

    async fn pg_seed_repo(pool: &PgPool, repo_id: &str, pipeline_config: Option<&str>) {
        sqlx::query(
            "INSERT INTO github_repos (id, display_name, pipeline_config)
             VALUES ($1, $2, $3::jsonb)",
        )
        .bind(repo_id)
        .bind(format!("Repo {repo_id}"))
        .bind(pipeline_config)
        .execute(pool)
        .await
        .expect("seed github_repos");
    }

    async fn pg_seed_agent(pool: &PgPool, agent_id: &str, pipeline_config: Option<&str>) {
        sqlx::query(
            "INSERT INTO agents (id, name, pipeline_config)
             VALUES ($1, $2, $3::jsonb)",
        )
        .bind(agent_id)
        .bind(format!("Agent {agent_id}"))
        .bind(pipeline_config)
        .execute(pool)
        .await
        .expect("seed agents");
    }

    #[test]
    fn merge_override_replaces_states() {
        let base = minimal_pipeline();
        let ovr = PipelineOverride {
            states: Some(vec![
                StateConfig {
                    id: "todo".into(),
                    label: "Todo".into(),
                    terminal: false,
                },
                StateConfig {
                    id: "done".into(),
                    label: "Complete".into(),
                    terminal: true,
                },
            ]),
            ..Default::default()
        };
        let merged = base.merge(&ovr);
        assert_eq!(merged.states.len(), 2);
        assert_eq!(merged.states[0].id, "todo");
        // Non-overridden sections preserved
        assert_eq!(merged.transitions.len(), 2);
        assert!(merged.gates.contains_key("review_passed"));
        assert_eq!(merged.phase_gate.dispatch_type, "phase-gate");
    }

    #[test]
    fn merge_override_replaces_hooks() {
        let base = minimal_pipeline();
        let ovr = PipelineOverride {
            hooks: Some({
                let mut m = HashMap::new();
                m.insert(
                    "backlog".into(),
                    HookBindings {
                        on_enter: vec!["CustomHook".into()],
                        on_exit: vec![],
                    },
                );
                m
            }),
            ..Default::default()
        };
        let merged = base.merge(&ovr);
        assert!(merged.hooks.contains_key("backlog"));
        assert_eq!(merged.hooks["backlog"].on_enter, vec!["CustomHook"]);
        // States unchanged
        assert_eq!(merged.states.len(), 3);
    }

    #[test]
    fn merge_empty_override_is_identity() {
        let base = minimal_pipeline();
        let ovr = PipelineOverride::default();
        let merged = base.merge(&ovr);
        assert_eq!(merged.states.len(), base.states.len());
        assert_eq!(merged.transitions.len(), base.transitions.len());
        assert_eq!(merged.gates.len(), base.gates.len());
        assert_eq!(merged.phase_gate, base.phase_gate);
    }

    #[test]
    fn chained_merge_applies_both_layers() {
        let base = minimal_pipeline();

        // Repo override: add hooks
        let repo_ovr = PipelineOverride {
            hooks: Some({
                let mut m = HashMap::new();
                m.insert(
                    "in_progress".into(),
                    HookBindings {
                        on_enter: vec!["RepoHook".into()],
                        on_exit: vec![],
                    },
                );
                m
            }),
            ..Default::default()
        };

        // Agent override: replace hooks entirely
        let agent_ovr = PipelineOverride {
            hooks: Some({
                let mut m = HashMap::new();
                m.insert(
                    "in_progress".into(),
                    HookBindings {
                        on_enter: vec!["AgentHook".into()],
                        on_exit: vec![],
                    },
                );
                m
            }),
            ..Default::default()
        };

        let after_repo = base.merge(&repo_ovr);
        assert_eq!(after_repo.hooks["in_progress"].on_enter, vec!["RepoHook"]);

        let after_agent = after_repo.merge(&agent_ovr);
        assert_eq!(after_agent.hooks["in_progress"].on_enter, vec!["AgentHook"]);
        // States still from base
        assert_eq!(after_agent.states.len(), 3);
    }

    #[test]
    fn resolve_with_no_overrides_returns_base() {
        // Load pipeline for resolve()
        ensure_loaded();
        let result = resolve(None, None);
        let default = get();
        assert_eq!(result.name, default.name);
        assert_eq!(result.states.len(), default.states.len());
    }

    #[test]
    fn parse_override_empty_returns_none() {
        assert!(parse_override("").unwrap().is_none());
        assert!(parse_override("null").unwrap().is_none());
        assert!(parse_override("{}").unwrap().is_none());
    }

    #[test]
    fn parse_override_valid_json() {
        let json = r#"{"hooks":{"review":{"on_enter":["MyHook"],"on_exit":[]}},"phase_gate":{"dispatch_to":"ch-qad","dispatch_type":"qa-gate","pass_verdict":"qa_passed","checks":["merge_verified","qa_passed"]}}"#;
        let ovr = parse_override(json).unwrap().unwrap();
        assert!(ovr.hooks.is_some());
        assert!(ovr.states.is_none());
        assert_eq!(
            ovr.phase_gate
                .as_ref()
                .map(|gate| gate.dispatch_to.as_str()),
            Some("ch-qad")
        );
    }

    #[test]
    fn merge_override_replaces_phase_gate() {
        let base = minimal_pipeline();
        let ovr = PipelineOverride {
            phase_gate: Some(PhaseGateConfig {
                dispatch_to: "ch-qad".into(),
                dispatch_type: "qa-gate".into(),
                pass_verdict: "qa_passed".into(),
                checks: vec!["merge_verified".into(), "qa_passed".into()],
            }),
            ..Default::default()
        };
        let merged = base.merge(&ovr);
        assert_eq!(merged.phase_gate.dispatch_to, "ch-qad");
        assert_eq!(merged.phase_gate.dispatch_type, "qa-gate");
        assert_eq!(merged.phase_gate.pass_verdict, "qa_passed");
        assert_eq!(
            merged.phase_gate.checks,
            vec!["merge_verified".to_string(), "qa_passed".to_string()]
        );
    }

    #[test]
    fn validate_rejects_timeout_unknown_state() {
        let mut p = minimal_pipeline();
        p.clocks.insert(
            "in_progress".into(),
            ClockConfig {
                set: "started_at".into(),
                mode: None,
            },
        );
        p.timeouts.insert(
            "nonexistent".into(),
            TimeoutConfig {
                duration: "1h".into(),
                clock: "started_at".into(),
                max_retries: None,
                on_exhaust: None,
                condition: None,
            },
        );
        let err = p.validate().unwrap_err();
        assert!(err.to_string().contains("unknown state"), "{err}");
    }

    #[test]
    fn validate_rejects_timeout_unknown_clock() {
        let mut p = minimal_pipeline();
        p.timeouts.insert(
            "in_progress".into(),
            TimeoutConfig {
                duration: "1h".into(),
                clock: "no_such_clock".into(),
                max_retries: None,
                on_exhaust: None,
                condition: None,
            },
        );
        let err = p.validate().unwrap_err();
        assert!(err.to_string().contains("unknown clock"), "{err}");
    }

    #[test]
    fn validate_allows_condition_based_timeout_pseudo_state() {
        let mut p = minimal_pipeline();
        p.timeouts.insert(
            "awaiting_something".into(),
            TimeoutConfig {
                duration: "15m".into(),
                clock: "custom_at".into(),
                max_retries: None,
                on_exhaust: None,
                condition: Some("some_field = 'value'".into()),
            },
        );
        assert!(p.validate().is_ok());
    }

    #[test]
    fn validate_accepts_timeout_with_clock_field_ref() {
        let mut p = minimal_pipeline();
        p.clocks.insert(
            "in_progress".into(),
            ClockConfig {
                set: "started_at".into(),
                mode: None,
            },
        );
        p.timeouts.insert(
            "in_progress".into(),
            TimeoutConfig {
                duration: "2h".into(),
                clock: "started_at".into(),
                max_retries: None,
                on_exhaust: None,
                condition: None,
            },
        );
        assert!(p.validate().is_ok());
    }

    #[tokio::test]
    async fn resolve_for_card_pg_supports_default_repo_and_repo_plus_agent_merges() {
        let Some(pg_db) = TestPostgresDb::create().await else {
            return;
        };
        let Some(pool) = pg_db.migrate().await else {
            pg_db.drop().await;
            return;
        };

        ensure_loaded();
        let default = resolve(None, None);
        let effective = resolve_for_card_pg(&pool, None, None).await;
        assert_eq!(effective.name, default.name);
        assert_eq!(effective.phase_gate, default.phase_gate);

        pg_seed_repo(
            &pool,
            "repo-only",
            Some(r#"{"hooks":{"review":{"on_enter":["RepoHook"],"on_exit":[]}}}"#),
        )
        .await;
        let effective = resolve_for_card_pg(&pool, Some("repo-only"), None).await;
        assert_eq!(
            effective
                .hooks
                .get("review")
                .map(|hooks| hooks.on_enter.clone()),
            Some(vec!["RepoHook".to_string()])
        );

        pg_seed_repo(
            &pool,
            "repo-stack",
            Some(r#"{"hooks":{"review":{"on_enter":["RepoHook"],"on_exit":[]}}}"#),
        )
        .await;
        pg_seed_agent(
            &pool,
            "agent-stack",
            Some(
                r#"{"phase_gate":{"dispatch_to":"agent-review","dispatch_type":"agent-gate","pass_verdict":"agent_passed","checks":["agent_passed"]}}"#,
            ),
        )
        .await;
        let effective = resolve_for_card_pg(&pool, Some("repo-stack"), Some("agent-stack")).await;
        assert_eq!(
            effective
                .hooks
                .get("review")
                .map(|hooks| hooks.on_enter.clone()),
            Some(vec!["RepoHook".to_string()])
        );
        assert_eq!(effective.phase_gate.dispatch_to, "agent-review");
        assert_eq!(effective.phase_gate.dispatch_type, "agent-gate");
        assert_eq!(effective.phase_gate.pass_verdict, "agent_passed");
        assert_eq!(
            effective.phase_gate.checks,
            vec!["agent_passed".to_string()]
        );

        pool.close().await;
        pg_db.drop().await;
    }

    #[test]
    fn to_graph_produces_nodes_and_edges() {
        let p = minimal_pipeline();
        let graph = p.to_graph();
        let nodes = graph["nodes"].as_array().unwrap();
        let edges = graph["edges"].as_array().unwrap();
        assert_eq!(nodes.len(), 3);
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[1]["type"], "gated");
        assert_eq!(edges[1]["gates"].as_array().unwrap().len(), 1);
    }
}
