use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use axum::Router;
use axum::routing::get;
use tower_http::services::ServeDir;

use crate::config::Config;
use crate::db::Db;
use crate::engine::PolicyEngine;
use crate::services::discord::health::HealthRegistry;

use super::worker_registry::SupervisedWorkerRegistry;
use super::{routes, ws};

pub(super) async fn serve_http(
    config: Config,
    db: Db,
    engine: PolicyEngine,
    health_registry: Option<Arc<HealthRegistry>>,
    worker_registry: &mut SupervisedWorkerRegistry,
) -> Result<()> {
    worker_registry.start_after_boot_reconcile()?;

    let dashboard_dir = resolve_dashboard_dir();
    provision_dashboard_dist_if_missing(&dashboard_dir);
    tracing::info!("Serving dashboard from {:?}", dashboard_dir);

    let broadcast_tx = ws::new_broadcast();
    let batch_buffer = worker_registry.start_after_websocket_broadcast(broadcast_tx.clone())?;

    seed_server_runtime_config(&db, &config);
    let pg_pool = crate::db::postgres::connect(&config)
        .await
        .map_err(anyhow::Error::msg)?;
    crate::services::termination_audit::init_audit_db(db.clone(), pg_pool.clone());

    let app = build_app(
        &dashboard_dir,
        db.clone(),
        engine.clone(),
        config.clone(),
        broadcast_tx,
        batch_buffer,
        health_registry,
        pg_pool,
    );

    let addr = format!("{}:{}", config.server.host, config.server.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("HTTP server listening on {addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

fn resolve_dashboard_dir() -> PathBuf {
    crate::cli::agentdesk_runtime_root()
        .map(|root| root.join("dashboard/dist"))
        .unwrap_or_else(|| PathBuf::from("dashboard/dist"))
}

fn provision_dashboard_dist_if_missing(dashboard_dir: &Path) {
    if dashboard_dir.join("index.html").exists() {
        return;
    }

    let workspace_dist = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("dashboard/dist");
    if !workspace_dist.join("index.html").exists() {
        tracing::warn!(
            "Dashboard dist not found at {:?} or {:?} — dashboard will be unavailable",
            dashboard_dir,
            workspace_dist
        );
        return;
    }

    tracing::info!(
        "Dashboard dist missing at {:?}, copying from workspace {:?}",
        dashboard_dir,
        workspace_dist
    );

    if let Some(parent) = dashboard_dir.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::remove_dir_all(dashboard_dir);

    match copy_dir_recursive(&workspace_dist, dashboard_dir) {
        Ok(count) => tracing::info!("Dashboard dist copied ({count} files)"),
        Err(error) => tracing::warn!("Failed to copy dashboard dist: {error}"),
    }
}

fn seed_server_runtime_config(db: &Db, config: &Config) {
    if let Ok(conn) = db.lock() {
        routes::settings::seed_config_defaults(&conn, config);
        conn.execute(
            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES ('server_port', ?1)",
            [config.server.port.to_string()],
        )
        .ok();
    }
}

fn build_app(
    dashboard_dir: &Path,
    db: Db,
    engine: PolicyEngine,
    config: Config,
    broadcast_tx: ws::BroadcastTx,
    batch_buffer: ws::BatchBuffer,
    health_registry: Option<Arc<HealthRegistry>>,
    pg_pool: Option<sqlx::PgPool>,
) -> Router {
    Router::new()
        .route("/ws", get(ws::ws_handler).with_state(broadcast_tx.clone()))
        .nest(
            "/api",
            routes::api_router_with_pg(
                db,
                engine,
                config,
                broadcast_tx,
                batch_buffer,
                health_registry,
                pg_pool,
            ),
        )
        .fallback_service(ServeDir::new(dashboard_dir).append_index_html_on_directories(true))
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<usize> {
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
