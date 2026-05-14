use anyhow::{Context, Result};

pub(crate) struct BootstrapState {
    pub(crate) config: crate::config::Config,
}

pub(crate) fn initialize() -> Result<BootstrapState> {
    crate::logging::init_tracing()?;

    let runtime_root = crate::config::runtime_root();
    let legacy_scan = runtime_root
        .as_ref()
        .map(|root| crate::services::discord_config_audit::scan_legacy_sources(root))
        .unwrap_or_default();

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
            path: std::path::PathBuf::from("config/agentdesk.yaml"),
            existed: true,
        }
    };

    let config = if let Some(root) = runtime_root.as_ref() {
        crate::services::discord_config_audit::audit_and_reconcile_config_only(
            root,
            loaded.config,
            loaded.path,
            loaded.existed,
            &legacy_scan,
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
