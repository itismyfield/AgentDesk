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

    let db = crate::db::init(&config).context("Failed to init DB")?;

    let pg_pool = crate::db::postgres::connect_and_migrate(&config)
        .await
        .map_err(anyhow::Error::msg)
        .context("Failed to init PostgreSQL")?;

    let engine = crate::engine::PolicyEngine::new_with_pg(&config, pg_pool)
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
