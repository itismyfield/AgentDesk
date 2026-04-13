use anyhow::{Context, Result};

pub(crate) struct BootstrapState {
    pub(crate) config: crate::config::Config,
    pub(crate) db: crate::db::Db,
}

pub(crate) fn initialize() -> Result<BootstrapState> {
    crate::logging::init_tracing()?;

    let runtime_root = crate::config::runtime_root();
    let legacy_scan = runtime_root
        .as_ref()
        .map(|root| crate::services::discord::config_audit::scan_legacy_sources(root))
        .unwrap_or_default();

    if let Some(root) = runtime_root.as_ref() {
        crate::runtime_layout::ensure_runtime_layout(root)
            .map_err(|error| anyhow::anyhow!("Failed to prepare runtime layout: {error}"))?;
    }

    let loaded = if let Some(root) = runtime_root.as_ref() {
        crate::services::discord::config_audit::load_runtime_config(root)
            .map_err(|error| anyhow::anyhow!("Failed to load config after layout prep: {error}"))?
    } else {
        let config = crate::config::load().context("Failed to load config")?;
        crate::services::discord::config_audit::LoadedRuntimeConfig {
            config,
            path: std::path::PathBuf::from("config/agentdesk.yaml"),
            existed: true,
        }
    };

    let db = crate::db::init(&loaded.config).context("Failed to init DB")?;
    crate::services::termination_audit::init_audit_db(db.clone());
    let config = if let Some(root) = runtime_root.as_ref() {
        crate::services::discord::config_audit::audit_and_reconcile(
            root,
            loaded.config,
            loaded.path,
            loaded.existed,
            &db,
            &legacy_scan,
            false,
        )
        .map_err(|error| anyhow::anyhow!("Failed to audit runtime config: {error}"))?
        .config
    } else {
        loaded.config
    };

    Ok(BootstrapState { config, db })
}
