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

    // Issue #2193 — Codex remote SSH runtime gate.
    //
    // The ADR (`docs/codex-remote-ssh-policy.md`) lands the gate now and
    // pins it to a compile-time prerequisites constant. Until every
    // ADR follow-up (real `services::remote`, `providers.codex.remote_hosts`
    // allow-list, PTY-bound process-group cancel, hardened SSH
    // invocation, cancel integration test) lands, that constant stays
    // `false`. Flipping `providers.codex.remote_ssh_enabled: true` in
    // `agentdesk.yaml` is a hard bootstrap error — not a warning —
    // because a warn-only gate becomes a persisted "enabled" signal
    // that a partial future implementation could silently honor.
    if config.codex_remote_ssh_enabled()
        && !crate::services::codex_remote_policy::PREREQUISITES_SATISFIED
    {
        return Err(anyhow::anyhow!(
            "providers.codex.remote_ssh_enabled is true, but the prerequisites in \
             docs/codex-remote-ssh-policy.md are not satisfied yet \
             (services::remote SSH implementation, providers.codex.remote_hosts \
             allow-list, hardened SSH invocation, process-group cancel, and the \
             cancel integration test). Set providers.codex.remote_ssh_enabled \
             back to false until the ADR follow-ups land (#2193)."
        ));
    }

    Ok(BootstrapState { config })
}
