use anyhow::{Context, Result};

pub(crate) fn run(state: crate::bootstrap::BootstrapState) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move { launch_server(state).await })
}

async fn launch_server(state: crate::bootstrap::BootstrapState) -> Result<()> {
    let crate::bootstrap::BootstrapState { mut config } = state;

    let pipeline_path = config.policies.dir.join("default-pipeline.yaml");
    if pipeline_path.exists() {
        crate::pipeline::load(&pipeline_path).context("Failed to load pipeline definition")?;
        tracing::info!("Pipeline loaded: {}", pipeline_path.display());
    }

    // #1237 (843f): AppState.db is now Option<Db>; runtime can host PG-only
    // routes via state.pg_pool_ref(). Until #1238 (843g) ports the remaining
    // SQLite-backed handlers (update_card, /api/onboarding/*, etc.), launch
    // still initializes the compat SQLite handle and threads it through so
    // those handlers keep working in production.
    let db = crate::db::init(&config).context("Failed to init legacy compatibility DB")?;

    let pg_pool = crate::db::postgres::connect_and_migrate(&config)
        .await
        .map_err(anyhow::Error::msg)
        .context("Failed to init PostgreSQL")?;

    if let Some(root) = crate::config::runtime_root().as_ref() {
        let legacy_scan = crate::services::discord_config_audit::scan_legacy_sources(root);
        let loaded =
            crate::services::discord_config_audit::load_runtime_config(root).map_err(|error| {
                anyhow::anyhow!("Failed to reload config after PG migration: {error}")
            })?;
        config = crate::services::discord_config_audit::audit_and_reconcile_config_only(
            root,
            loaded.config,
            loaded.path,
            loaded.existed,
            &legacy_scan,
            false,
        )
        .map_err(|error| {
            anyhow::anyhow!("Failed to persist config audit after PG migration: {error}")
        })?
        .config;
    }

    let engine = crate::engine::PolicyEngine::new_with_pg(&config, pg_pool.clone())
        .context("Failed to init policy engine")?;

    tracing::info!(
        "AgentDesk v{} starting on {}:{}",
        env!("CARGO_PKG_VERSION"),
        config.server.host,
        config.server.port
    );

    crate::server::run(config.clone(), db, engine, None)
        .await
        .context("Server error")?;

    Ok(())
}
