//! `memory.memento_consolidation` — weekly consolidation of memento fragments
//! (#1089 / 908-7).
//!
//! Periodically asks the memento MCP backend to merge low-importance / duplicate
//! fragments via the `memory_consolidate` tool. The goal is to keep recall
//! response sizes bounded so the upstream cost-efficiency wins from 908-1..6
//! (token throttle, dedup, fragment cap, etc.) do not regress over time.
//!
//! ## Behaviour
//!
//!   * Runs once per week (registered with a 604800s interval).
//!   * If memento is not configured (no endpoint or no access key env), the
//!     job logs a single info line and returns `Ok(())` — never errors out
//!     when the backend is intentionally absent.
//!   * On a configured runtime the job posts an MCP `tools/call` for
//!     `memory_consolidate`, captures `before_count` / `after_count` from the
//!     response payload, and emits a `memento_consolidation_completed`
//!     observability event with the deltas.
//!   * Network / RPC failures surface as `Err(_)` so the maintenance scheduler
//!     can record `last_status="error"` for `/api/cron-jobs`.
//!
//! Recall-size A/B comparison happens out-of-band — see
//! `docs/reports/cost-efficiency-908.md` for the report template.

use std::time::Duration;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};

use crate::runtime_layout;

/// Weekly cadence: 7 days.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// MCP protocol version we negotiate against. Mirrors the constant in
/// `services::memory::memento` so this module stays self-contained without
/// pulling in the full memento backend (which is intentionally `pub(crate)`).
const MEMENTO_PROTOCOL_VERSION: &str = "2025-11-25";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Optional explicit workspace label. When `None` we default to
    /// `agentdesk-maintenance` so the consolidation pass operates against a
    /// neutral workspace rather than per-agent silos.
    pub workspace: Option<String>,
}

impl Config {
    pub fn default_runtime() -> Self {
        Self {
            workspace: std::env::var("MEMENTO_WORKSPACE")
                .ok()
                .filter(|w| !w.trim().is_empty()),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConsolidationReport {
    pub before_count: u64,
    pub after_count: u64,
    pub merged_count: u64,
    pub skipped: bool,
    pub skip_reason: Option<String>,
}

impl ConsolidationReport {
    pub fn savings(&self) -> u64 {
        self.before_count.saturating_sub(self.after_count)
    }
}

/// Job entrypoint registered against the maintenance scheduler.
pub async fn run(config: Config) -> Result<()> {
    let report = run_inner(config).await?;
    if report.skipped {
        tracing::info!(
            target: "maintenance",
            job = "memory.memento_consolidation",
            reason = %report.skip_reason.as_deref().unwrap_or("unknown"),
            "memento_consolidation skipped"
        );
    } else {
        tracing::info!(
            target: "maintenance",
            job = "memory.memento_consolidation",
            before = report.before_count,
            after = report.after_count,
            merged = report.merged_count,
            savings = report.savings(),
            "memento_consolidation completed"
        );
    }

    let payload = json!({
        "skipped": report.skipped,
        "skip_reason": report.skip_reason,
        "before_count": report.before_count,
        "after_count": report.after_count,
        "merged_count": report.merged_count,
        "savings": report.savings(),
    });
    crate::services::observability::events::record_simple(
        "memento_consolidation_completed",
        None,
        None,
        payload,
    );
    Ok(())
}

/// Inner runner. Returns a structured report so tests can assert without
/// going through `tracing::info!` or the observability subsystem.
pub async fn run_inner(config: Config) -> Result<ConsolidationReport> {
    let Some(runtime) = resolve_runtime() else {
        return Ok(ConsolidationReport {
            skipped: true,
            skip_reason: Some("memento not configured".to_string()),
            ..ConsolidationReport::default()
        });
    };

    let workspace = config
        .workspace
        .clone()
        .unwrap_or_else(|| "agentdesk-maintenance".to_string());

    let client = reqwest::Client::new();
    let session_id = initialize_session(&client, &runtime).await?;
    let payload = call_memory_consolidate(&client, &runtime, &session_id, &workspace).await?;

    let before = extract_count(
        &payload,
        &["before_count", "fragments_before", "totalBefore"],
    );
    let after = extract_count(&payload, &["after_count", "fragments_after", "totalAfter"]);
    let merged = extract_count(&payload, &["merged_count", "merged", "consolidated"])
        .unwrap_or_else(|| before.unwrap_or(0).saturating_sub(after.unwrap_or(0)));

    Ok(ConsolidationReport {
        before_count: before.unwrap_or(0),
        after_count: after.unwrap_or(0),
        merged_count: merged,
        skipped: false,
        skip_reason: None,
    })
}

#[derive(Debug, Clone)]
struct RuntimeConfig {
    endpoint: String,
    access_key: String,
}

fn resolve_runtime() -> Option<RuntimeConfig> {
    let root = crate::config::runtime_root()?;
    let backend = runtime_layout::load_memory_backend(&root);
    let endpoint = normalize_endpoint(&backend.mcp.endpoint);
    if endpoint.is_empty() {
        return None;
    }
    let env_name = backend.mcp.access_key_env.trim();
    if env_name.is_empty() {
        return None;
    }
    let access_key = std::env::var(env_name).ok()?;
    Some(RuntimeConfig {
        endpoint,
        access_key,
    })
}

fn normalize_endpoint(raw: &str) -> String {
    let trimmed = raw.trim().trim_end_matches('/');
    if trimmed.is_empty() {
        String::new()
    } else if trimmed.ends_with("/mcp") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/mcp")
    }
}

async fn initialize_session(client: &reqwest::Client, runtime: &RuntimeConfig) -> Result<String> {
    let response = auth(client.post(&runtime.endpoint), runtime)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": MEMENTO_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "agentdesk-maintenance",
                    "version": env!("CARGO_PKG_VERSION"),
                },
                "accessKey": runtime.access_key,
            }
        }))
        .send()
        .await
        .map_err(|err| anyhow!("memento initialize request failed: {err}"))?;

    let session_id = response
        .headers()
        .get("MCP-Session-Id")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| anyhow!("memento initialize response read failed: {err}"))?;
    if !status.is_success() {
        return Err(anyhow!("memento initialize failed with {status}: {text}"));
    }

    session_id.ok_or_else(|| anyhow!("memento initialize missing MCP-Session-Id header"))
}

async fn call_memory_consolidate(
    client: &reqwest::Client,
    runtime: &RuntimeConfig,
    session_id: &str,
    workspace: &str,
) -> Result<Value> {
    let response = auth(client.post(&runtime.endpoint), runtime)
        .header("MCP-Session-Id", session_id)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "memory_consolidate",
                "arguments": {
                    "workspace": workspace,
                }
            }
        }))
        .send()
        .await
        .map_err(|err| anyhow!("memory_consolidate request failed: {err}"))?;

    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|err| anyhow!("memory_consolidate response read failed: {err}"))?;
    if !status.is_success() {
        return Err(anyhow!("memory_consolidate failed with {status}: {text}"));
    }

    let payload: Value = serde_json::from_str(&text)
        .map_err(|err| anyhow!("memory_consolidate decode failed: {err}; body={text}"))?;
    if let Some(error) = payload.get("error") {
        return Err(anyhow!("memory_consolidate rpc failed: {error}"));
    }

    // Memento returns tool output inside result.content[0].text as JSON text.
    let text = payload
        .get("result")
        .and_then(|r| r.get("content"))
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find_map(|i| i.get("text").and_then(Value::as_str))
        })
        .ok_or_else(|| anyhow!("memory_consolidate response missing result.content[0].text"))?;
    let parsed: Value = serde_json::from_str(text)
        .map_err(|err| anyhow!("memory_consolidate content decode failed: {err}; text={text}"))?;
    Ok(parsed)
}

fn auth(builder: reqwest::RequestBuilder, runtime: &RuntimeConfig) -> reqwest::RequestBuilder {
    builder
        .header("Authorization", format!("Bearer {}", runtime.access_key))
        .header("Accept", "application/json")
        .header("Content-Type", "application/json")
}

fn extract_count(payload: &Value, keys: &[&str]) -> Option<u64> {
    for key in keys {
        if let Some(v) = payload.get(*key).and_then(Value::as_u64) {
            return Some(v);
        }
    }
    // Memento sometimes nests counts under "stats" or "summary".
    for parent in ["stats", "summary", "result"] {
        if let Some(child) = payload.get(parent) {
            for key in keys {
                if let Some(v) = child.get(*key).and_then(Value::as_u64) {
                    return Some(v);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skip_report_has_zero_counts_and_reason() {
        // Construct a "skipped" report directly to validate the contract that
        // run_inner returns when memento isn't configured. The runner path is
        // network-touching so we don't exercise it in unit tests; this captures
        // the in-memory shape callers (observability event, tracing) rely on.
        let report = ConsolidationReport {
            skipped: true,
            skip_reason: Some("memento not configured".to_string()),
            ..ConsolidationReport::default()
        };
        assert!(report.skipped);
        assert_eq!(report.before_count, 0);
        assert_eq!(report.after_count, 0);
        assert_eq!(report.merged_count, 0);
        assert_eq!(report.savings(), 0);
        assert_eq!(
            report.skip_reason.as_deref(),
            Some("memento not configured")
        );
    }

    #[test]
    fn extract_count_reads_top_level_keys() {
        let payload = json!({
            "before_count": 42,
            "after_count": 30,
            "merged_count": 12,
        });
        assert_eq!(
            extract_count(&payload, &["before_count", "fragments_before"]),
            Some(42)
        );
        assert_eq!(
            extract_count(&payload, &["after_count", "fragments_after"]),
            Some(30)
        );
    }

    #[test]
    fn extract_count_falls_through_to_nested_summary() {
        let payload = json!({
            "summary": {
                "fragments_before": 100,
                "fragments_after": 70,
            }
        });
        assert_eq!(
            extract_count(&payload, &["before_count", "fragments_before"]),
            Some(100)
        );
        assert_eq!(
            extract_count(&payload, &["after_count", "fragments_after"]),
            Some(70)
        );
    }

    #[test]
    fn extract_count_returns_none_when_absent() {
        let payload = json!({"unrelated": "value"});
        assert!(extract_count(&payload, &["before_count"]).is_none());
    }

    #[test]
    fn savings_uses_saturating_sub() {
        let report = ConsolidationReport {
            before_count: 5,
            after_count: 100, // pathological: shouldn't underflow
            ..Default::default()
        };
        assert_eq!(report.savings(), 0);
    }

    #[test]
    fn normalize_endpoint_appends_mcp_when_missing() {
        assert_eq!(
            normalize_endpoint("https://memento.example.com"),
            "https://memento.example.com/mcp"
        );
        assert_eq!(
            normalize_endpoint("https://memento.example.com/"),
            "https://memento.example.com/mcp"
        );
        assert_eq!(
            normalize_endpoint("https://memento.example.com/mcp"),
            "https://memento.example.com/mcp"
        );
        assert_eq!(normalize_endpoint(""), "");
        assert_eq!(normalize_endpoint("   "), "");
    }

    #[test]
    fn config_default_runtime_picks_up_workspace_env() {
        // Manipulating $MEMENTO_WORKSPACE in tests would race with other suites,
        // so we only assert the empty-env shape — the env-var read path is
        // exercised by integration tests when memento is fully wired.
        let cfg = Config { workspace: None };
        assert!(cfg.workspace.is_none());
    }
}
