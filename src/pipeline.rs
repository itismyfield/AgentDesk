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
use sqlx::{PgPool, Row as SqlxRow};
use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

/// Global singleton pipeline config (the default), loaded once at startup.
static PIPELINE: OnceLock<PipelineConfig> = OnceLock::new();
const PIPELINE_OVERRIDE_HEALTH_KV_KEY: &str = "pipeline_override_health_report";
const PIPELINE_OVERRIDE_AUDIT_ACTOR: &str = "pipeline";

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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PipelineOverrideParseFailure {
    pub layer: String,
    pub target_id: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PipelineOverrideReplaceWarning {
    pub layer: String,
    pub target_id: String,
    pub section: String,
    pub dropped_count: usize,
    pub dropped_items: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PipelineOverrideHealthReport {
    pub generated_at: String,
    pub status: String,
    pub warnings_count: usize,
    pub warnings: Vec<String>,
    pub parse_failures: Vec<PipelineOverrideParseFailure>,
    pub replace_warnings: Vec<PipelineOverrideReplaceWarning>,
}

#[derive(Debug, Clone)]
struct OverrideSourceRow {
    layer: &'static str,
    target_id: String,
    json: String,
}

impl PipelineOverrideHealthReport {
    fn finalize(&mut self) {
        self.parse_failures.sort_by(|left, right| {
            (&left.layer, &left.target_id, &left.error).cmp(&(
                &right.layer,
                &right.target_id,
                &right.error,
            ))
        });
        self.replace_warnings.sort_by(|left, right| {
            (&left.layer, &left.target_id, &left.section).cmp(&(
                &right.layer,
                &right.target_id,
                &right.section,
            ))
        });
        for warning in &mut self.replace_warnings {
            warning.dropped_items.sort();
            warning.dropped_items.dedup();
            warning.dropped_count = warning.dropped_items.len();
        }
        self.warnings.sort();
        self.warnings.dedup();
        self.warnings_count = self.warnings.len();
        self.status = if self.warnings.is_empty() {
            "ok".to_string()
        } else {
            "warn".to_string()
        };
    }
}

// reason: postgres override-health loader exercised by pipeline tests; the live
// refresh path persists via refresh_override_health_report, so this read-back
// helper has no production caller yet.
#[allow(dead_code)]
pub async fn load_persisted_override_health_report(
    _db: &crate::db::Db,
    pg_pool: Option<&PgPool>,
) -> Option<PipelineOverrideHealthReport> {
    if let Some(pool) = pg_pool {
        match sqlx::query_scalar::<_, String>("SELECT value FROM kv_meta WHERE key = $1")
            .bind(PIPELINE_OVERRIDE_HEALTH_KV_KEY)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(raw)) => return serde_json::from_str(&raw).ok(),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    "[pipeline] failed to load persisted postgres override health report: {error}"
                );
            }
        }
    }
    None
}

pub async fn refresh_override_health_report(
    pg_pool: Option<&PgPool>,
) -> PipelineOverrideHealthReport {
    ensure_loaded();
    let base = resolve(None, None);
    let rows = if let Some(pool) = pg_pool {
        match load_override_rows_pg(pool).await {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!("[pipeline] failed to scan postgres pipeline overrides: {error}");
                Vec::new()
            }
        }
    } else {
        tracing::warn!("[pipeline] postgres pool unavailable; skipping override health scan");
        Vec::new()
    };

    let mut report = build_override_health_report(&base, &rows);
    report.finalize();

    for warning in &report.warnings {
        tracing::warn!("[pipeline] {warning}");
    }

    persist_override_health_report(pg_pool, &report).await;
    record_override_audit_logs(pg_pool, &report).await;
    report
}

fn parse_override_for_resolve(
    layer: &str,
    target_id: &str,
    json: &str,
) -> Option<PipelineOverride> {
    match parse_override(json) {
        Ok(parsed) => parsed,
        Err(error) => {
            tracing::warn!("[pipeline] override parse failed for {layer}:{target_id}: {error}");
            None
        }
    }
}

async fn load_override_rows_pg(pool: &PgPool) -> Result<Vec<OverrideSourceRow>> {
    let mut rows = Vec::new();

    let repo_rows = sqlx::query(
        "SELECT id, pipeline_config::text AS pipeline_config
         FROM github_repos
         WHERE pipeline_config IS NOT NULL AND TRIM(pipeline_config::text) != ''",
    )
    .fetch_all(pool)
    .await
    .with_context(|| "scan postgres repo pipeline overrides")?;
    rows.extend(repo_rows.into_iter().filter_map(|row| {
        Some(OverrideSourceRow {
            layer: "repo",
            target_id: row.try_get::<String, _>("id").ok()?,
            json: row.try_get::<String, _>("pipeline_config").ok()?,
        })
    }));

    let agent_rows = sqlx::query(
        "SELECT id, pipeline_config::text AS pipeline_config
         FROM agents
         WHERE pipeline_config IS NOT NULL AND TRIM(pipeline_config::text) != ''",
    )
    .fetch_all(pool)
    .await
    .with_context(|| "scan postgres agent pipeline overrides")?;
    rows.extend(agent_rows.into_iter().filter_map(|row| {
        Some(OverrideSourceRow {
            layer: "agent",
            target_id: row.try_get::<String, _>("id").ok()?,
            json: row.try_get::<String, _>("pipeline_config").ok()?,
        })
    }));

    Ok(rows)
}

fn build_override_health_report(
    base: &PipelineConfig,
    rows: &[OverrideSourceRow],
) -> PipelineOverrideHealthReport {
    let mut report = PipelineOverrideHealthReport {
        generated_at: chrono::Utc::now().to_rfc3339(),
        ..PipelineOverrideHealthReport::default()
    };

    for row in rows {
        match parse_override(&row.json) {
            Ok(Some(ovr)) => {
                for warning in build_replace_warnings(base, &ovr, row.layer, &row.target_id) {
                    report.warnings.push(format_replace_warning(&warning));
                    report.replace_warnings.push(warning);
                }
            }
            Ok(None) => {}
            Err(error) => {
                let failure = PipelineOverrideParseFailure {
                    layer: row.layer.to_string(),
                    target_id: row.target_id.clone(),
                    error: error.to_string(),
                };
                report.warnings.push(format!(
                    "{} override {} parse failed: {}",
                    row.layer, row.target_id, failure.error
                ));
                report.parse_failures.push(failure);
            }
        }
    }

    report
}

fn build_replace_warnings(
    base: &PipelineConfig,
    override_cfg: &PipelineOverride,
    layer: &str,
    target_id: &str,
) -> Vec<PipelineOverrideReplaceWarning> {
    let mut warnings = Vec::new();

    if let Some(states) = override_cfg.states.as_ref() {
        push_replace_warning(
            &mut warnings,
            layer,
            target_id,
            "states",
            dropped_items(
                base.states.iter().map(|state| state.id.clone()),
                states.iter().map(|state| state.id.clone()),
            ),
        );
    }
    if let Some(transitions) = override_cfg.transitions.as_ref() {
        push_replace_warning(
            &mut warnings,
            layer,
            target_id,
            "transitions",
            dropped_items(
                base.transitions.iter().map(transition_label),
                transitions.iter().map(transition_label),
            ),
        );
    }
    if let Some(gates) = override_cfg.gates.as_ref() {
        push_replace_warning(
            &mut warnings,
            layer,
            target_id,
            "gates",
            dropped_items(base.gates.keys().cloned(), gates.keys().cloned()),
        );
    }
    if let Some(hooks) = override_cfg.hooks.as_ref() {
        push_replace_warning(
            &mut warnings,
            layer,
            target_id,
            "hooks",
            dropped_items(base.hooks.keys().cloned(), hooks.keys().cloned()),
        );
    }
    if let Some(events) = override_cfg.events.as_ref() {
        push_replace_warning(
            &mut warnings,
            layer,
            target_id,
            "events",
            dropped_items(base.events.keys().cloned(), events.keys().cloned()),
        );
    }
    if let Some(clocks) = override_cfg.clocks.as_ref() {
        push_replace_warning(
            &mut warnings,
            layer,
            target_id,
            "clocks",
            dropped_items(base.clocks.keys().cloned(), clocks.keys().cloned()),
        );
    }
    if let Some(timeouts) = override_cfg.timeouts.as_ref() {
        push_replace_warning(
            &mut warnings,
            layer,
            target_id,
            "timeouts",
            dropped_items(base.timeouts.keys().cloned(), timeouts.keys().cloned()),
        );
    }
    if let Some(phase_gate) = override_cfg.phase_gate.as_ref() {
        push_replace_warning(
            &mut warnings,
            layer,
            target_id,
            "phase_gate.checks",
            dropped_items(
                base.phase_gate.checks.iter().cloned(),
                phase_gate.checks.iter().cloned(),
            ),
        );
    }

    warnings
}

fn push_replace_warning(
    warnings: &mut Vec<PipelineOverrideReplaceWarning>,
    layer: &str,
    target_id: &str,
    section: &str,
    dropped_items: Vec<String>,
) {
    if dropped_items.is_empty() {
        return;
    }
    warnings.push(PipelineOverrideReplaceWarning {
        layer: layer.to_string(),
        target_id: target_id.to_string(),
        section: section.to_string(),
        dropped_count: dropped_items.len(),
        dropped_items,
    });
}

fn dropped_items(
    parent_items: impl IntoIterator<Item = String>,
    child_items: impl IntoIterator<Item = String>,
) -> Vec<String> {
    let child: std::collections::BTreeSet<String> = child_items.into_iter().collect();
    parent_items
        .into_iter()
        .filter(|item| !child.contains(item))
        .collect()
}

fn transition_label(transition: &TransitionConfig) -> String {
    format!("{}->{}", transition.from, transition.to)
}

fn format_replace_warning(warning: &PipelineOverrideReplaceWarning) -> String {
    let preview = warning
        .dropped_items
        .iter()
        .take(5)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{} override {} replaces {} and drops {} inherited entries{}",
        warning.layer,
        warning.target_id,
        warning.section,
        warning.dropped_count,
        if preview.is_empty() {
            String::new()
        } else {
            format!(": {preview}")
        }
    )
}

async fn persist_override_health_report(
    pg_pool: Option<&PgPool>,
    report: &PipelineOverrideHealthReport,
) {
    let Ok(rendered) = serde_json::to_string(report) else {
        return;
    };

    if let Some(pool) = pg_pool {
        match sqlx::query(
            "INSERT INTO kv_meta (key, value)
             VALUES ($1, $2)
             ON CONFLICT (key) DO UPDATE
             SET value = EXCLUDED.value",
        )
        .bind(PIPELINE_OVERRIDE_HEALTH_KV_KEY)
        .bind(&rendered)
        .execute(pool)
        .await
        {
            Ok(_) => return,
            Err(error) => {
                tracing::warn!(
                    "[pipeline] failed to persist postgres override health report: {error}"
                );
            }
        }
    }

    if pg_pool.is_none() {
        tracing::warn!(
            "[pipeline] postgres pool unavailable; not persisting override health report"
        );
    }
}

async fn record_override_audit_logs(
    pg_pool: Option<&PgPool>,
    report: &PipelineOverrideHealthReport,
) {
    if let Some(pool) = pg_pool {
        match pool.begin().await {
            Ok(mut tx) => {
                let mut failed = None;

                for failure in &report.parse_failures {
                    let entity_id = format!("{}:{}", failure.layer, failure.target_id);
                    let action = format!("pipeline_override_parse_failed: {}", failure.error);
                    if let Err(error) = sqlx::query(
                        "INSERT INTO audit_logs (entity_type, entity_id, action, actor)
                         VALUES ('pipeline_override', $1, $2, $3)",
                    )
                    .bind(&entity_id)
                    .bind(&action)
                    .bind(PIPELINE_OVERRIDE_AUDIT_ACTOR)
                    .execute(&mut *tx)
                    .await
                    {
                        failed = Some(error);
                        break;
                    }
                }

                if failed.is_none() {
                    for warning in &report.replace_warnings {
                        let entity_id = format!("{}:{}", warning.layer, warning.target_id);
                        let action = format!(
                            "pipeline_override_section_replace_warning:{} dropped {}",
                            warning.section, warning.dropped_count
                        );
                        if let Err(error) = sqlx::query(
                            "INSERT INTO audit_logs (entity_type, entity_id, action, actor)
                             VALUES ('pipeline_override', $1, $2, $3)",
                        )
                        .bind(&entity_id)
                        .bind(&action)
                        .bind(PIPELINE_OVERRIDE_AUDIT_ACTOR)
                        .execute(&mut *tx)
                        .await
                        {
                            failed = Some(error);
                            break;
                        }
                    }
                }

                match failed {
                    None => match tx.commit().await {
                        Ok(_) => return,
                        Err(error) => {
                            tracing::warn!(
                                "[pipeline] failed to commit postgres override audit logs: {error}"
                            );
                        }
                    },
                    Some(error) => {
                        tracing::warn!(
                            "[pipeline] failed to record postgres override audit logs: {error}"
                        );
                    }
                }
            }
            Err(error) => {
                tracing::warn!(
                    "[pipeline] failed to open postgres override audit log transaction: {error}"
                );
            }
        }
        return;
    }

    tracing::warn!("[pipeline] postgres pool unavailable; skipping override audit logs");
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
        .and_then(|json| parse_override_for_resolve("repo", rid, &json))
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
        .and_then(|json| parse_override_for_resolve("agent", aid, &json))
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

fn is_valid_state_id_slug(value: &str) -> bool {
    let mut bytes = value.bytes();
    match bytes.next() {
        Some(b'a'..=b'z') => {}
        _ => return false,
    }
    bytes.all(|byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'_'))
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

/// Backoff policy for stage retries (#1082).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BackoffPolicy {
    /// Fixed 1m → 5m → 15m exponential schedule.
    Exponential,
    /// Linear 5m between retries.
    Linear,
    /// No backoff — immediate retry by the next tick.
    None,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        BackoffPolicy::Exponential
    }
}

/// `on_failure` policy for stages (#1082).
// reason: staged-rollout policy enum retained for config compatibility; the
// runtime mirror lives in services::pipeline_routes and is not yet wired here.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OnFailurePolicy {
    /// Escalate to manual intervention / PM channel.
    Escalate,
    /// Retry according to `backoff` schedule until `max_retries` is reached.
    RetryWithBackoff,
    /// Fall back to the stage named by `on_failure_target`.
    FallbackStage,
    /// Fail the card immediately (backward-compatible default).
    Fail,
}

impl Default for OnFailurePolicy {
    fn default() -> Self {
        OnFailurePolicy::Fail
    }
}

/// `on_exhaust` policy for timeouts (#1082).
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnExhaustPolicy {
    /// Escalate to PM / manual intervention after retries exhausted.
    Escalate,
    /// Notify watchers without state change.
    Notify,
    /// Fail the card.
    Fail,
}

impl Default for OnExhaustPolicy {
    fn default() -> Self {
        OnExhaustPolicy::Escalate
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    pub duration: String,
    pub clock: String,
    #[serde(default)]
    pub max_retries: Option<u32>,
    /// Legacy free-form on_exhaust (state id to transition to).
    /// Preferred: use `on_exhaust_policy` for typed behavior.
    #[serde(default)]
    pub on_exhaust: Option<String>,
    /// Typed exhaust policy (#1082). When set, overrides legacy string behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_exhaust_policy: Option<OnExhaustPolicy>,
    /// Backoff policy between retries (#1082).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff: Option<BackoffPolicy>,
    #[serde(default)]
    pub condition: Option<String>,
}

// reason: #1082 timeout retry/backoff resolution surface retained for staged
// policy rollout; currently consumed only by the #1082 DoD unit tests, pending
// wiring into the live timeout executor.
#[allow(dead_code)]
impl TimeoutConfig {
    /// Default max_retries when caller did not specify (1, per #1082 DoD).
    pub const DEFAULT_MAX_RETRIES: u32 = 1;

    /// Resolve `max_retries`, defaulting to `DEFAULT_MAX_RETRIES` (>=1).
    pub fn effective_max_retries(&self) -> u32 {
        self.max_retries
            .filter(|v| *v >= 1)
            .unwrap_or(Self::DEFAULT_MAX_RETRIES)
    }

    /// Resolve backoff policy, defaulting to Exponential.
    pub fn effective_backoff(&self) -> BackoffPolicy {
        self.backoff.unwrap_or_default()
    }

    /// Resolve exhaust policy, defaulting to Escalate.
    pub fn effective_on_exhaust_policy(&self) -> OnExhaustPolicy {
        self.on_exhaust_policy.unwrap_or_default()
    }

    /// Compute backoff delay for the Nth retry (1-indexed).
    /// Exponential schedule: 1m → 5m → 15m → 15m (capped).
    /// Linear: 5m per retry.
    pub fn backoff_delay_seconds(&self, attempt: u32) -> u64 {
        match self.effective_backoff() {
            BackoffPolicy::None => 0,
            BackoffPolicy::Linear => 5 * 60,
            BackoffPolicy::Exponential => match attempt {
                0 | 1 => 60,
                2 => 5 * 60,
                _ => 15 * 60,
            },
        }
    }
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

        for state in &self.states {
            if !is_valid_state_id_slug(&state.id) {
                anyhow::bail!(
                    "state id '{}' must match kanban status slug contract ^[a-z][a-z0-9_]*$",
                    state.id
                );
            }
        }

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
            // #1082: max_retries must be >= 1 when explicitly set.
            if let Some(mr) = timeout.max_retries {
                if mr == 0 {
                    anyhow::bail!("timeout '{}' has max_retries=0; must be >= 1", key);
                }
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

#[cfg(test)]
mod state_slug_contract_tests {
    use super::*;

    #[test]
    fn validate_rejects_state_id_that_would_fail_kanban_status_check() {
        let config = PipelineConfig {
            name: "test".to_string(),
            version: 1,
            states: vec![StateConfig {
                id: "qa-test".to_string(),
                label: "QA Test".to_string(),
                terminal: false,
            }],
            transitions: Vec::new(),
            gates: HashMap::new(),
            hooks: HashMap::new(),
            events: HashMap::new(),
            clocks: HashMap::new(),
            timeouts: HashMap::new(),
            phase_gate: PhaseGateConfig::default(),
        };

        let err = config.validate().unwrap_err();
        assert!(
            err.to_string()
                .contains("kanban status slug contract ^[a-z][a-z0-9_]*$"),
            "{err}"
        );
    }
}

// ── Tests ────────────────────────────────────────────────────────
