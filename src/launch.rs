use anyhow::{Context, Result};

pub(crate) fn run(state: crate::bootstrap::BootstrapState) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move { launch_server(state).await })
}

async fn launch_server(state: crate::bootstrap::BootstrapState) -> Result<()> {
    let crate::bootstrap::BootstrapState { config } = state;

    let pipeline_path = config.policies.dir.join("default-pipeline.yaml");
    if pipeline_path.exists() {
        crate::pipeline::load(&pipeline_path).context("Failed to load pipeline definition")?;
        tracing::info!("Pipeline loaded: {}", pipeline_path.display());
    }

    let pg_pool = crate::db::postgres::connect_and_migrate(&config)
        .await
        .map_err(anyhow::Error::msg)
        .context("Failed to init PostgreSQL")?;

    let legacy_db = if pg_pool.is_none() {
        crate::db::init(&config).context("Failed to init legacy SQLite DB")?
    } else {
        crate::db::unavailable("PostgreSQL mode disables the legacy SQLite runtime backend")
    };

    let engine = if let Some(pool) = pg_pool.clone() {
        crate::engine::PolicyEngine::new_with_pg(&config, Some(pool))
    } else {
        crate::engine::PolicyEngine::new_with_legacy_db(&config, legacy_db.clone())
    }
    .context("Failed to init policy engine")?;

    tracing::info!(
        "AgentDesk v{} starting on {}:{}",
        env!("CARGO_PKG_VERSION"),
        config.server.host,
        config.server.port
    );

    crate::server::run(config.clone(), legacy_db, engine, None)
        .await
        .context("Server error")?;

    Ok(())
}
