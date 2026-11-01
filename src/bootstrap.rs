use anyhow::{Context, Result};

pub(crate) struct BootstrapState {
    pub(crate) config: crate::config::Config,
}

pub(crate) fn initialize() -> Result<BootstrapState> {
    crate::logging::init_tracing()?;

    let runtime_root = crate::config::runtime_root();
    crate::services::legacy_body_census::start();
    if let Some(root) = runtime_root.as_ref() {
        crate::runtime_layout::ensure_runtime_layout(root)
            .map_err(|error| anyhow::anyhow!("Failed to prepare runtime layout: {error}"))?;
    }

    let loaded = if let Some(root) = runtime_root.as_ref() {
        crate::services::discord_config_audit::load_runtime_config(root)
            .map_err(|error| anyhow::anyhow!("Failed to load config after layout prep: {error}"))?
    } else {
        let config = crate::config::load().context("Failed to load config")?;
        crate::services::discord_config_audit::LoadedRuntimeConfig {
            config,
            path: crate::config::resolved_config_path(),
            existed: true,
        }
    };

    let config = if let Some(root) = runtime_root.as_ref() {
        crate::services::discord_config_audit::audit_and_reconcile_config_only(
            root,
            loaded.config,
            loaded.path,
            loaded.existed,
            false,
        )
        .map_err(|error| anyhow::anyhow!("Failed to audit runtime config: {error}"))?
        .config
    } else {
        loaded.config
    };

    if let Err(error) = crate::services::mcp_config::sync_codex_mcp_servers(&config) {
        tracing::warn!("  [mcp] Failed to sync Codex MCP servers: {error}");
    }
    if let Err(error) = crate::services::mcp_config::sync_opencode_mcp_servers(&config) {
        tracing::warn!("  [mcp] Failed to sync OpenCode MCP servers: {error}");
    }
    if let Err(error) = crate::services::mcp_config::sync_qwen_mcp_servers(&config) {
        tracing::warn!("  [mcp] Failed to sync Qwen MCP servers: {error}");
    }
    if let Err(error) = crate::services::mcp_config::sync_gemini_mcp_servers(&config) {
        tracing::warn!("  [mcp] Failed to sync Gemini MCP servers: {error}");
    }

    // #1699: install the prompt-manifest retention snapshot so write-time
    // truncation in `db::prompt_manifests::save_prompt_manifest_pg` reflects
    // the operator-chosen `agentdesk.yaml` policy. Set-once; restart required
    // to change retention bounds.
    crate::db::prompt_manifests::install_retention_config(config.prompt_manifest_retention.clone());
    crate::services::provider_hosting::install_provider_hosting_config(&config);

    Ok(BootstrapState { config })
}

/// Installs the process-lifetime snapshots, then ensures the installed dedicated Herdr configs.
/// Static config errors abort boot; migration refusal is carried by the selected hosts.
pub(crate) fn install_boot_snapshots(
    config: &crate::config::Config,
    selection: crate::config::session_hosts::migration::HostsBootSelection,
) -> Result<()> {
    crate::config::validate_config(config)?;
    crate::services::tui_o::channel_policy::install(config)?;
    crate::services::provider_hosting::install_provider_hosting_config(config);
    crate::config::session_hosts::install_selected(selection);
    #[cfg(unix)]
    crate::services::session_host::managed_config::maintenance::boot();
    crate::config_live_reload::install(config.clone());
    Ok(())
}

pub(crate) async fn prepare_session_hosts_boot(
    candidate: crate::config::session_hosts::BootSessionHosts,
    pool: Option<&sqlx::PgPool>,
) -> crate::config::session_hosts::migration::HostsBootSelection {
    use crate::config::session_hosts::{approval_store::FileApprovalStore, migration};
    migration::select(
        candidate,
        &FileApprovalStore::runtime(),
        &migration::PgRows(pool),
        &migration::ConsentHolds,
        false,
    )
    .await
}

pub(crate) async fn prepare_and_install_boot_snapshots(
    config: &crate::config::Config,
    pool: Option<&sqlx::PgPool>,
) -> Result<()> {
    crate::services::legacy_body_census::ensure_ready().await;
    crate::config::validate_config(config)?;
    let candidate = crate::config::session_hosts::BootSessionHosts::from_config(config)?;
    let selection = prepare_session_hosts_boot(candidate, pool).await;
    crate::services::session_host::policy::boot_grants::prepare(selection.hosts()).await;
    install_boot_snapshots(config, selection)
}

/// The dcserver preserves its static-validation failure contract before starting HTTP or intake.
pub(crate) async fn prepare_dcserver_boot(
    config: &crate::config::Config,
    pool: &sqlx::PgPool,
) -> Option<crate::config_live_reload::ConfigHotReloadGuard> {
    if let Err(error) = prepare_and_install_boot_snapshots(config, Some(pool)).await {
        eprintln!("  ✖ Failed to install boot snapshots: {error:#}");
        std::process::exit(1);
    }
    crate::cli::dcserver::startup::spawn_skill_manifest_audit(config, pool);
    crate::config_live_reload::start(
        crate::config::resolved_config_path(),
        config.config_hot_reload,
    )
}

#[cfg(all(test, unix))]
#[path = "config/session_hosts/boot_pg_tests.rs"]
mod tests;
