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

    let pg_pool = crate::db::postgres::connect(&config)
        .await
        .map_err(anyhow::Error::msg)
        .context("Failed to init PostgreSQL")?;

    if let Some(pool) = pg_pool.as_ref() {
        crate::db::postgres::with_startup_advisory_lock(pool, || async {
            crate::db::postgres::migrate(pool).await?;
            if let Some(root) = crate::config::runtime_root().as_ref() {
                let legacy_scan = crate::services::discord_config_audit::scan_legacy_sources(root);
                let loaded = crate::services::discord_config_audit::load_runtime_config(root)
                    .map_err(|error| {
                        format!("Failed to reload config after PG migration: {error}")
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
                    format!("Failed to persist config audit after PG migration: {error}")
                })?
                .config;
            }
            crate::db::postgres::startup_reseed_with_warmup_pool(pool, &config).await
        })
        .await
        .map_err(anyhow::Error::msg)
        .context("Failed to init PostgreSQL")?;
    }
    if let Some(pool) = pg_pool.as_ref() {
        crate::services::agent_recovery::hydrate_from_pg(pool).await;
    }
    after_database_ready(&config, pg_pool.as_ref(), || {
        serve(&config, pg_pool.clone())
    })
    .await
}

async fn serve(config: &crate::config::Config, pg_pool: Option<sqlx::PgPool>) -> Result<()> {
    let engine = crate::engine::PolicyEngine::new_with_pg(config, pg_pool.clone())
        .context("Failed to init policy engine")?;

    tracing::info!(
        "AgentDesk v{} starting on {}:{}",
        env!("CARGO_PKG_VERSION"),
        config.server.host,
        config.server.port
    );

    crate::server::run(config.clone(), engine, None, pg_pool)
        .await
        .context("Server error")?;

    Ok(())
}

pub(crate) async fn after_database_ready<F, Fut>(
    config: &crate::config::Config,
    pool: Option<&sqlx::PgPool>,
    serve: F,
) -> Result<()>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    crate::bootstrap::prepare_and_install_boot_snapshots(config, pool).await?;
    serve().await
}
