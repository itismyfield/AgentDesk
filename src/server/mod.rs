pub mod routes;
pub mod ws;

use anyhow::Result;
use axum::Router;
use axum::routing::get;
use tower_http::services::ServeDir;

use crate::config::Config;
use crate::db::Db;
use crate::engine::PolicyEngine;

pub async fn run(config: Config, db: Db, engine: PolicyEngine) -> Result<()> {
    // Spawn periodic GitHub sync task
    let sync_interval = config.github.sync_interval_minutes;
    if sync_interval > 0 {
        let sync_db = db.clone();
        let sync_engine = engine.clone();
        tokio::spawn(async move {
            github_sync_loop(sync_db, sync_engine, sync_interval).await;
        });
    }

    // Spawn periodic policy tick (fires OnTick every 60s)
    {
        let tick_engine = engine.clone();
        let tick_db = db.clone();
        tokio::spawn(async move {
            policy_tick_loop(tick_engine, tick_db).await;
        });
    }

    // Resolve dashboard dist path relative to runtime root or binary location
    let dashboard_dir = crate::cli::agentdesk_runtime_root()
        .map(|r| r.join("dashboard/dist"))
        .unwrap_or_else(|| std::path::PathBuf::from("dashboard/dist"));
    tracing::info!("Serving dashboard from {:?}", dashboard_dir);

    let broadcast_tx = ws::new_broadcast();

    let app = Router::new()
        .route("/ws", get(ws::ws_handler).with_state(broadcast_tx.clone()))
        .nest("/api", routes::api_router(db.clone(), engine.clone()))
        .fallback_service(ServeDir::new(&dashboard_dir));

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("HTTP server listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Background task that fires the OnTick policy hook at regular intervals.
async fn policy_tick_loop(engine: PolicyEngine, db: Db) {
    use std::time::Duration;

    let interval = Duration::from_secs(60);
    tracing::info!("[policy-tick] OnTick timer started (every 60s)");

    loop {
        tokio::time::sleep(interval).await;
        if let Err(e) = engine.fire_hook(crate::engine::hooks::Hook::OnTick, serde_json::json!({}))
        {
            tracing::warn!("[policy-tick] OnTick hook error: {e}");
            // Record failure
            if let Ok(conn) = db.lock() {
                conn.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('last_tick_status', 'error')",
                    [],
                )
                .ok();
            }
        } else {
            // Record success
            if let Ok(conn) = db.lock() {
                conn.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('last_tick_ms', ?1)",
                    [chrono::Utc::now().timestamp_millis().to_string()],
                )
                .ok();
                conn.execute(
                    "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('last_tick_status', 'ok')",
                    [],
                )
                .ok();
            }
        }
    }
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
            match crate::github::sync::sync_github_issues_for_repo(&db, &engine, &repo.id, &issues) {
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
