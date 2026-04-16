pub mod routes;
mod worker_registry;
pub mod ws;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::routing::get;
use rusqlite::Connection;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use tower_http::services::ServeDir;

use crate::config::Config;
use crate::db::Db;
use crate::engine::PolicyEngine;
use crate::services::discord::health::HealthRegistry;

const MEMORY_HEALTH_STARTUP_REASON: &str = "startup";
const MEMORY_HEALTH_FIVE_MIN_REASON: &str = "OnTick5min";
const FIVE_MIN_POLICY_TICK_INTERVAL: u64 = 10;
const ESCALATION_PENDING_TTL_SEC: i64 = 600;

static DEPLOY_GATE_RUNNING: AtomicBool = AtomicBool::new(false);

fn deploy_gate_title(phase: i64) -> String {
    format!("[Deploy Gate] Phase {phase} 빌드+배포")
}

fn deploy_gate_failure_reason(phase: i64, detail: &str) -> String {
    format!("{} 실패: {}", deploy_gate_title(phase), detail.trim())
}

fn phase_live_entry_count(conn: &Connection, run_id: &str, phase: i64) -> i64 {
    conn.query_row(
        "SELECT COUNT(*)
         FROM auto_queue_entries
         WHERE run_id = ?1
           AND status IN ('pending', 'dispatched')
           AND COALESCE(batch_phase, 0) = ?2",
        rusqlite::params![run_id, phase],
        |row| row.get(0),
    )
    .unwrap_or(0)
}

fn deploy_gate_anchor_title(conn: &Connection, card_id: &str) -> String {
    conn.query_row(
        "SELECT title, github_issue_number FROM kanban_cards WHERE id = ?1",
        [card_id],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, Option<i64>>(1)?,
            ))
        },
    )
    .map(|(title, issue)| match (issue, title) {
        (Some(issue), Some(title)) if !title.trim().is_empty() => format!("#{issue} {title}"),
        (_, Some(title)) if !title.trim().is_empty() => title,
        _ => card_id.to_string(),
    })
    .unwrap_or_else(|_| card_id.to_string())
}

fn enqueue_deploy_gate_escalation(conn: &Connection, card_id: &str, reason: &str) {
    if card_id.trim().is_empty() || reason.trim().is_empty() {
        return;
    }

    let pending_key = format!("pm_pending:{card_id}");
    let mut entry = conn
        .query_row(
            "SELECT value FROM kv_meta WHERE key = ?1",
            [&pending_key],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        .filter(|value| value.is_object())
        .unwrap_or_else(|| json!({}));

    entry["title"] = json!(deploy_gate_anchor_title(conn, card_id));

    let mut reasons = entry
        .get("reasons")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    if !reasons
        .iter()
        .any(|value| value.as_str().map(|s| s == reason).unwrap_or(false))
    {
        reasons.push(json!(reason));
    }
    entry["reasons"] = serde_json::Value::Array(reasons);

    if let Ok(payload) = serde_json::to_string(&entry) {
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value, expires_at)
             VALUES (?1, ?2, datetime('now', '+' || ?3 || ' seconds'))",
            rusqlite::params![pending_key, payload, ESCALATION_PENDING_TTL_SEC.to_string()],
        )
        .ok();
    }
}

fn poll_deploy_gates(db: &Db) {
    if DEPLOY_GATE_RUNNING.load(Ordering::Acquire) {
        return;
    }

    let gate = {
        let Ok(conn) = db.lock() else { return };
        let mut stmt = match conn.prepare(
            "SELECT pg.run_id, pg.phase, r.deploy_phases, pg.anchor_card_id
             FROM auto_queue_phase_gates pg
             JOIN auto_queue_runs r ON r.id = pg.run_id
             WHERE pg.status = 'pending' AND r.deploy_phases IS NOT NULL
             LIMIT 1",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return,
        };
        let result: Option<(String, i64, String, Option<String>)> = stmt
            .query_row([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .ok();
        let Some((run_id, phase, deploy_phases_json, anchor_card_id)) = result else {
            return;
        };
        let deploy_phases: Vec<i64> = serde_json::from_str(&deploy_phases_json).unwrap_or_default();
        if !deploy_phases.contains(&phase) {
            return;
        }
        let live_phase_entries = phase_live_entry_count(&conn, &run_id, phase);
        if live_phase_entries > 0 {
            tracing::info!(
                "[deploy-gate] clearing stale gate for run {} phase {} — {} live entries remain",
                &run_id[..8.min(run_id.len())],
                phase,
                live_phase_entries
            );
            conn.execute(
                "DELETE FROM auto_queue_phase_gates WHERE run_id = ?1 AND phase = ?2",
                rusqlite::params![run_id, phase],
            )
            .ok();
            conn.execute(
                "UPDATE auto_queue_runs SET status = 'active', completed_at = NULL WHERE id = ?1 AND status = 'paused'",
                rusqlite::params![run_id],
            )
            .ok();
            return;
        }
        (run_id, phase, anchor_card_id)
    };

    let (run_id, phase, anchor_card_id) = gate;
    let db = db.clone();
    DEPLOY_GATE_RUNNING.store(true, Ordering::Release);

    std::thread::spawn(move || {
        let body = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            tracing::info!(
                "[deploy-gate] starting deploy for run {} phase {}",
                &run_id[..8.min(run_id.len())],
                phase
            );

            let result = crate::engine::ops::deploy_ops::run_deploy();
            let success = result
                .get("ok")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let summary = result
                .get("summary")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown");

            if let Ok(conn) = db.lock() {
                if success {
                    tracing::info!(
                        "[deploy-gate] deploy succeeded for run {} phase {}: {}",
                        &run_id[..8.min(run_id.len())],
                        phase,
                        summary
                    );
                    conn.execute(
                        "DELETE FROM auto_queue_phase_gates WHERE run_id = ?1 AND phase = ?2",
                        rusqlite::params![run_id, phase],
                    )
                    .ok();
                    conn.execute(
                        "UPDATE auto_queue_runs SET status = 'active', completed_at = NULL WHERE id = ?1 AND status = 'paused'",
                        rusqlite::params![run_id],
                    )
                    .ok();
                } else {
                    let error = result
                        .get("error")
                        .and_then(|value| value.as_str())
                        .unwrap_or("deploy failed");
                    tracing::warn!(
                        "[deploy-gate] deploy failed for run {} phase {}: {}",
                        &run_id[..8.min(run_id.len())],
                        phase,
                        error
                    );
                    conn.execute(
                        "UPDATE auto_queue_phase_gates
                         SET status = 'failed', failure_reason = ?3
                         WHERE run_id = ?1 AND phase = ?2",
                        rusqlite::params![run_id, phase, error],
                    )
                    .ok();
                    if let Some(anchor_card_id) = anchor_card_id.as_deref() {
                        enqueue_deploy_gate_escalation(
                            &conn,
                            anchor_card_id,
                            &deploy_gate_failure_reason(phase, error),
                        );
                    }
                }
            }
        }));

        if let Err(panic) = body {
            let msg = panic
                .downcast_ref::<String>()
                .map(|s| s.as_str())
                .or_else(|| panic.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            tracing::error!("[deploy-gate] thread panicked: {}", msg);
        }

        DEPLOY_GATE_RUNNING.store(false, Ordering::Release);
    });
}

async fn refresh_memory_health_for_startup() {
    crate::services::memory::refresh_backend_health(MEMORY_HEALTH_STARTUP_REASON).await;
}

async fn refresh_memory_health_for_five_min_tick() {
    crate::services::memory::refresh_backend_health(MEMORY_HEALTH_FIVE_MIN_REASON).await;
}

fn is_five_min_policy_tick(count: u64) -> bool {
    count != 0 && count % FIVE_MIN_POLICY_TICK_INTERVAL == 0
}

pub async fn run(
    config: Config,
    db: Db,
    engine: PolicyEngine,
    health_registry: Option<Arc<HealthRegistry>>,
) -> Result<()> {
    seed_startup_runtime_state(&db, &config);

    let mut worker_registry = worker_registry::SupervisedWorkerRegistry::new(
        config.clone(),
        db.clone(),
        engine.clone(),
        health_registry.clone(),
    );
    worker_registry.run_boot_only_steps().await?;
    worker_registry.start_after_boot_reconcile()?;

    // Resolve dashboard dist path relative to runtime root or binary location
    let dashboard_dir = crate::cli::agentdesk_runtime_root()
        .map(|r| r.join("dashboard/dist"))
        .unwrap_or_else(|| std::path::PathBuf::from("dashboard/dist"));

    // Auto-provision: if runtime dist is missing, copy from workspace source
    if !dashboard_dir.join("index.html").exists() {
        let workspace_dist =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dashboard/dist");
        if workspace_dist.join("index.html").exists() {
            tracing::info!(
                "Dashboard dist missing at {:?}, copying from workspace {:?}",
                dashboard_dir,
                workspace_dist
            );
            if let Some(parent) = dashboard_dir.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // Remove stale dist dir if it exists but is incomplete
            let _ = std::fs::remove_dir_all(&dashboard_dir);
            match copy_dir_recursive(&workspace_dist, &dashboard_dir) {
                Ok(n) => tracing::info!("Dashboard dist copied ({n} files)"),
                Err(e) => tracing::warn!("Failed to copy dashboard dist: {e}"),
            }
        } else {
            tracing::warn!(
                "Dashboard dist not found at {:?} or {:?} — dashboard will be unavailable",
                dashboard_dir,
                workspace_dist
            );
        }
    }

    tracing::info!("Serving dashboard from {:?}", dashboard_dir);

    let broadcast_tx = ws::new_broadcast();
    let batch_buffer = worker_registry.start_after_websocket_broadcast(broadcast_tx.clone())?;

    let app = Router::new()
        .route("/ws", get(ws::ws_handler).with_state(broadcast_tx.clone()))
        .nest(
            "/api",
            routes::api_router(
                db.clone(),
                engine.clone(),
                config.clone(),
                broadcast_tx.clone(),
                batch_buffer,
                health_registry,
            ),
        )
        .fallback_service(ServeDir::new(&dashboard_dir).append_index_html_on_directories(true));

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("HTTP server listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

fn seed_startup_runtime_state(db: &Db, config: &Config) {
    if let Ok(conn) = db.lock() {
        routes::settings::seed_config_defaults(&conn, config);
        // server_port is always overwritten (not INSERT OR IGNORE) to match current config
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('server_port', ?1)",
            [config.server.port.to_string()],
        )
        .ok();
    } else {
        tracing::warn!("[startup] failed to lock db for config default seeding");
    }

    if let Err(error) = seed_github_repos_from_config(db, config) {
        tracing::warn!("[startup] failed to seed github repos from config: {error}");
    }
}

fn seed_github_repos_from_config(db: &Db, config: &Config) -> std::result::Result<(), String> {
    use std::collections::BTreeSet;

    let mut repo_ids = BTreeSet::new();
    for raw_repo_id in &config.github.repos {
        let repo_id = raw_repo_id.trim();
        if repo_id.is_empty() {
            continue;
        }
        if !repo_id.contains('/') {
            tracing::warn!(
                "[startup] skipping invalid github.repos entry {:?}: expected owner/repo",
                raw_repo_id
            );
            continue;
        }
        repo_ids.insert(repo_id.to_string());
    }

    for repo_id in repo_ids {
        crate::github::register_repo(db, &repo_id)?;
    }

    Ok(())
}

/// Background task that fires tiered OnTick hooks at different intervals (#127).
///
/// 3 tiers to prevent slow sections from blocking time-critical recovery:
/// - OnTick30s (30s): retry, unsent notification recovery, deadlock detection [I], orphan recovery [K]
/// - OnTick1min (1m): non-critical timeouts [A][C][D][E][L], stale detection
/// - OnTick5min (5m): non-critical reconciliation [R][B][F][G][H][M][O], idle session cleanup
/// - OnTick (legacy, 5m): backward compat for policies that only register onTick
async fn policy_tick_loop(engine: PolicyEngine, db: Db) {
    use std::time::Duration;

    tracing::info!("[policy-tick] 3-tier tick started: 30s / 1min / 5min");

    let mut interval_30s = tokio::time::interval(Duration::from_secs(30));
    let mut count = 0u64;

    // Skip the first immediate tick
    interval_30s.tick().await;

    loop {
        interval_30s.tick().await;
        count += 1;

        poll_deploy_gates(&db);

        // ── 30s tier: every tick ── (#134: fire by name for dynamic hook binding)
        fire_tick_hook_by_name(&engine, &db, "OnTick30s", "30s");
        drain_transitions(&engine, &db);

        // ── 1min tier: every 2nd tick (60s) ──
        if count % 2 == 0 {
            fire_tick_hook_by_name(&engine, &db, "OnTick1min", "1min");
            drain_transitions(&engine, &db);
        }

        // ── 5min tier: every 10th tick (300s) ──
        if is_five_min_policy_tick(count) {
            fire_tick_hook_by_name(&engine, &db, "OnTick5min", "5min");
            drain_transitions(&engine, &db);
            refresh_memory_health_for_five_min_tick().await;
            if let Err(error) =
                crate::services::api_friction::process_api_friction_patterns(&db, None, None).await
            {
                tracing::warn!("[policy-tick] api-friction aggregation failed: {error}");
            }
            // Also fire legacy OnTick for backward compat
            fire_tick_hook_by_name(&engine, &db, "OnTick", "legacy");
            drain_transitions(&engine, &db);
        }
    }
}

/// Fire a single tick hook by name, log timing, record telemetry, and notify any dispatches created by JS.
/// Uses try_fire_hook_by_name for dynamic hook binding (#134).
fn fire_tick_hook_by_name(engine: &PolicyEngine, db: &Db, hook_name: &str, label: &str) {
    let start = std::time::Instant::now();
    let now_ms = chrono::Utc::now().timestamp_millis().to_string();

    let key_ms = format!("last_tick_{}_ms", label);
    let key_status = format!("last_tick_{}_status", label);

    if let Err(e) = engine.try_fire_hook_by_name(hook_name, serde_json::json!({})) {
        tracing::warn!("[policy-tick] {} hook error: {e}", label);
        if let Ok(conn) = db.lock() {
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, 'error')",
                [&key_status],
            )
            .ok();
        }
    } else {
        let elapsed = start.elapsed();
        if elapsed.as_millis() > 500 {
            tracing::warn!("[policy-tick] {} took {}ms", label, elapsed.as_millis());
        } else {
            tracing::debug!("[policy-tick] {} took {}ms", label, elapsed.as_millis());
        }
        if let Ok(conn) = db.lock() {
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params![key_ms, now_ms],
            )
            .ok();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, 'ok')",
                [&key_status],
            )
            .ok();
            // Also update legacy key for backward compat
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('last_tick_ms', ?1)",
                [&now_ms],
            )
            .ok();
            conn.execute(
                "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('last_tick_status', 'ok')",
                [],
            )
            .ok();
        }
    }

    crate::kanban::drain_hook_side_effects(db, engine);
}

/// Drain pending transitions after each tier execution.
fn drain_transitions(engine: &PolicyEngine, db: &Db) {
    crate::kanban::drain_hook_side_effects(db, engine);
}

/// Background task that periodically fetches rate-limit data from external providers
/// and caches it in the `rate_limit_cache` table for the dashboard API.
async fn rate_limit_sync_loop(db: Db) {
    use std::time::Duration;

    let interval = Duration::from_secs(120);
    // Run immediately on startup, then every 2 minutes
    let mut first = true;

    loop {
        if !first {
            tokio::time::sleep(interval).await;
        }
        first = false;

        // --- Claude rate limits ---
        // Priority: 1) OAuth token (Claude Code subscription), 2) ANTHROPIC_API_KEY
        let claude_result = if let Some(token) = get_claude_oauth_token() {
            fetch_claude_oauth_usage(&token).await
        } else if let Ok(api_key) = std::env::var("ANTHROPIC_API_KEY") {
            fetch_anthropic_rate_limits(&api_key).await
        } else {
            Err(anyhow::anyhow!("no Claude credentials found"))
        };
        match claude_result {
            Ok(buckets) => {
                let data = serde_json::json!({ "buckets": buckets }).to_string();
                let now = chrono::Utc::now().timestamp();
                if let Ok(conn) = db.lock() {
                    conn.execute(
                        "INSERT OR REPLACE INTO rate_limit_cache (provider, data, fetched_at) VALUES (?1, ?2, ?3)",
                        rusqlite::params!["claude", data, now],
                    )
                    .ok();
                }
                tracing::info!("[rate-limit-sync] Claude: {} buckets cached", buckets.len());
            }
            Err(e) => {
                tracing::warn!("[rate-limit-sync] Claude rate_limit fetch failed: {e}");
            }
        }

        // --- Codex rate limits ---
        // Priority: 1) ~/.codex/auth.json (Codex CLI subscription), 2) OPENAI_API_KEY
        let codex_result = if let Some(token) = load_codex_access_token() {
            fetch_codex_oauth_usage(&token).await
        } else if let Ok(api_key) = std::env::var("OPENAI_API_KEY") {
            fetch_openai_rate_limits(&api_key).await
        } else {
            Err(anyhow::anyhow!("no Codex credentials found"))
        };
        match codex_result {
            Ok(buckets) => {
                let data = serde_json::json!({ "buckets": buckets }).to_string();
                let now = chrono::Utc::now().timestamp();
                if let Ok(conn) = db.lock() {
                    conn.execute(
                        "INSERT OR REPLACE INTO rate_limit_cache (provider, data, fetched_at) VALUES (?1, ?2, ?3)",
                        rusqlite::params!["codex", data, now],
                    )
                    .ok();
                }
                tracing::info!("[rate-limit-sync] Codex: {} buckets cached", buckets.len());
            }
            Err(e) => {
                tracing::warn!("[rate-limit-sync] Codex rate_limit fetch failed: {e}");
            }
        }

        // --- Gemini rate limits ---
        // Uses OAuth2 creds from ~/.gemini/oauth_creds.json.
        // Returns RPM/RPD buckets with known quota limits; usage fields are -1 (unavailable).
        match fetch_gemini_rate_limits().await {
            Ok(buckets) => {
                let n = buckets.len();
                let data = serde_json::json!({ "buckets": buckets }).to_string();
                let now = chrono::Utc::now().timestamp();
                if let Ok(conn) = db.lock() {
                    conn.execute(
                        "INSERT OR REPLACE INTO rate_limit_cache (provider, data, fetched_at) VALUES (?1, ?2, ?3)",
                        rusqlite::params!["gemini", data, now],
                    )
                    .ok();
                }
                tracing::info!("[rate-limit-sync] Gemini: {} buckets cached", n);
            }
            Err(e) => {
                tracing::warn!("[rate-limit-sync] Gemini rate_limit fetch failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    fn test_db() -> Db {
        crate::db::test_db()
    }

    fn test_engine_with_dir(db: &Db, dir: &std::path::Path) -> PolicyEngine {
        let mut config = crate::config::Config::default();
        config.policies.dir = dir.to_path_buf();
        config.policies.hot_reload = false;
        PolicyEngine::new(&config, db.clone()).unwrap()
    }

    fn insert_agent(conn: &Connection, agent_id: &str) {
        conn.execute(
            "INSERT INTO agents (id, name, provider, created_at, updated_at)
             VALUES (?1, ?2, 'codex', datetime('now'), datetime('now'))",
            rusqlite::params![agent_id, format!("Agent {agent_id}")],
        )
        .unwrap();
    }

    fn kv_value(db: &Db, key: &str) -> Option<String> {
        let conn = db.lock().unwrap();
        conn.query_row("SELECT value FROM kv_meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .ok()
    }

    fn repo_ids(db: &Db) -> Vec<String> {
        crate::github::list_repos(db)
            .unwrap()
            .into_iter()
            .map(|repo| repo.id)
            .collect()
    }

    fn insert_pending_message(db: &Db, target: &str, content: &str) -> i64 {
        let conn = db.lock().unwrap();
        conn.execute(
            "INSERT INTO message_outbox (target, content, bot, source) VALUES (?1, ?2, 'notify', 'system')",
            rusqlite::params![target, content],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn message_row_status(db: &Db, id: i64) -> (String, Option<String>, Option<String>) {
        db.lock()
            .unwrap()
            .query_row(
                "SELECT status, error, sent_at FROM message_outbox WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap()
    }

    #[test]
    fn extract_gemini_quota_limits_preserves_paid_tier_values() {
        let payload = json!({
            "metrics": [{
                "metric": "generativelanguage.googleapis.com/generate_content_free_tier_requests",
                "consumerQuotaLimits": [
                    {
                        "unit": "1/min/{project}",
                        "quotaBuckets": [
                            {"effectiveLimit": "200"},
                            {"effectiveLimit": 120}
                        ]
                    },
                    {
                        "unit": "1/d/{project}",
                        "quotaBuckets": [
                            {"effectiveLimit": 20000},
                            {"effectiveLimit": 18000}
                        ]
                    }
                ]
            }]
        });

        let (rpm_limit, rpd_limit) = extract_gemini_quota_limits(&payload);
        assert_eq!(rpm_limit, 120);
        assert_eq!(rpd_limit, 18000);
    }

    #[test]
    fn extract_gemini_quota_limits_accepts_string_effective_limits() {
        let payload = json!({
            "metrics": [{
                "metric": "generativelanguage.googleapis.com/generate_content_free_tier_requests",
                "consumerQuotaLimits": [
                    {
                        "unit": "1/min/{project}",
                        "quotaBuckets": [
                            {"effectiveLimit": "200"},
                            {"effectiveLimit": "180"}
                        ]
                    },
                    {
                        "unit": "1/d/{project}",
                        "quotaBuckets": [
                            {"effectiveLimit": "30000"},
                            {"effectiveLimit": "20000"}
                        ]
                    }
                ]
            }]
        });

        let (rpm_limit, rpd_limit) = extract_gemini_quota_limits(&payload);
        assert_eq!(rpm_limit, 180);
        assert_eq!(rpd_limit, 20000);
    }

    #[test]
    fn build_gemini_rate_limit_buckets_uses_non_negative_usage_placeholders() {
        let buckets = build_gemini_rate_limit_buckets(15, 1500);

        assert_eq!(buckets[0]["used"], json!(0));
        assert_eq!(buckets[0]["remaining"], json!(15));
        assert_eq!(buckets[1]["used"], json!(0));
        assert_eq!(buckets[1]["remaining"], json!(1500));
    }

    #[tokio::test]
    async fn startup_memory_health_refresh_uses_startup_reason() {
        crate::services::memory::reset_backend_health_for_tests();
        refresh_memory_health_for_startup().await;
        assert_eq!(
            crate::services::memory::last_refresh_reason_for_tests().as_deref(),
            Some(MEMORY_HEALTH_STARTUP_REASON)
        );
    }

    #[test]
    fn five_min_policy_tick_runs_on_every_tenth_iteration() {
        assert!(!is_five_min_policy_tick(1));
        assert!(!is_five_min_policy_tick(9));
        assert!(is_five_min_policy_tick(10));
        assert!(is_five_min_policy_tick(20));
    }

    #[tokio::test]
    async fn five_min_memory_health_refresh_uses_tick_reason() {
        crate::services::memory::reset_backend_health_for_tests();
        refresh_memory_health_for_five_min_tick().await;
        assert_eq!(
            crate::services::memory::last_refresh_reason_for_tests().as_deref(),
            Some(MEMORY_HEALTH_FIVE_MIN_REASON)
        );
    }

    #[test]
    fn enqueue_deploy_gate_escalation_uses_phase_title() {
        let db = test_db();
        let conn = db.lock().unwrap();
        insert_agent(&conn, "agent-1");
        conn.execute(
            "INSERT INTO kanban_cards (id, title, github_issue_number, status, assigned_agent_id, created_at, updated_at)
             VALUES ('card-deploy-escalation', 'Deploy Anchor', 621, 'done', 'agent-1', datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();

        let reason = deploy_gate_failure_reason(1, "deploy-dev failed");
        enqueue_deploy_gate_escalation(&conn, "card-deploy-escalation", &reason);
        enqueue_deploy_gate_escalation(&conn, "card-deploy-escalation", &reason);

        let raw: String = conn
            .query_row(
                "SELECT value FROM kv_meta WHERE key = 'pm_pending:card-deploy-escalation'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let payload: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let reasons = payload["reasons"].as_array().cloned().unwrap_or_default();

        assert_eq!(payload["title"], json!("#621 Deploy Anchor"));
        assert_eq!(
            reasons.len(),
            1,
            "duplicate deploy gate reasons should be deduped"
        );
        assert_eq!(
            reasons[0],
            json!("[Deploy Gate] Phase 1 빌드+배포 실패: deploy-dev failed")
        );
    }

    #[test]
    fn poll_deploy_gates_clears_stale_gate_when_phase_still_has_live_entries() {
        DEPLOY_GATE_RUNNING.store(false, Ordering::Release);
        let db = test_db();
        {
            let conn = db.lock().unwrap();
            insert_agent(&conn, "agent-1");
            conn.execute(
                "INSERT INTO auto_queue_runs (id, repo, agent_id, status, deploy_phases, created_at)
                 VALUES ('run-stale-deploy-gate', 'test/repo', 'agent-1', 'paused', '[1]', datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
                 VALUES ('card-stale-deploy-anchor', 'Deploy Anchor', 'done', 'agent-1', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kanban_cards (id, title, status, assigned_agent_id, created_at, updated_at)
                 VALUES ('card-stale-deploy-live', 'Deploy Live', 'ready', 'agent-1', datetime('now'), datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_entries (id, run_id, kanban_card_id, agent_id, status, batch_phase, priority_rank, created_at)
                 VALUES ('entry-stale-deploy-live', 'run-stale-deploy-gate', 'card-stale-deploy-live', 'agent-1', 'pending', 1, 0, datetime('now'))",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO auto_queue_phase_gates (
                    run_id, phase, status, verdict, dispatch_id, pass_verdict, next_phase,
                    final_phase, anchor_card_id, failure_reason, created_at, updated_at
                 ) VALUES (
                    'run-stale-deploy-gate', 1, 'pending', NULL, NULL, 'phase_gate_passed', 2,
                    0, 'card-stale-deploy-anchor', NULL, datetime('now'), datetime('now')
                 )",
                [],
            )
            .unwrap();
        }

        poll_deploy_gates(&db);

        let conn = db.lock().unwrap();
        let run_status: String = conn
            .query_row(
                "SELECT status FROM auto_queue_runs WHERE id = 'run-stale-deploy-gate'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let gate_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM auto_queue_phase_gates WHERE run_id = 'run-stale-deploy-gate' AND phase = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let escalation_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM kv_meta WHERE key = 'pm_pending:card-stale-deploy-anchor'",
                [],
                |row| row.get(0),
            )
            .unwrap();

        assert_eq!(run_status, "active");
        assert_eq!(
            gate_count, 0,
            "stale deploy gates must be cleared before deploy starts"
        );
        assert_eq!(
            escalation_count, 0,
            "stale gate cleanup must not enqueue a PM escalation"
        );
    }

    #[test]
    fn seed_startup_runtime_state_records_server_port_and_registered_repos() {
        let db = test_db();
        let mut config = crate::config::Config::default();
        config.server.port = 43121;
        config.github.repos = vec!["owner/repo-a".to_string(), "owner/repo-b".to_string()];

        seed_startup_runtime_state(&db, &config);

        assert_eq!(kv_value(&db, "server_port").as_deref(), Some("43121"));
        assert_eq!(
            repo_ids(&db),
            vec!["owner/repo-a".to_string(), "owner/repo-b".to_string()]
        );
    }

    #[test]
    fn seed_startup_runtime_state_deduplicates_and_skips_invalid_repo_entries() {
        let db = test_db();
        let mut config = crate::config::Config::default();
        config.github.repos = vec![
            " owner/repo-a ".to_string(),
            "owner/repo-a".to_string(),
            "".to_string(),
            "noslash".to_string(),
            "owner/repo-b".to_string(),
        ];

        seed_startup_runtime_state(&db, &config);
        seed_startup_runtime_state(&db, &config);

        assert_eq!(
            repo_ids(&db),
            vec!["owner/repo-a".to_string(), "owner/repo-b".to_string()]
        );
    }

    #[test]
    fn tiered_tick_hooks_record_expected_markers_per_label() {
        crate::services::memory::reset_backend_health_for_tests();
        let dir = tempfile::TempDir::new().unwrap();
        std::fs::write(
            dir.path().join("tick-tier-probe.js"),
            r#"
            agentdesk.registerPolicy({
                name: "tick-tier-probe",
                priority: 1,
                onTick30s: function() {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('probe_30s', 'hit')"
                    );
                },
                onTick1min: function() {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('probe_1min', 'hit')"
                    );
                },
                onTick5min: function() {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('probe_5min', 'hit')"
                    );
                },
                onTick: function() {
                    agentdesk.db.execute(
                        "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('probe_legacy', 'hit')"
                    );
                }
            });
            "#,
        )
        .unwrap();

        let db = test_db();
        let engine = test_engine_with_dir(&db, dir.path());

        fire_tick_hook_by_name(&engine, &db, "OnTick30s", "30s");
        assert_eq!(kv_value(&db, "probe_30s").as_deref(), Some("hit"));
        assert_eq!(kv_value(&db, "last_tick_30s_status").as_deref(), Some("ok"));
        assert!(kv_value(&db, "last_tick_30s_ms").is_some());
        assert_eq!(kv_value(&db, "probe_1min"), None);
        assert_eq!(kv_value(&db, "probe_5min"), None);
        assert_eq!(kv_value(&db, "probe_legacy"), None);

        fire_tick_hook_by_name(&engine, &db, "OnTick1min", "1min");
        assert_eq!(kv_value(&db, "probe_1min").as_deref(), Some("hit"));
        assert_eq!(
            kv_value(&db, "last_tick_1min_status").as_deref(),
            Some("ok")
        );
        assert!(kv_value(&db, "last_tick_1min_ms").is_some());

        fire_tick_hook_by_name(&engine, &db, "OnTick5min", "5min");
        assert_eq!(kv_value(&db, "probe_5min").as_deref(), Some("hit"));
        assert_eq!(
            kv_value(&db, "last_tick_5min_status").as_deref(),
            Some("ok")
        );
        assert!(kv_value(&db, "last_tick_5min_ms").is_some());

        fire_tick_hook_by_name(&engine, &db, "OnTick", "legacy");
        assert_eq!(kv_value(&db, "probe_legacy").as_deref(), Some("hit"));
        assert_eq!(
            kv_value(&db, "last_tick_legacy_status").as_deref(),
            Some("ok")
        );
        assert!(kv_value(&db, "last_tick_legacy_ms").is_some());
        assert!(kv_value(&db, "last_tick_ms").is_some());
    }

    #[tokio::test]
    async fn drain_message_outbox_batch_marks_successful_rows_sent() {
        let db = test_db();
        let message_id = insert_pending_message(&db, "channel:1492506767085801535", "hello");
        let delivered = Arc::new(Mutex::new(Vec::new()));

        let processed = drain_message_outbox_batch_once(&db, {
            let delivered = delivered.clone();
            move |target, content, source, bot| {
                let delivered = delivered.clone();
                async move {
                    delivered.lock().unwrap().push(json!({
                        "target": target,
                        "content": content,
                        "source": source,
                        "bot": bot,
                    }));
                    ("200 OK".to_string(), json!({"ok": true}).to_string())
                }
            }
        })
        .await;

        let captured = delivered.lock().unwrap().clone();
        let (status, error, sent_at) = message_row_status(&db, message_id);

        assert_eq!(processed, 1, "one pending outbox row should be drained");
        assert_eq!(status, "sent");
        assert_eq!(error, None);
        assert!(sent_at.is_some(), "successful delivery must stamp sent_at");
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0]["target"], "channel:1492506767085801535");
        assert_eq!(captured[0]["content"], "hello");
        assert_eq!(captured[0]["source"], "system");
        assert_eq!(captured[0]["bot"], "notify");
    }

    #[tokio::test]
    async fn drain_message_outbox_batch_marks_http_failures_failed() {
        let db = test_db();
        let message_id = insert_pending_message(&db, "channel:1492506767085801535", "boom");

        let processed =
            drain_message_outbox_batch_once(&db, |_target, _content, _source, _bot| async {
                (
                    "500 Internal Server Error".to_string(),
                    json!({"error": "mock failure"}).to_string(),
                )
            })
            .await;

        let (status, error, sent_at) = message_row_status(&db, message_id);

        assert_eq!(
            processed, 1,
            "failed deliveries still consume the pending outbox row"
        );
        assert_eq!(status, "failed");
        assert_eq!(sent_at, None);
        let error = error.expect("failed rows must persist error details");
        assert!(error.contains("500 Internal Server Error"));
        assert!(error.contains("mock failure"));
    }
}

/// Fetch rate limits from the Anthropic API via the count_tokens endpoint (free, no token cost).
/// Parses `anthropic-ratelimit-*` response headers into bucket format.
async fn fetch_anthropic_rate_limits(
    api_key: &str,
) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let client = reqwest::Client::new();
    let resp = client
        .post("https://api.anthropic.com/v1/messages/count_tokens")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&serde_json::json!({
            "model": "claude-haiku-4-5-20251001",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await?;

    let headers = resp.headers().clone();
    let mut buckets = Vec::new();

    // Parse requests bucket
    if let Some(limit) = parse_header_i64(&headers, "anthropic-ratelimit-requests-limit") {
        let remaining =
            parse_header_i64(&headers, "anthropic-ratelimit-requests-remaining").unwrap_or(limit);
        let reset = parse_header_reset(&headers, "anthropic-ratelimit-requests-reset");
        buckets.push(serde_json::json!({
            "name": "requests",
            "limit": limit,
            "used": limit - remaining,
            "remaining": remaining,
            "reset": reset,
        }));
    }

    // Parse tokens bucket
    if let Some(limit) = parse_header_i64(&headers, "anthropic-ratelimit-tokens-limit") {
        let remaining =
            parse_header_i64(&headers, "anthropic-ratelimit-tokens-remaining").unwrap_or(limit);
        let reset = parse_header_reset(&headers, "anthropic-ratelimit-tokens-reset");
        buckets.push(serde_json::json!({
            "name": "tokens",
            "limit": limit,
            "used": limit - remaining,
            "remaining": remaining,
            "reset": reset,
        }));
    }

    Ok(buckets)
}

/// Fetch rate limits from the OpenAI API via the models endpoint (free, read-only).
/// Parses `x-ratelimit-*` response headers into bucket format.
async fn fetch_openai_rate_limits(api_key: &str) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.openai.com/v1/models")
        .header("authorization", format!("Bearer {api_key}"))
        .send()
        .await?;

    let headers = resp.headers().clone();
    let mut buckets = Vec::new();

    // OpenAI rate limit headers: x-ratelimit-limit-requests, x-ratelimit-remaining-requests, etc.
    if let Some(limit) = parse_header_i64(&headers, "x-ratelimit-limit-requests") {
        let remaining =
            parse_header_i64(&headers, "x-ratelimit-remaining-requests").unwrap_or(limit);
        let reset = parse_header_reset(&headers, "x-ratelimit-reset-requests");
        buckets.push(serde_json::json!({
            "name": "requests",
            "limit": limit,
            "used": limit - remaining,
            "remaining": remaining,
            "reset": reset,
        }));
    }

    if let Some(limit) = parse_header_i64(&headers, "x-ratelimit-limit-tokens") {
        let remaining = parse_header_i64(&headers, "x-ratelimit-remaining-tokens").unwrap_or(limit);
        let reset = parse_header_reset(&headers, "x-ratelimit-reset-tokens");
        buckets.push(serde_json::json!({
            "name": "tokens",
            "limit": limit,
            "used": limit - remaining,
            "remaining": remaining,
            "reset": reset,
        }));
    }

    Ok(buckets)
}

fn parse_header_i64(headers: &reqwest::header::HeaderMap, name: &str) -> Option<i64> {
    headers.get(name)?.to_str().ok()?.parse().ok()
}

/// Parse ISO 8601 reset timestamp from header into unix epoch seconds.
fn parse_header_reset(headers: &reqwest::header::HeaderMap, name: &str) -> i64 {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| {
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|dt| dt.timestamp())
        })
        .unwrap_or(0)
}

/// Read Claude Code OAuth token from macOS Keychain, falling back to ~/.claude/.credentials.json.
fn get_claude_oauth_token() -> Option<String> {
    // Try macOS Keychain first
    if let Ok(output) = std::process::Command::new("security")
        .args([
            "find-generic-password",
            "-s",
            "Claude Code-credentials",
            "-w",
        ])
        .output()
    {
        if output.status.success() {
            if let Ok(raw) = String::from_utf8(output.stdout) {
                let raw = raw.trim();
                if let Ok(creds) = serde_json::from_str::<serde_json::Value>(raw) {
                    if let Some(token) = creds
                        .get("claudeAiOauth")
                        .and_then(|o| o.get("accessToken"))
                        .and_then(|v| v.as_str())
                    {
                        return Some(token.to_string());
                    }
                }
            }
        }
    }
    // Fallback: credentials file
    let home = dirs::home_dir()?;
    let cred_path = home.join(".claude").join(".credentials.json");
    let raw = std::fs::read_to_string(cred_path).ok()?;
    let creds: serde_json::Value = serde_json::from_str(&raw).ok()?;
    creds
        .get("claudeAiOauth")
        .and_then(|o| o.get("accessToken"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Fetch Claude usage via OAuth API (subscription-based, no API key needed).
/// Returns utilization-based buckets (5h, 7d).
async fn fetch_claude_oauth_usage(token: &str) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://api.anthropic.com/api/oauth/usage")
        .header("accept", "application/json")
        .header("authorization", format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("user-agent", "agentdesk/1.0.0")
        .send()
        .await?;

    if resp.status() == 429 {
        return Err(anyhow::anyhow!("Claude OAuth usage API rate limited (429)"));
    }
    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Claude OAuth usage API returned {}",
            resp.status()
        ));
    }

    let data: serde_json::Value = resp.json().await?;
    let mut buckets = Vec::new();

    for key in &["five_hour", "seven_day", "seven_day_sonnet"] {
        if let Some(bucket) = data.get(key) {
            let utilization = bucket
                .get("utilization")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let resets_at = bucket
                .get("resets_at")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let label = match *key {
                "five_hour" => "5h",
                "seven_day" => "7d",
                "seven_day_sonnet" => "7d Sonnet",
                _ => key,
            };
            // Convert utilization (0-100 float) to used/limit format for consistency
            let limit = 100i64;
            let used = utilization.round() as i64;
            let reset_ts = chrono::DateTime::parse_from_rfc3339(resets_at)
                .map(|dt| dt.timestamp())
                .unwrap_or(0);

            buckets.push(serde_json::json!({
                "name": label,
                "limit": limit,
                "used": used,
                "remaining": limit - used,
                "reset": reset_ts,
            }));
        }
    }

    Ok(buckets)
}

/// Read Codex CLI access token from ~/.codex/auth.json.
fn load_codex_access_token() -> Option<String> {
    let home = dirs::home_dir()?;
    let auth_path = home.join(".codex").join("auth.json");
    let raw = std::fs::read_to_string(auth_path).ok()?;
    let auth: serde_json::Value = serde_json::from_str(&raw).ok()?;
    auth.get("tokens")
        .and_then(|t| t.get("access_token"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// Fetch Codex usage via chatgpt.com backend API (subscription-based, no API key needed).
async fn fetch_codex_oauth_usage(token: &str) -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://chatgpt.com/backend-api/codex/usage")
        .header("authorization", format!("Bearer {token}"))
        .header("user-agent", "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36")
        .header("accept", "application/json")
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Codex usage API returned {}",
            resp.status()
        ));
    }

    let data: serde_json::Value = resp.json().await?;
    let mut buckets = Vec::new();

    if let Some(rl) = data.get("rate_limit") {
        for window_key in &["primary_window", "secondary_window"] {
            if let Some(window) = rl.get(window_key) {
                let used_percent = window
                    .get("used_percent")
                    .and_then(|v| v.as_f64())
                    .unwrap_or(0.0);
                let window_seconds = window
                    .get("limit_window_seconds")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0);
                let reset_at = window.get("reset_at").and_then(|v| v.as_i64()).unwrap_or(0);

                let label = if window_seconds <= 18000 {
                    "5h"
                } else if window_seconds <= 86400 {
                    "1d"
                } else {
                    "7d"
                };

                let limit = 100i64;
                let used = used_percent.round() as i64;

                buckets.push(serde_json::json!({
                    "name": label,
                    "limit": limit,
                    "used": used,
                    "remaining": limit - used,
                    "reset": reset_at,
                }));
            }
        }
    }

    Ok(buckets)
}

// ── Gemini rate-limit helpers ─────────────────────────────────────────────────

/// Extract OAuth2 app credentials (client_id, client_secret) for the Gemini CLI
/// "installed app" flow.  These are public client credentials distributed inside
/// the Gemini CLI npm bundle — not server secrets.  The refresh_token in
/// ~/.gemini/oauth_creds.json is the actual per-user secret.
///
/// Resolution order:
///   1. env vars GEMINI_CLIENT_ID / GEMINI_CLIENT_SECRET
///   2. parse from the installed Gemini CLI bundle (e.g. Homebrew path)
fn load_gemini_oauth_app_creds() -> Result<(String, String), anyhow::Error> {
    // 1. Environment variables take precedence (CI / custom installs)
    if let (Ok(id), Ok(secret)) = (
        std::env::var("GEMINI_CLIENT_ID"),
        std::env::var("GEMINI_CLIENT_SECRET"),
    ) {
        return Ok((id, secret));
    }

    // 2. Parse from the Gemini CLI bundle on disk.
    //    Support both Homebrew Cellar installs and npm-global installs.
    let candidate_globs = [
        "/opt/homebrew/Cellar/gemini-cli/*/libexec/lib/node_modules/@google/gemini-cli/bundle/chunk-*.js",
        "/usr/local/Cellar/gemini-cli/*/libexec/lib/node_modules/@google/gemini-cli/bundle/chunk-*.js",
        "/opt/homebrew/lib/node_modules/@google/gemini-cli/bundle/chunk-*.js",
        "/usr/local/lib/node_modules/@google/gemini-cli/bundle/chunk-*.js",
        "/usr/lib/node_modules/@google/gemini-cli/bundle/chunk-*.js",
    ];

    for pattern in &candidate_globs {
        let Ok(paths) = glob::glob(pattern) else {
            continue;
        };
        for entry in paths.flatten() {
            let Ok(content) = std::fs::read_to_string(&entry) else {
                continue;
            };
            // Gemini CLI 0.38.x bundles export OAuth constants like:
            //   var OAUTH_CLIENT_ID = "<id>";
            //   var OAUTH_CLIENT_SECRET = "<secret>";
            // Older bundles also inline:
            //   clientId:"<id>",clientSecret:"<secret>"
            let id = extract_assigned_string(&content, "OAUTH_CLIENT_ID")
                .or_else(|| extract_quoted_value(&content, "clientId"));
            let secret = extract_assigned_string(&content, "OAUTH_CLIENT_SECRET")
                .or_else(|| extract_quoted_value(&content, "clientSecret"));
            if let (Some(id), Some(secret)) = (id, secret) {
                return Ok((id, secret));
            }
        }
    }

    Err(anyhow::anyhow!(
        "Gemini OAuth app credentials not found. \
         Set GEMINI_CLIENT_ID / GEMINI_CLIENT_SECRET env vars or install gemini-cli."
    ))
}

/// Extract the value of a key from a JS bundle snippet like `key:"value"`.
fn extract_quoted_value(src: &str, key: &str) -> Option<String> {
    // Match:  clientId:"<value>"  or  clientId:'<value>'
    let needle = format!("{key}:\"");
    if let Some(start) = src.find(&needle) {
        let rest = &src[start + needle.len()..];
        if let Some(end) = rest.find('"') {
            return Some(rest[..end].to_string());
        }
    }
    let needle_sq = format!("{key}:'");
    if let Some(start) = src.find(&needle_sq) {
        let rest = &src[start + needle_sq.len()..];
        if let Some(end) = rest.find('\'') {
            return Some(rest[..end].to_string());
        }
    }
    None
}

/// Extract the value of a JS assignment like `var KEY = "value";`.
fn extract_assigned_string(src: &str, key: &str) -> Option<String> {
    for quote in ['"', '\''] {
        let needle = format!("{key} = {quote}");
        if let Some(start) = src.find(&needle) {
            let rest = &src[start + needle.len()..];
            if let Some(end) = rest.find(quote) {
                return Some(rest[..end].to_string());
            }
        }
    }
    None
}

/// Read (and refresh if expired) the Gemini OAuth2 access token from
/// `~/.gemini/oauth_creds.json`.  Writes back the new token on refresh.
async fn load_gemini_access_token() -> Result<String, anyhow::Error> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("no home dir"))?;
    let creds_path = home.join(".gemini").join("oauth_creds.json");
    let raw = std::fs::read_to_string(&creds_path)
        .map_err(|e| anyhow::anyhow!("cannot read ~/.gemini/oauth_creds.json: {e}"))?;
    let mut creds: serde_json::Value = serde_json::from_str(&raw)?;

    let now_ms = chrono::Utc::now().timestamp_millis();
    let expiry_ms = creds
        .get("expiry_date")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    // 30-second buffer to avoid using a token that expires mid-request
    if expiry_ms > now_ms + 30_000 {
        return creds
            .get("access_token")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow::anyhow!("no access_token in oauth_creds.json"));
    }

    // Token expired — refresh via Google token endpoint
    let refresh_token = creds
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no refresh_token in oauth_creds.json"))?
        .to_string();

    let (client_id, client_secret) = load_gemini_oauth_app_creds()?;

    let client = reqwest::Client::new();
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .form(&[
            ("client_id", client_id.as_str()),
            ("client_secret", client_secret.as_str()),
            ("refresh_token", refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Gemini token refresh returned {}",
            resp.status()
        ));
    }

    let new_data: serde_json::Value = resp.json().await?;
    let new_access_token = new_data
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("no access_token in refresh response"))?
        .to_string();
    let expires_in = new_data
        .get("expires_in")
        .and_then(|v| v.as_i64())
        .unwrap_or(3600);

    // Persist refreshed token so the next call doesn't need to refresh again
    creds["access_token"] = serde_json::json!(new_access_token.clone());
    creds["expiry_date"] = serde_json::json!(now_ms + expires_in * 1000);
    if let Ok(updated) = serde_json::to_string_pretty(&creds) {
        let _ = std::fs::write(&creds_path, updated);
    }

    Ok(new_access_token)
}

/// Discover the "Default Gemini Project" ID via the Cloud Resource Manager API.
/// The Gemini CLI creates this project automatically during OAuth setup.
async fn discover_gemini_project_id(token: &str) -> Result<String, anyhow::Error> {
    let client = reqwest::Client::new();
    let resp = client
        .get("https://cloudresourcemanager.googleapis.com/v1/projects?filter=name:Default%20Gemini%20Project")
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Cloud Resource Manager returned {}",
            resp.status()
        ));
    }

    let data: serde_json::Value = resp.json().await?;
    data.get("projects")
        .and_then(|p| p.as_array())
        .and_then(|arr| arr.first())
        .and_then(|proj| proj.get("projectId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            anyhow::anyhow!("Default Gemini Project not found via Cloud Resource Manager")
        })
}

fn extract_gemini_quota_limits(data: &serde_json::Value) -> (i64, i64) {
    let mut rpm_limit: Option<i64> = None;
    let mut rpd_limit: Option<i64> = None;

    if let Some(metrics) = data.get("metrics").and_then(|m| m.as_array()) {
        for metric in metrics {
            let metric_name = metric.get("metric").and_then(|m| m.as_str()).unwrap_or("");
            if metric_name
                != "generativelanguage.googleapis.com/generate_content_free_tier_requests"
            {
                continue;
            }

            if let Some(limits) = metric.get("consumerQuotaLimits").and_then(|l| l.as_array()) {
                for limit in limits {
                    let unit = limit.get("unit").and_then(|u| u.as_str()).unwrap_or("");
                    let is_per_minute = unit.contains("/min/");
                    let is_per_day = unit.starts_with("1/d/");

                    if let Some(buckets) = limit.get("quotaBuckets").and_then(|b| b.as_array()) {
                        // Take the minimum positive limit across all model buckets —
                        // this reflects the tightest constraint a user is likely to hit.
                        let min_positive = buckets
                            .iter()
                            .filter_map(|b| {
                                b.get("effectiveLimit")
                                    .and_then(parse_gemini_effective_limit_value)
                            })
                            .filter(|&v| v > 0)
                            .min();

                        if let Some(min_val) = min_positive {
                            if is_per_minute {
                                rpm_limit =
                                    Some(rpm_limit.map_or(min_val, |current| current.min(min_val)));
                            } else if is_per_day {
                                rpd_limit =
                                    Some(rpd_limit.map_or(min_val, |current| current.min(min_val)));
                            }
                        }
                    }
                }
            }
        }
    }

    (rpm_limit.unwrap_or(15), rpd_limit.unwrap_or(1500))
}

fn parse_gemini_effective_limit_value(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_str().and_then(|raw| raw.parse::<i64>().ok()))
}

fn build_gemini_rate_limit_buckets(rpm_limit: i64, rpd_limit: i64) -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "name": "rpm",
            "limit": rpm_limit,
            "used": 0_i64,
            "remaining": rpm_limit,
            "reset": 0_i64,
        }),
        serde_json::json!({
            "name": "rpd",
            "limit": rpd_limit,
            "used": 0_i64,
            "remaining": rpd_limit,
            "reset": 0_i64,
        }),
    ]
}

/// Fetch Gemini quota limits via the Google Cloud ServiceUsage API.
///
/// Returns RPM and RPD buckets sourced from `generate_content_free_tier_requests`
/// quota metrics. The API does not expose real-time usage counters, so the
/// returned buckets use non-negative placeholder usage (`used = 0`,
/// `remaining = limit`) to keep downstream UI math stable.
async fn fetch_gemini_rate_limits() -> Result<Vec<serde_json::Value>, anyhow::Error> {
    let token = load_gemini_access_token().await?;
    let project_id = discover_gemini_project_id(&token).await?;

    let client = reqwest::Client::new();
    let url = format!(
        "https://serviceusage.googleapis.com/v1beta1/projects/{}/services/\
         generativelanguage.googleapis.com/consumerQuotaMetrics\
         ?fields=metrics.metric,metrics.consumerQuotaLimits",
        project_id
    );
    let resp = client
        .get(&url)
        .header("authorization", format!("Bearer {token}"))
        .send()
        .await?;

    if !resp.status().is_success() {
        return Err(anyhow::anyhow!(
            "Gemini ServiceUsage API returned {}",
            resp.status()
        ));
    }

    let data: serde_json::Value = resp.json().await?;
    let (rpm_limit, rpd_limit) = extract_gemini_quota_limits(&data);
    Ok(build_gemini_rate_limit_buckets(rpm_limit, rpd_limit))
}

/// Background task that periodically syncs GitHub issues for all registered repos.
async fn github_sync_loop(db: Db, engine: crate::engine::PolicyEngine, interval_minutes: u64) {
    use std::time::Duration;

    if !crate::github::gh_available() {
        tracing::warn!("[github-sync] gh CLI not available — periodic sync disabled");
        return;
    }

    tracing::info!(
        "[github-sync] Periodic sync enabled (every {} minutes)",
        interval_minutes
    );

    let interval = Duration::from_secs(interval_minutes * 60);

    loop {
        tokio::time::sleep(interval).await;

        tracing::debug!("[github-sync] Running periodic sync...");

        // Fetch repos
        let repos = match crate::github::list_repos(&db) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("[github-sync] Failed to list repos: {e}");
                continue;
            }
        };

        for repo in &repos {
            if !repo.sync_enabled {
                continue;
            }

            let issues = match crate::github::sync::fetch_issues(&repo.id) {
                Ok(i) => i,
                Err(e) => {
                    tracing::warn!("[github-sync] Fetch failed for {}: {e}", repo.id);
                    continue;
                }
            };

            // Triage new issues
            match crate::github::triage::triage_new_issues(&db, &repo.id, &issues) {
                Ok(n) if n > 0 => {
                    tracing::info!("[github-sync] Triaged {n} new issues for {}", repo.id);
                }
                Err(e) => {
                    tracing::warn!("[github-sync] Triage failed for {}: {e}", repo.id);
                }
                _ => {}
            }

            // Sync state
            match crate::github::sync::sync_github_issues_for_repo(&db, &engine, &repo.id, &issues)
            {
                Ok(r) => {
                    if r.closed_count > 0 || r.inconsistency_count > 0 {
                        tracing::info!(
                            "[github-sync] {}: closed={}, inconsistencies={}",
                            repo.id,
                            r.closed_count,
                            r.inconsistency_count
                        );
                    }
                }
                Err(e) => {
                    tracing::error!("[github-sync] Sync failed for {}: {e}", repo.id);
                }
            }
        }
    }
}

/// Recursively copy a directory tree. Returns the number of files copied.
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<usize> {
    std::fs::create_dir_all(dst)?;
    let mut count = 0;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dest_path = dst.join(entry.file_name());
        if ty.is_dir() {
            count += copy_dir_recursive(&entry.path(), &dest_path)?;
        } else {
            std::fs::copy(entry.path(), &dest_path)?;
            count += 1;
        }
    }
    Ok(count)
}

/// Async worker that drains the message_outbox table via the in-process Discord delivery path (#120).
/// Runs every 2 seconds, processes up to 10 messages per tick.
fn load_pending_message_outbox_batch(db: &Db) -> Vec<(i64, String, String, String, String)> {
    let conn = match db.lock() {
        Ok(conn) => conn,
        Err(_) => return Vec::new(),
    };
    let mut stmt = match conn.prepare(
        "SELECT id, target, content, bot, source FROM message_outbox \
         WHERE status = 'pending' ORDER BY id ASC LIMIT 10",
    ) {
        Ok(stmt) => stmt,
        Err(_) => return Vec::new(),
    };
    stmt.query_map([], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ))
    })
    .ok()
    .map(|rows| rows.filter_map(|row| row.ok()).collect())
    .unwrap_or_default()
}

async fn drain_message_outbox_batch_once<F, Fut>(db: &Db, mut deliver: F) -> usize
where
    F: FnMut(String, String, String, String) -> Fut,
    Fut: std::future::Future<Output = (String, String)>,
{
    let pending = load_pending_message_outbox_batch(db);
    if pending.is_empty() {
        return 0;
    }

    for (id, target, content, bot, source) in &pending {
        let (status, err_text) =
            deliver(target.clone(), content.clone(), source.clone(), bot.clone()).await;
        if status == "200 OK" {
            if let Ok(conn) = db.lock() {
                conn.execute(
                    "UPDATE message_outbox SET status = 'sent', sent_at = datetime('now') WHERE id = ?1",
                    [id],
                )
                .ok();
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::debug!("[{ts}] [outbox] ✅ delivered msg {id} → {target}");
        } else {
            if let Ok(conn) = db.lock() {
                conn.execute(
                    "UPDATE message_outbox SET status = 'failed', error = ?1 WHERE id = ?2",
                    rusqlite::params![format!("{status}: {err_text}"), id],
                )
                .ok();
            }
            tracing::warn!("[outbox] ❌ msg {id} → {target} failed: {status}");
        }
    }

    pending.len()
}

async fn message_outbox_loop(db: Db, health_registry: Option<Arc<HealthRegistry>>) {
    use std::time::Duration;

    let Some(health_registry) = health_registry else {
        tracing::error!("[outbox] Health registry unavailable; message outbox worker stopped");
        return;
    };

    // Give Discord runtime bootstrap a brief head start before polling.
    tokio::time::sleep(Duration::from_secs(3)).await;
    tracing::info!("[outbox] Message outbox worker started (adaptive backoff 500ms-5s)");

    let mut poll_interval = Duration::from_millis(500);
    let max_interval = Duration::from_secs(5);

    loop {
        tokio::time::sleep(poll_interval).await;
        if drain_message_outbox_batch_once(&db, {
            let health_registry = health_registry.clone();
            let db = db.clone();
            move |target, content, source, bot| {
                let health_registry = health_registry.clone();
                let db = db.clone();
                async move {
                    let (status, err_text) = crate::services::discord::health::send_message(
                        &health_registry,
                        &db,
                        &target,
                        &content,
                        &source,
                        &bot,
                    )
                    .await;
                    (status.to_string(), err_text)
                }
            }
        })
        .await
            == 0
        {
            // No work: increase interval (up to max)
            poll_interval = (poll_interval.mul_f64(1.5)).min(max_interval);
            continue;
        }
        // Work found: reset to fast polling
        poll_interval = Duration::from_millis(500);
    }
}

async fn dm_reply_retry_loop(db: Db) {
    let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
    interval.tick().await; // skip immediate first tick
    loop {
        interval.tick().await;
        crate::services::discord::retry_failed_dm_notifications(&db).await;
    }
}
