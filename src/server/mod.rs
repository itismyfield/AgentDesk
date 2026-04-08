pub mod routes;
pub mod ws;

use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::routing::get;
use tower_http::services::ServeDir;

use crate::config::Config;
use crate::db::Db;
use crate::engine::PolicyEngine;
use crate::services::discord::health::HealthRegistry;

pub async fn run(
    config: Config,
    db: Db,
    engine: PolicyEngine,
    health_registry: Option<Arc<HealthRegistry>>,
) -> Result<()> {
    crate::services::memory::refresh_backend_health("startup").await;

    // Startup: drain any deferred hooks persisted before last shutdown (#125)
    engine.drain_startup_hooks();

    // #251 (boot-only): reconcile broken DB/runtime state before workers begin
    // draining outbox rows or ticks resume.
    crate::reconcile::reconcile_boot_runtime(&db, &engine)?;

    // Spawn periodic GitHub sync task
    let sync_interval = config.github.sync_interval_minutes;
    if sync_interval > 0 {
        let sync_db = db.clone();
        let sync_engine = engine.clone();
        tokio::spawn(async move {
            github_sync_loop(sync_db, sync_engine, sync_interval).await;
        });
    }

    // Spawn periodic policy tick on a DEDICATED OS thread to avoid
    // engine lock deadlock with request handler threads.
    // The std::thread runs its own blocking loop, never competing with
    // tokio workers for the engine Mutex.
    {
        let tick_engine = engine.clone();
        let tick_db = db.clone();
        std::thread::Builder::new()
            .name("policy-tick".to_string())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap_or_else(|e| {
                        eprintln!("Fatal: failed to create policy-tick runtime: {e}");
                        std::process::exit(1);
                    });
                rt.block_on(policy_tick_loop(tick_engine, tick_db));
            })
            .map_err(|e| anyhow::anyhow!("Failed to spawn policy-tick thread: {e}"))?;
    }

    // Spawn periodic rate-limit cache sync (every 120s)
    {
        let rl_db = db.clone();
        tokio::spawn(async move {
            rate_limit_sync_loop(rl_db).await;
        });
    }

    // Spawn async message outbox worker (#120) — drains queued messages
    {
        let outbox_db = db.clone();
        let outbox_port = config.server.port;
        tokio::spawn(async move {
            message_outbox_loop(outbox_db, outbox_port).await;
        });
    }

    // #144: Spawn dispatch notification outbox worker — centralizes Discord side-effects
    {
        let dispatch_outbox_db = db.clone();
        tokio::spawn(async move {
            routes::dispatches::dispatch_outbox_loop(dispatch_outbox_db).await;
        });
    }

    // #189: Spawn DM reply notification retry loop (5-min interval)
    {
        let dm_retry_db = db.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(300));
            interval.tick().await; // skip immediate first tick
            loop {
                interval.tick().await;
                crate::services::discord::retry_failed_dm_notifications(&dm_retry_db).await;
            }
        });
    }

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
    let batch_buffer = ws::spawn_batch_flusher(broadcast_tx.clone());

    // Seed config defaults + store server port in kv_meta so policy JS can read them
    if let Ok(conn) = db.lock() {
        routes::settings::seed_config_defaults(&conn);
        // server_port is always overwritten (not INSERT OR IGNORE) to match current config
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('server_port', ?1)",
            [config.server.port.to_string()],
        )
        .ok();
    }

    let app = Router::new()
        .route("/ws", get(ws::ws_handler).with_state(broadcast_tx.clone()))
        .nest(
            "/api",
            routes::api_router(
                db.clone(),
                engine.clone(),
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

        // ── 30s tier: every tick ── (#134: fire by name for dynamic hook binding)
        fire_tick_hook_by_name(&engine, &db, "OnTick30s", "30s");
        drain_transitions(&engine, &db);

        // ── 1min tier: every 2nd tick (60s) ──
        if count % 2 == 0 {
            fire_tick_hook_by_name(&engine, &db, "OnTick1min", "1min");
            drain_transitions(&engine, &db);
        }

        // ── 5min tier: every 10th tick (300s) ──
        if count % 10 == 0 {
            fire_tick_hook_by_name(&engine, &db, "OnTick5min", "5min");
            drain_transitions(&engine, &db);
            crate::services::memory::refresh_backend_health("OnTick5min").await;
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

/// Async worker that drains the message_outbox table and delivers via /api/send (#120).
/// Runs every 2 seconds, processes up to 10 messages per tick.
async fn message_outbox_loop(db: Db, port: u16) {
    use std::time::Duration;

    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("[outbox] Failed to create HTTP client: {e}");
            return;
        }
    };

    let url = crate::config::local_api_url(port, "/api/send");

    // Wait for server to be ready
    tokio::time::sleep(Duration::from_secs(3)).await;
    tracing::info!("[outbox] Message outbox worker started (adaptive backoff 500ms-5s)");

    let mut poll_interval = Duration::from_millis(500);
    let max_interval = Duration::from_secs(5);

    loop {
        tokio::time::sleep(poll_interval).await;

        // Fetch pending messages
        let pending: Vec<(i64, String, String, String, String)> = {
            let conn = match db.lock() {
                Ok(c) => c,
                Err(_) => continue,
            };
            let mut stmt = match conn.prepare(
                "SELECT id, target, content, bot, source FROM message_outbox \
                 WHERE status = 'pending' ORDER BY id ASC LIMIT 10",
            ) {
                Ok(s) => s,
                Err(_) => continue,
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
            .map(|rows| rows.filter_map(|r| r.ok()).collect())
            .unwrap_or_default()
        };

        if pending.is_empty() {
            // No work: increase interval (up to max)
            poll_interval = (poll_interval.mul_f64(1.5)).min(max_interval);
            continue;
        }
        // Work found: reset to fast polling
        poll_interval = Duration::from_millis(500);

        for (id, target, content, bot, source) in pending {
            let body = serde_json::json!({
                "target": target,
                "content": content,
                "bot": bot,
                "source": source,
            });

            match client.post(&url).json(&body).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "UPDATE message_outbox SET status = 'sent', sent_at = datetime('now') WHERE id = ?1",
                            [id],
                        )
                        .ok();
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::debug!("[{ts}] [outbox] ✅ delivered msg {id} → {target}");
                }
                Ok(resp) => {
                    let status = resp.status();
                    let err_text = resp.text().await.unwrap_or_default();
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "UPDATE message_outbox SET status = 'failed', error = ?1 WHERE id = ?2",
                            rusqlite::params![format!("{status}: {err_text}"), id],
                        )
                        .ok();
                    }
                    tracing::warn!("[outbox] ❌ msg {id} → {target} failed: {status}");
                }
                Err(e) => {
                    if let Ok(conn) = db.lock() {
                        conn.execute(
                            "UPDATE message_outbox SET status = 'failed', error = ?1 WHERE id = ?2",
                            rusqlite::params![e.to_string(), id],
                        )
                        .ok();
                    }
                    tracing::warn!("[outbox] ❌ msg {id} → {target} error: {e}");
                }
            }
        }
    }
}
