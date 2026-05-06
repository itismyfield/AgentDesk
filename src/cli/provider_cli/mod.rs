use clap::{Args, Subcommand};
use serde_json::json;

use crate::services::provider_cli::io::{
    load_migration_state, load_registry, save_migration_state, save_registry, save_smoke_result,
};
use crate::services::provider_cli::orchestration::{
    apply_canary_override, canary_promotion_evidence, clear_canary_override,
    configured_provider_agents, evaluate_provider_session_guard, promote_registry_candidate,
    rollback_registry_previous, session_guard_evidence,
};
use crate::services::provider_cli::registry::{
    MigrationState, PROVIDER_UPDATE_STRATEGIES, ProviderCliChannel, ProviderCliMigrationState,
};
use crate::services::provider_cli::smoke::run_smoke;
use crate::services::provider_cli::snapshot::snapshot_current_channel;
use crate::services::provider_cli::upgrade::{
    migration_state_rank, new_migration_state, run_upgrade, transition,
};
use crate::services::provider_cli::{build_retention_set, cleanup_dry_run};

const SUPPORTED_PROVIDERS: &[&str] = &["codex", "claude", "gemini", "opencode", "qwen"];

#[derive(Args)]
pub struct ProviderCliArgs {
    #[command(subcommand)]
    pub action: ProviderCliAction,
}

#[derive(Subcommand)]
pub enum ProviderCliAction {
    /// Show current registry channels and migration states
    Status {
        /// Restrict output to a single provider (codex, claude, gemini, opencode, qwen)
        provider: Option<String>,
        /// Output raw JSON instead of the default table
        #[arg(long)]
        json: bool,
    },
    /// Show what a migration would do without running it
    Plan {
        /// Provider to plan (codex, claude, gemini, opencode, qwen)
        provider: String,
    },
    /// [step] Snapshot current binary and run upgrade
    ///
    /// Use `agentdesk provider-cli run` for the full automated migration.
    Upgrade {
        /// Provider to upgrade
        provider: String,
        /// Skip preserving the previous binary (for mutates_in_place providers)
        #[arg(long)]
        skip_previous_preservation: bool,
        /// Override candidate binary path after upgrade
        #[arg(long)]
        candidate_path: Option<String>,
    },
    /// [step] Run smoke checks against current or candidate binary
    ///
    /// Smoke is run automatically by `agentdesk provider-cli run`.
    Smoke {
        /// Provider to smoke-test
        provider: String,
        /// Channel to test: current or candidate
        #[arg(long, default_value = "current")]
        channel: String,
    },
    /// [step] Select a canary agent for the migration
    ///
    /// Canary selection is handled automatically by `agentdesk provider-cli run`.
    Canary {
        /// Provider to select canary for
        provider: String,
        /// Explicitly request this agent as the canary
        #[arg(long = "agent")]
        canary_agent: Option<String>,
    },
    /// Confirm promotion after canary passes (operator gate)
    Promote {
        /// Provider to promote
        provider: String,
        /// Operator note recorded in migration history
        #[arg(long)]
        evidence: Option<String>,
    },
    /// Roll back migration to previous state
    Rollback {
        /// Provider to roll back
        provider: String,
        /// Operator note recorded in migration history
        #[arg(long)]
        evidence: Option<String>,
    },
    /// List paths eligible for cleanup (dry-run only — deletion not implemented)
    Cleanup {
        /// Provider to inspect
        provider: String,
    },
    /// Run full migration orchestration up to CanaryActive
    Run {
        /// Provider to migrate
        provider: String,
        /// Override candidate path (skip upgrade step)
        #[arg(long)]
        candidate_path: Option<String>,
        /// Request a specific canary agent
        #[arg(long = "canary-agent")]
        canary_agent: Option<String>,
        /// Skip binary preservation (use with caution)
        #[arg(long)]
        skip_upgrade: bool,
        /// Skip operator confirmation gate after canary passes
        #[arg(long, alias = "auto-promote")]
        skip_confirm: bool,
    },
    /// Resume migration from the current persisted state
    Resume {
        /// Provider to resume
        provider: String,
        /// Skip operator confirmation gate after canary passes
        #[arg(long, alias = "auto-promote")]
        skip_confirm: bool,
    },
}

fn runtime_root() -> Result<std::path::PathBuf, String> {
    crate::config::runtime_root().ok_or_else(|| "AGENTDESK_ROOT_DIR not configured".to_string())
}

fn print_json(value: &serde_json::Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).unwrap_or_else(|_| "{}".to_string())
    );
}

fn normalize_provider_arg(provider: &str) -> Result<String, String> {
    let provider = provider.trim().to_ascii_lowercase();
    if SUPPORTED_PROVIDERS.contains(&provider.as_str()) {
        Ok(provider)
    } else {
        Err(format!("unsupported provider: {provider}"))
    }
}

pub fn cmd_provider_cli(args: ProviderCliArgs) -> Result<(), String> {
    match args.action {
        ProviderCliAction::Status { provider, json } => {
            let provider = provider
                .as_deref()
                .map(normalize_provider_arg)
                .transpose()?;
            // --json explicit flag, or non-TTY stdout (piped/scripted) → JSON output.
            let json_output = json || !std::io::IsTerminal::is_terminal(&std::io::stdout());
            cmd_status(provider.as_deref(), json_output)
        }
        ProviderCliAction::Plan { provider } => {
            let provider = normalize_provider_arg(&provider)?;
            cmd_plan(&provider)
        }
        ProviderCliAction::Upgrade {
            provider,
            skip_previous_preservation,
            candidate_path,
        } => {
            let provider = normalize_provider_arg(&provider)?;
            cmd_upgrade(
                &provider,
                skip_previous_preservation,
                candidate_path.as_deref(),
            )
        }
        ProviderCliAction::Smoke { provider, channel } => {
            let provider = normalize_provider_arg(&provider)?;
            cmd_smoke(&provider, &channel)
        }
        ProviderCliAction::Canary {
            provider,
            canary_agent,
        } => {
            let provider = normalize_provider_arg(&provider)?;
            cmd_canary(&provider, canary_agent.as_deref())
        }
        ProviderCliAction::Promote { provider, evidence } => {
            let provider = normalize_provider_arg(&provider)?;
            cmd_promote(&provider, evidence.as_deref())
        }
        ProviderCliAction::Rollback { provider, evidence } => {
            let provider = normalize_provider_arg(&provider)?;
            cmd_rollback(&provider, evidence.as_deref())
        }
        ProviderCliAction::Cleanup { provider } => {
            let provider = normalize_provider_arg(&provider)?;
            cmd_cleanup(&provider)
        }
        ProviderCliAction::Run {
            provider,
            candidate_path,
            canary_agent,
            skip_upgrade,
            skip_confirm,
        } => {
            let provider = normalize_provider_arg(&provider)?;
            cmd_run(
                &provider,
                candidate_path.as_deref(),
                canary_agent.as_deref(),
                skip_upgrade,
                skip_confirm,
            )
        }
        ProviderCliAction::Resume {
            provider,
            skip_confirm,
        } => {
            let provider = normalize_provider_arg(&provider)?;
            cmd_resume(&provider, skip_confirm)
        }
    }
}

fn cmd_status(provider: Option<&str>, json_output: bool) -> Result<(), String> {
    let root = runtime_root()?;
    let registry = load_registry(&root)
        .map_err(|e| format!("load registry: {e}"))?
        .unwrap_or_default();

    let filter: Vec<&str> = if let Some(p) = provider {
        vec![p]
    } else {
        SUPPORTED_PROVIDERS.to_vec()
    };

    if json_output {
        let mut output = Vec::new();
        for p in filter {
            let channels = registry.providers.get(p);
            let live_current = channels
                .and_then(|c| c.current.as_ref())
                .cloned()
                .or_else(|| snapshot_current_channel(p));
            let migration = load_migration_state(&root, p).ok().flatten();
            output.push(json!({
                "provider": p,
                "current": live_current.as_ref().map(|ch| ch.version.clone()),
                "candidate": channels.and_then(|c| c.candidate.as_ref()).map(|ch| &ch.version),
                "previous": channels.and_then(|c| c.previous.as_ref()).map(|ch| &ch.version),
                "migration_state": migration.as_ref().map(|m| format!("{:?}", m.state)),
                "canary_agent": migration.as_ref().and_then(|m| m.selected_agent_id.clone()),
            }));
        }
        print_json(&json!({ "providers": output }));
        return Ok(());
    }

    // Tabular output.
    let d = |v: Option<&str>| v.unwrap_or("-").to_string();
    let mut rows: Vec<[String; 5]> = Vec::new();
    for p in filter {
        let channels = registry.providers.get(p);
        let live_current = channels
            .and_then(|c| c.current.as_ref())
            .cloned()
            .or_else(|| snapshot_current_channel(p));
        let migration = load_migration_state(&root, p).ok().flatten();
        let state_str = migration.as_ref().map(|m| format!("{:?}", m.state));
        rows.push([
            p.to_string(),
            d(live_current.as_ref().map(|ch| ch.version.as_str())),
            d(channels
                .and_then(|c| c.candidate.as_ref())
                .map(|ch| ch.version.as_str())),
            d(state_str.as_deref()),
            d(migration
                .as_ref()
                .and_then(|m| m.selected_agent_id.as_deref())),
        ]);
    }

    const HEADERS: [&str; 5] = ["PROVIDER", "CURRENT", "CANDIDATE", "STATE", "CANARY AGENT"];
    let mut col_widths: [usize; 5] = HEADERS.map(|h| h.len());
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            col_widths[i] = col_widths[i].max(cell.len());
        }
    }

    let fmt_row = |cells: &[String]| -> String {
        cells
            .iter()
            .zip(col_widths.iter())
            .map(|(cell, &w)| format!("{cell:<w$}"))
            .collect::<Vec<_>>()
            .join("  ")
    };

    let header_strs: Vec<String> = HEADERS.iter().map(|s| s.to_string()).collect();
    println!("{}", fmt_row(&header_strs));
    println!(
        "{}",
        col_widths
            .iter()
            .map(|&w| "-".repeat(w))
            .collect::<Vec<_>>()
            .join("  ")
    );
    for row in &rows {
        println!("{}", fmt_row(row));
    }

    Ok(())
}

fn cmd_plan(provider: &str) -> Result<(), String> {
    let strategy = PROVIDER_UPDATE_STRATEGIES
        .iter()
        .find(|s| s.provider == provider)
        .ok_or_else(|| format!("no update strategy for provider: {provider}"))?;

    let snapshot = snapshot_current_channel(provider);

    print_json(&json!({
        "provider": provider,
        "strategy": {
            "install_source": strategy.install_source,
            "command": strategy.command_argv,
            "mutates_in_place": strategy.mutates_in_place,
            "allow_candidate_path_change": strategy.allow_candidate_path_change,
        },
        "current_version": snapshot.as_ref().map(|ch| &ch.version),
        "current_path": snapshot.as_ref().map(|ch| &ch.canonical_path),
        "plan": [
            "1. snapshot current binary",
            "2. preserve previous binary (mutates_in_place)",
            "3. run upgrade command",
            "4. smoke check candidate",
            "5. select canary agent",
            "6. start canary session",
            "7. await canary pass",
            "8. await operator confirm_promote",
            "9. recreate all provider sessions",
        ],
    }));
    Ok(())
}

fn cmd_upgrade(
    provider: &str,
    skip_previous_preservation: bool,
    candidate_path: Option<&str>,
) -> Result<(), String> {
    let root = runtime_root()?;

    let current = snapshot_current_channel(provider)
        .ok_or_else(|| format!("cannot resolve current binary for provider: {provider}"))?;

    eprintln!("Current {provider} version: {}", current.version);

    if let Some(path) = candidate_path {
        // Manual candidate override — skip running the upgrade command.
        let canonical = std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string());
        let candidate = crate::services::provider_cli::snapshot::snapshot_current_channel(provider)
            .map(|mut ch| {
                ch.path = path.to_string();
                ch.canonical_path = canonical.clone();
                ch.source = "manual_override".to_string();
                ch
            })
            .unwrap_or_else(
                || crate::services::provider_cli::registry::ProviderCliChannel {
                    path: path.to_string(),
                    canonical_path: canonical.clone(),
                    version: "unknown".to_string(),
                    version_output: None,
                    source: "manual_override".to_string(),
                    checked_at: chrono::Utc::now(),
                    evidence: Default::default(),
                },
            );

        let mut state = load_migration_state(&root, provider)
            .map_err(|e| e.to_string())?
            .unwrap_or_else(|| new_migration_state(provider, current.clone()));
        state.candidate_channel = Some(candidate.clone());
        save_migration_state(&root, &state).map_err(|e| e.to_string())?;

        let candidate_path = candidate.canonical_path.clone();
        persist_upgrade_candidate(&root, provider, &current, candidate)?;

        print_json(&json!({
            "provider": provider,
            "candidate_path": candidate_path,
            "note": "manual candidate override — upgrade command was skipped",
        }));
        return Ok(());
    }

    let prev_path = root
        .join("runtime")
        .join(format!("{provider}-previous-binary"));
    let result = run_upgrade(
        provider,
        &current,
        Some(prev_path.as_path()),
        skip_previous_preservation,
    )
    .map_err(|e| format!("upgrade failed: {e}"))?;

    eprintln!(
        "Upgraded {provider}: {} -> {}",
        result.pre_version, result.post_version
    );

    // Persist candidate in migration state.
    let mut state = load_migration_state(&root, provider)
        .map_err(|e| e.to_string())?
        .unwrap_or_else(|| new_migration_state(provider, current.clone()));
    state.candidate_channel = Some(result.candidate_channel.clone());
    save_migration_state(&root, &state).map_err(|e| e.to_string())?;

    persist_upgrade_candidate(&root, provider, &current, result.candidate_channel.clone())?;

    print_json(&json!({
        "provider": provider,
        "pre_version": result.pre_version,
        "post_version": result.post_version,
        "candidate_path": result.candidate_channel.canonical_path,
    }));
    Ok(())
}

fn persist_upgrade_candidate(
    root: &std::path::Path,
    provider: &str,
    current: &ProviderCliChannel,
    candidate: ProviderCliChannel,
) -> Result<(), String> {
    let mut registry = load_registry(root)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    let channels = registry.providers.entry(provider.to_string()).or_default();
    channels.current.get_or_insert_with(|| current.clone());
    channels.candidate = Some(candidate);
    save_registry(root, &registry).map_err(|e| e.to_string())?;
    Ok(())
}

fn cmd_smoke(provider: &str, channel: &str) -> Result<(), String> {
    let root = runtime_root()?;
    let registry = load_registry(&root)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();

    let ch_info = registry
        .providers
        .get(provider)
        .and_then(|channels| match channel {
            "current" => channels.current.as_ref(),
            "candidate" => channels.candidate.as_ref(),
            "previous" => channels.previous.as_ref(),
            _ => None,
        })
        .ok_or_else(|| format!("no {channel} channel for provider: {provider}"))?;

    let result = run_smoke(provider, channel, &ch_info.path, &ch_info.canonical_path);
    let passed = crate::services::provider_cli::smoke::smoke_passed(&result);

    save_smoke_result(&root, &result).map_err(|e| e.to_string())?;

    print_json(&json!({
        "provider": provider,
        "channel": channel,
        "overall_status": result.overall_status,
        "passed": passed,
        "version_check": format!("{:?}", result.checks.version),
    }));

    if !passed {
        return Err(format!("smoke check failed for {provider}/{channel}"));
    }
    Ok(())
}

fn cmd_canary(provider: &str, canary_agent: Option<&str>) -> Result<(), String> {
    let root = runtime_root()?;

    let mut state = load_migration_state(&root, provider)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no migration state for provider: {provider}"))?;

    if !canary_override_allowed(&state.state) {
        return Err(format!(
            "canary override requires smoke_candidate_passed or canary_selected state; current state is {:?}",
            state.state
        ));
    }

    // In a CLI context we don't have live session info; set the agent id directly.
    let agent_id = canary_agent
        .filter(|agent| !agent.trim().is_empty())
        .map(str::to_string)
        .or_else(|| {
            state
                .selected_agent_id
                .as_deref()
                .filter(|agent| !agent.trim().is_empty())
                .map(str::to_string)
        })
        .ok_or_else(|| "no canary agent specified (use --agent <agent-id>)".to_string())?;

    state.selected_agent_id = Some(agent_id.clone());
    if state.state == MigrationState::SmokeCandidatePassed {
        transition(&mut state, MigrationState::CanarySelected, None)
            .map_err(|e| format!("transition error: {e}"))?;
    }
    save_migration_state(&root, &state).map_err(|e| e.to_string())?;
    apply_canary_override(&root, provider, &agent_id)?;

    print_json(&json!({
        "provider": provider,
        "canary_agent_id": agent_id,
    }));
    Ok(())
}

fn canary_override_allowed(state: &MigrationState) -> bool {
    matches!(
        state,
        MigrationState::SmokeCandidatePassed | MigrationState::CanarySelected
    )
}

fn cmd_promote(provider: &str, evidence: Option<&str>) -> Result<(), String> {
    let root = runtime_root()?;
    let mut state = load_migration_state(&root, provider)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no migration state for provider: {provider}"))?;

    if state.state == MigrationState::ProviderAgentsMigrated {
        print_json(&json!({
            "provider": provider,
            "state": format!("{:?}", state.state),
        }));
        return Ok(());
    }

    if state.state == MigrationState::CanaryActive {
        let canary_evidence = canary_promotion_evidence(&root, &state, evidence)?;
        advance_to(
            &mut state,
            MigrationState::CanaryPassed,
            Some(canary_evidence),
        )
        .map_err(|e| format!("transition error: {e}"))?;
        advance_to(
            &mut state,
            MigrationState::AwaitingOperatorPromote,
            evidence.map(str::to_string),
        )
        .map_err(|e| format!("transition error: {e}"))?;
        save_migration_state(&root, &state).map_err(|e| e.to_string())?;
    } else if !matches!(
        state.state,
        MigrationState::CanaryPassed | MigrationState::AwaitingOperatorPromote
    ) {
        return Err(format!(
            "promotion requires canary_active, canary_passed, or awaiting_operator_promote state; current state is {:?}",
            state.state
        ));
    } else if state.state == MigrationState::CanaryPassed {
        advance_to(
            &mut state,
            MigrationState::AwaitingOperatorPromote,
            evidence.map(str::to_string),
        )
        .map_err(|e| format!("transition error: {e}"))?;
        save_migration_state(&root, &state).map_err(|e| e.to_string())?;
    }

    let guard = evaluate_provider_session_guard(
        &root,
        provider,
        state.selected_agent_id.as_deref(),
        "candidate",
    );
    if !guard.is_clear() {
        let selected_agent_id = state.selected_agent_id.clone();
        transition(
            &mut state,
            MigrationState::Failed,
            Some(guard.evidence_json()),
        )
        .map_err(|e| format!("transition error: {e}"))?;
        let clear_result = selected_agent_id
            .as_deref()
            .map(|agent_id| clear_canary_override(&root, provider, agent_id))
            .transpose();
        save_migration_state(&root, &state).map_err(|e| e.to_string())?;
        if let Err(error) = clear_result {
            return Err(format!(
                "safe session guard blocked promotion: {}; additionally failed to clear canary override: {error}",
                guard.blockers.join("; ")
            ));
        }
        return Err(format!(
            "safe session guard blocked promotion: {}",
            guard.blockers.join("; ")
        ));
    }

    advance_to(
        &mut state,
        MigrationState::ProviderSessionsSafeEnding,
        Some(session_guard_evidence(evidence, &guard)),
    )
    .map_err(|e| format!("transition error: {e}"))?;
    advance_to(
        &mut state,
        MigrationState::ProviderSessionsRecreated,
        Some(guard.evidence_json()),
    )
    .map_err(|e| format!("transition error: {e}"))?;
    promote_registry_candidate(&root, provider)?;
    advance_to(&mut state, MigrationState::ProviderAgentsMigrated, None)
        .map_err(|e| format!("transition error: {e}"))?;
    save_migration_state(&root, &state).map_err(|e| e.to_string())?;

    print_json(&json!({
        "provider": provider,
        "state": format!("{:?}", state.state),
    }));
    Ok(())
}

fn cmd_rollback(provider: &str, evidence: Option<&str>) -> Result<(), String> {
    let root = runtime_root()?;
    let mut state = load_migration_state(&root, provider)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no migration state for provider: {provider}"))?;

    transition(
        &mut state,
        MigrationState::RolledBack,
        evidence.map(str::to_string),
    )
    .map_err(|e| format!("transition error: {e}"))?;

    // Restore previous binary to current slot in registry if rollback_target is set.
    if state.rollback_target.is_some() || state.current_channel.is_some() {
        rollback_registry_previous(&root, provider)?;
    }

    save_migration_state(&root, &state).map_err(|e| e.to_string())?;

    print_json(&json!({
        "provider": provider,
        "state": format!("{:?}", state.state),
    }));
    Ok(())
}

fn cmd_cleanup(provider: &str) -> Result<(), String> {
    let root = runtime_root()?;
    let registry = load_registry(&root)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();

    let migration_states: Vec<_> = SUPPORTED_PROVIDERS
        .iter()
        .filter_map(|p| load_migration_state(&root, p).ok().flatten())
        .collect();

    let retention = build_retention_set(&registry, &migration_states);

    let scan_dir = root.join("runtime").join("provider-cli-binaries");
    if !scan_dir.exists() {
        print_json(&json!({ "provider": provider, "scan_dir": scan_dir, "candidates": [] }));
        return Ok(());
    }

    let candidates =
        cleanup_dry_run(&scan_dir, &retention).map_err(|e| format!("cleanup scan: {e}"))?;

    print_json(&json!({
        "provider": provider,
        "scan_dir": scan_dir,
        "candidates": candidates,
        "note": "dry-run only — no files deleted",
    }));
    Ok(())
}

fn resolve_canary_agent_id(
    provider: &str,
    requested_agent: Option<&str>,
    existing_agent: Option<&str>,
) -> Result<String, String> {
    if let Some(agent) = requested_agent.filter(|agent| !agent.trim().is_empty()) {
        return Ok(agent.to_string());
    }
    if let Some(agent) = existing_agent.filter(|agent| !agent.trim().is_empty()) {
        return Ok(agent.to_string());
    }
    let agents = configured_provider_agents(provider);
    crate::services::provider_cli::select_canary_agent(provider, &agents, None).ok_or_else(|| {
        format!("no configured canary agent found for provider: {provider}; pass --canary-agent")
    })
}

fn preserved_previous_channel(
    current: &ProviderCliChannel,
    prev_path: &std::path::Path,
) -> ProviderCliChannel {
    let path = prev_path.to_string_lossy().to_string();
    let mut evidence = current.evidence.clone();
    evidence.insert("preserved_from".to_string(), current.canonical_path.clone());
    ProviderCliChannel {
        path: path.clone(),
        canonical_path: path,
        version: current.version.clone(),
        version_output: current.version_output.clone(),
        source: "previous_preserved".to_string(),
        checked_at: chrono::Utc::now(),
        evidence,
    }
}

fn should_reuse_migration_state(state: &ProviderCliMigrationState) -> bool {
    !matches!(
        state.state,
        MigrationState::Failed
            | MigrationState::RolledBack
            | MigrationState::ProviderAgentsMigrated
    )
}

/// Full migration orchestration: runs through the state machine up to
/// `CanaryActive`. Promotion verifies a recorded candidate canary launch when
/// present, or records a warning when no launch artifact exists.
fn cmd_run(
    provider: &str,
    candidate_path: Option<&str>,
    canary_agent: Option<&str>,
    skip_upgrade: bool,
    skip_confirm: bool,
) -> Result<(), String> {
    let root = runtime_root()?;

    // Step 1: snapshot current.
    let current = snapshot_current_channel(provider)
        .ok_or_else(|| format!("cannot resolve current binary for provider: {provider}"))?;
    eprintln!("[1/7] Current {provider} version: {}", current.version);

    // Initialize or reload migration state.
    let mut state = load_migration_state(&root, provider)
        .map_err(|e| e.to_string())?
        .filter(should_reuse_migration_state)
        .unwrap_or_else(|| new_migration_state(provider, current.clone()));

    advance_to(&mut state, MigrationState::CurrentSnapshotted, None)?;
    save_migration_state(&root, &state).map_err(|e| e.to_string())?;

    // Step 2: smoke current.
    let smoke_cur = run_smoke(provider, "current", &current.path, &current.canonical_path);
    let _ = save_smoke_result(&root, &smoke_cur);
    if !crate::services::provider_cli::smoke::smoke_passed(&smoke_cur) {
        return Err(format!("smoke check failed on current {provider} binary"));
    }
    advance_to(&mut state, MigrationState::SmokeCurrentPassed, None)?;
    save_migration_state(&root, &state).map_err(|e| e.to_string())?;
    eprintln!("[2/7] Smoke check on current binary passed");

    // Step 3: preserve previous + upgrade (unless skip_upgrade or candidate_path provided).
    let mut registry_current = current.clone();
    let mut registry_previous = current.clone();
    let candidate = if let Some(path) = candidate_path {
        advance_to(
            &mut state,
            MigrationState::PreviousPreserved,
            Some("skipped: candidate_path provided".to_string()),
        )?;
        advance_to(&mut state, MigrationState::UpgradePlanned, None)?;
        advance_to(&mut state, MigrationState::UpgradeSucceeded, None)?;
        let canonical = std::fs::canonicalize(path)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| path.to_string());
        snapshot_current_channel(provider)
            .map(|mut ch| {
                ch.path = path.to_string();
                ch.canonical_path = canonical.clone();
                ch.source = "manual_override".to_string();
                ch
            })
            .unwrap_or_else(
                || crate::services::provider_cli::registry::ProviderCliChannel {
                    path: path.to_string(),
                    canonical_path: canonical,
                    version: "unknown".to_string(),
                    version_output: None,
                    source: "manual_override".to_string(),
                    checked_at: chrono::Utc::now(),
                    evidence: Default::default(),
                },
            )
    } else if skip_upgrade {
        advance_to(
            &mut state,
            MigrationState::PreviousPreserved,
            Some("skipped: --skip-upgrade".to_string()),
        )?;
        advance_to(&mut state, MigrationState::UpgradePlanned, None)?;
        advance_to(&mut state, MigrationState::UpgradeSucceeded, None)?;
        current.clone()
    } else {
        advance_to(&mut state, MigrationState::PreviousPreserved, None)?;
        let prev_path = root
            .join("runtime")
            .join(format!("{provider}-previous-binary"));
        advance_to(&mut state, MigrationState::UpgradePlanned, None)?;
        let result = run_upgrade(provider, &current, Some(prev_path.as_path()), false)
            .map_err(|e| format!("upgrade: {e}"))?;
        eprintln!(
            "[3/7] Upgraded: {} -> {}",
            result.pre_version, result.post_version
        );
        advance_to(&mut state, MigrationState::UpgradeSucceeded, None)?;
        let preserved = preserved_previous_channel(&current, &prev_path);
        registry_current = preserved.clone();
        registry_previous = preserved;
        result.candidate_channel
    };

    state.candidate_channel = Some(candidate.clone());
    save_migration_state(&root, &state).map_err(|e| e.to_string())?;

    // Update registry.
    let mut registry = load_registry(&root)
        .map_err(|e| e.to_string())?
        .unwrap_or_default();
    {
        let channels = registry.providers.entry(provider.to_string()).or_default();
        channels.previous = Some(registry_previous);
        channels.current = Some(registry_current);
        channels.candidate = Some(candidate.clone());
    }
    save_registry(&root, &registry).map_err(|e| e.to_string())?;

    advance_to(&mut state, MigrationState::CandidateDiscovered, None)?;
    save_migration_state(&root, &state).map_err(|e| e.to_string())?;

    // Step 4: smoke candidate.
    let smoke_cand = run_smoke(
        provider,
        "candidate",
        &candidate.path,
        &candidate.canonical_path,
    );
    let _ = save_smoke_result(&root, &smoke_cand);
    if !crate::services::provider_cli::smoke::smoke_passed(&smoke_cand) {
        return Err(format!("smoke check failed on candidate {provider} binary"));
    }
    advance_to(&mut state, MigrationState::SmokeCandidatePassed, None)?;
    save_migration_state(&root, &state).map_err(|e| e.to_string())?;
    eprintln!("[4/7] Smoke check on candidate binary passed");

    // Step 5: canary selection and scoped candidate override.
    let selected_agent_id =
        resolve_canary_agent_id(provider, canary_agent, state.selected_agent_id.as_deref())?;
    state.selected_agent_id = Some(selected_agent_id.clone());
    apply_canary_override(&root, provider, &selected_agent_id)?;
    advance_to(&mut state, MigrationState::CanarySelected, None)?;
    save_migration_state(&root, &state).map_err(|e| e.to_string())?;
    eprintln!(
        "[5/7] Canary agent: {}",
        state.selected_agent_id.as_deref().unwrap_or("(none set)")
    );

    // Step 6: canary resolver lifecycle. Known old-channel launch artifacts
    // must be inactive, or the operator must explicitly force recreation.
    let canary_guard =
        crate::services::provider_cli::session_guard::evaluate_session_migration_guards(
            &root,
            provider,
            std::slice::from_ref(&selected_agent_id),
            "candidate",
        );
    if !canary_guard.is_clear() {
        transition(
            &mut state,
            MigrationState::Failed,
            Some(canary_guard.evidence_json()),
        )?;
        let clear_result = clear_canary_override(&root, provider, &selected_agent_id);
        save_migration_state(&root, &state).map_err(|e| e.to_string())?;
        if let Err(error) = clear_result {
            return Err(format!(
                "safe session guard blocked canary recreate: {}; additionally failed to clear canary override: {error}",
                canary_guard.blockers.join("; ")
            ));
        }
        return Err(format!(
            "safe session guard blocked canary recreate: {}",
            canary_guard.blockers.join("; ")
        ));
    }
    advance_to(
        &mut state,
        MigrationState::CanarySessionSafeEnding,
        Some(canary_guard.evidence_json()),
    )?;
    advance_to(
        &mut state,
        MigrationState::CanarySessionRecreated,
        Some(canary_guard.evidence_json()),
    )?;
    advance_to(&mut state, MigrationState::CanaryActive, None)?;
    save_migration_state(&root, &state).map_err(|e| e.to_string())?;
    eprintln!("[6/7] Canary override active — run one canary turn before promotion");

    // Step 7: auto-promote if requested.
    if skip_confirm {
        return Err(format!(
            "--skip-confirm requires a recorded candidate canary launch artifact; run a canary turn, then `agentdesk provider-cli resume {provider} --skip-confirm`"
        ));
    } else {
        eprintln!(
            "[7/7] Stopped at CanaryActive — run a canary turn, then `agentdesk provider-cli promote {provider} --evidence <note>`"
        );
    }

    print_json(&json!({
        "provider": provider,
        "state": format!("{:?}", state.state),
        "candidate_version": candidate.version,
    }));
    Ok(())
}

/// Resume migration from the current persisted state.
fn cmd_resume(provider: &str, skip_confirm: bool) -> Result<(), String> {
    let root = runtime_root()?;
    let state = load_migration_state(&root, provider)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("no migration state for provider: {provider}. Run `agentdesk provider-cli run {provider}` first."))?;

    eprintln!(
        "Resuming {provider} migration from state: {:?}",
        state.state
    );

    match state.state {
        MigrationState::CanaryActive => {
            if skip_confirm {
                cmd_promote(
                    provider,
                    Some("resumed with --skip-confirm after canary decision"),
                )
            } else {
                eprintln!(
                    "Migration is at CanaryActive. Run a canary turn, then use --skip-confirm or run `agentdesk provider-cli promote {provider} --evidence <note>`."
                );
                print_json(&json!({ "provider": provider, "state": "canary_active" }));
                Ok(())
            }
        }
        MigrationState::CanaryPassed => {
            if skip_confirm {
                cmd_promote(provider, Some("resumed with --skip-confirm"))
            } else {
                eprintln!(
                    "Migration is at CanaryPassed. Use --skip-confirm or run `agentdesk provider-cli promote {provider}`."
                );
                print_json(&json!({ "provider": provider, "state": "canary_passed" }));
                Ok(())
            }
        }
        MigrationState::AwaitingOperatorPromote => {
            if skip_confirm {
                cmd_promote(provider, Some("resumed with --skip-confirm"))
            } else {
                eprintln!(
                    "Migration is at AwaitingOperatorPromote. Use --skip-confirm or run `agentdesk provider-cli promote {provider}`."
                );
                print_json(&json!({ "provider": provider, "state": "awaiting_operator_promote" }));
                Ok(())
            }
        }
        MigrationState::ProviderAgentsMigrated => {
            eprintln!("Migration already complete.");
            print_json(&json!({ "provider": provider, "state": "provider_agents_migrated" }));
            Ok(())
        }
        MigrationState::RolledBack | MigrationState::Failed => Err(format!(
            "Migration is in terminal state {:?}; start a new migration with `agentdesk provider-cli run {provider}`.",
            state.state
        )),
        other => {
            let skip_upgrade = migration_state_rank(&other)
                .zip(migration_state_rank(&MigrationState::UpgradeSucceeded))
                .map(|(current_rank, upgrade_succeeded_rank)| {
                    current_rank >= upgrade_succeeded_rank
                })
                .unwrap_or(false);
            eprintln!(
                "State {:?} is mid-migration; re-running orchestration from scratch (skip_upgrade={}).",
                other, skip_upgrade
            );
            cmd_run(
                provider,
                None,
                state.selected_agent_id.as_deref(),
                skip_upgrade,
                skip_confirm,
            )
        }
    }
}

/// Advance `state` to `next` only if not already at or past it.
/// If already at `next`, this is a no-op (idempotent for recovery runs).
fn advance_to(
    state: &mut crate::services::provider_cli::registry::ProviderCliMigrationState,
    next: MigrationState,
    evidence: Option<String>,
) -> Result<(), String> {
    if state_is_at_or_past(&state.state, &next) {
        return Ok(());
    }
    transition(state, next, evidence).map_err(|e| format!("advance_to: {e}"))
}

fn state_is_at_or_past(current: &MigrationState, next: &MigrationState) -> bool {
    match (migration_state_rank(current), migration_state_rank(next)) {
        (Some(current_rank), Some(next_rank)) => current_rank >= next_rank,
        _ => current == next,
    }
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
mod tests {
    use super::*;
    use std::sync::{Mutex, MutexGuard};

    static ROOT_ENV_LOCK: Mutex<()> = Mutex::new(());

    struct RootEnvGuard {
        _guard: MutexGuard<'static, ()>,
    }

    impl RootEnvGuard {
        fn set(path: &std::path::Path) -> Self {
            let guard = ROOT_ENV_LOCK
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", path) };
            Self { _guard: guard }
        }
    }

    impl Drop for RootEnvGuard {
        fn drop(&mut self) {
            unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") };
        }
    }

    struct EnvVarGuard {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn set_path(name: &'static str, path: &std::path::Path) -> Self {
            let previous = std::env::var_os(name);
            unsafe { std::env::set_var(name, path.as_os_str()) };
            Self { name, previous }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(value) => unsafe { std::env::set_var(self.name, value) },
                None => unsafe { std::env::remove_var(self.name) },
            }
        }
    }

    fn test_channel(path: &str) -> crate::services::provider_cli::ProviderCliChannel {
        crate::services::provider_cli::ProviderCliChannel {
            path: path.to_string(),
            canonical_path: path.to_string(),
            version: "test-version".to_string(),
            version_output: None,
            source: "test".to_string(),
            checked_at: chrono::Utc::now(),
            evidence: Default::default(),
        }
    }

    #[cfg(unix)]
    fn write_executable(path: &std::path::Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::write(path, body).unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn plan_shows_strategy_for_known_provider() {
        // cmd_plan prints to stdout; just verify it doesn't error for a known provider.
        // Use a temp dir so runtime_root doesn't fail.
        let dir = tempfile::tempdir().unwrap();
        let _root = RootEnvGuard::set(dir.path());
        let result = cmd_plan("codex");
        assert!(result.is_ok());
    }

    #[test]
    fn plan_errors_for_unknown_provider() {
        let result = cmd_plan("__unknown_provider__");
        assert!(result.is_err());
    }

    #[test]
    fn cli_entrypoint_rejects_unsupported_provider_before_io() {
        let result = cmd_provider_cli(ProviderCliArgs {
            action: ProviderCliAction::Plan {
                provider: "../codex".to_string(),
            },
        });

        assert_eq!(result, Err("unsupported provider: ../codex".to_string()));
    }

    #[test]
    fn completed_or_terminal_migration_state_is_not_reused_by_run() {
        use crate::services::provider_cli::registry::{MigrationState, ProviderCliMigrationState};
        use chrono::Utc;

        let mut state = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::AwaitingOperatorPromote,
            selected_agent_id: None,
            current_channel: None,
            candidate_channel: None,
            rollback_target: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            history: vec![],
        };

        assert!(should_reuse_migration_state(&state));
        for terminal in [
            MigrationState::ProviderAgentsMigrated,
            MigrationState::RolledBack,
            MigrationState::Failed,
        ] {
            state.state = terminal;
            assert!(!should_reuse_migration_state(&state));
        }
    }

    #[test]
    fn status_empty_when_no_registry() {
        let dir = tempfile::tempdir().unwrap();
        let _root = RootEnvGuard::set(dir.path());
        let result = cmd_status(None, false);
        assert!(result.is_ok());
    }

    #[test]
    fn persist_upgrade_candidate_records_current_when_registry_empty() {
        let dir = tempfile::tempdir().unwrap();
        let current = test_channel("/tmp/current-codex");
        let candidate = test_channel("/tmp/candidate-codex");

        persist_upgrade_candidate(dir.path(), "codex", &current, candidate.clone()).unwrap();

        let registry = load_registry(dir.path()).unwrap().unwrap();
        let channels = registry.providers.get("codex").unwrap();
        assert_eq!(channels.current.as_ref(), Some(&current));
        assert_eq!(channels.candidate.as_ref(), Some(&candidate));
    }

    #[cfg(unix)]
    #[test]
    fn upgrade_manual_candidate_path_records_canonical_path() {
        let dir = tempfile::tempdir().unwrap();
        let _root = RootEnvGuard::set(dir.path());
        let bin_dir = dir.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let current = bin_dir.join("codex-current");
        let candidate_real = bin_dir.join("codex-candidate-real");
        let candidate_link = bin_dir.join("codex-candidate-link");
        write_executable(&current, "#!/bin/sh\necho current-version\n");
        write_executable(&candidate_real, "#!/bin/sh\necho candidate-version\n");
        std::os::unix::fs::symlink(&candidate_real, &candidate_link).unwrap();
        let _codex_path = EnvVarGuard::set_path("AGENTDESK_CODEX_PATH", &current);

        let result = cmd_upgrade("codex", false, Some(candidate_link.to_str().unwrap()));
        let state = load_migration_state(dir.path(), "codex").unwrap().unwrap();
        let registry = load_registry(dir.path()).unwrap().unwrap();
        let expected = std::fs::canonicalize(&candidate_link)
            .unwrap()
            .to_string_lossy()
            .to_string();

        assert!(result.is_ok());
        assert_eq!(
            state.candidate_channel.as_ref().unwrap().canonical_path,
            expected
        );
        assert_eq!(
            registry
                .providers
                .get("codex")
                .unwrap()
                .candidate
                .as_ref()
                .unwrap()
                .canonical_path,
            expected
        );
    }

    #[test]
    fn rollback_transitions_to_rolled_back() {
        let dir = tempfile::tempdir().unwrap();
        let _root = RootEnvGuard::set(dir.path());

        use crate::services::provider_cli::registry::{MigrationState, ProviderCliMigrationState};
        use chrono::Utc;
        let ms = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::AwaitingOperatorPromote,
            selected_agent_id: None,
            current_channel: None,
            candidate_channel: None,
            rollback_target: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            history: vec![],
        };
        save_migration_state(dir.path(), &ms).unwrap();

        let result = cmd_rollback("codex", Some("test rollback"));
        assert!(result.is_ok());
    }

    #[test]
    fn canary_rejects_non_canary_ready_state_without_registry_override() {
        let dir = tempfile::tempdir().unwrap();
        let _root = RootEnvGuard::set(dir.path());

        use crate::services::provider_cli::registry::{
            MigrationState, ProviderChannels, ProviderCliMigrationState, ProviderCliRegistry,
        };
        use chrono::Utc;
        let candidate = test_channel("/tmp/candidate-codex");
        let ms = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::Planned,
            selected_agent_id: None,
            current_channel: None,
            candidate_channel: Some(candidate.clone()),
            rollback_target: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            history: vec![],
        };
        save_migration_state(dir.path(), &ms).unwrap();
        let mut registry = ProviderCliRegistry::default();
        registry.providers.insert(
            "codex".to_string(),
            ProviderChannels {
                candidate: Some(candidate),
                ..Default::default()
            },
        );
        save_registry(dir.path(), &registry).unwrap();

        let result = cmd_canary("codex", Some("codex-agent"));
        let registry = load_registry(dir.path()).unwrap().unwrap();

        assert!(result.is_err());
        assert!(
            registry
                .providers
                .get("codex")
                .unwrap()
                .agent_overrides
                .is_empty()
        );
    }

    #[test]
    fn promote_transitions_to_provider_agents_migrated_and_clears_canary_override() {
        let dir = tempfile::tempdir().unwrap();
        let _root = RootEnvGuard::set(dir.path());

        use crate::services::provider_cli::registry::{
            LaunchArtifact, MigrationState, ProviderChannels, ProviderCliMigrationState,
            ProviderCliRegistry,
        };
        use chrono::Utc;
        let current = test_channel("/tmp/current-codex");
        let candidate = test_channel("/tmp/candidate-codex");
        let ms = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::AwaitingOperatorPromote,
            selected_agent_id: Some("codex-agent".to_string()),
            current_channel: Some(current.clone()),
            candidate_channel: Some(candidate.clone()),
            rollback_target: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            history: vec![],
        };
        save_migration_state(dir.path(), &ms).unwrap();
        let mut registry = ProviderCliRegistry::default();
        let mut channels = ProviderChannels {
            current: Some(current),
            candidate: Some(candidate.clone()),
            ..Default::default()
        };
        channels
            .agent_overrides
            .insert("codex-agent".to_string(), "candidate".to_string());
        registry.providers.insert("codex".to_string(), channels);
        save_registry(dir.path(), &registry).unwrap();
        crate::services::provider_cli::io::save_launch_artifact(
            dir.path(),
            &LaunchArtifact {
                provider: "codex".to_string(),
                agent_id: Some("codex-agent".to_string()),
                channel_id: Some("123".to_string()),
                session_key: Some("codex-agent-session".to_string()),
                channel: "candidate".to_string(),
                cli_path: candidate.path.clone(),
                canonical_path: candidate.canonical_path.clone(),
                cli_version: candidate.version.clone(),
                process_id: None,
                tmux_session: Some("agentdesk-codex-agent".to_string()),
                launched_at: Utc::now(),
            },
        )
        .unwrap();

        let result = cmd_promote("codex", Some("operator approval"));
        let state = load_migration_state(dir.path(), "codex").unwrap().unwrap();
        let registry = load_registry(dir.path()).unwrap().unwrap();
        assert!(result.is_ok());
        assert_eq!(state.state, MigrationState::ProviderAgentsMigrated);
        let channels = registry.providers.get("codex").unwrap();
        assert_eq!(channels.current.as_ref(), Some(&candidate));
        assert!(channels.agent_overrides.is_empty());
    }

    #[test]
    fn promote_guard_failure_clears_canary_override() {
        let dir = tempfile::tempdir().unwrap();
        let _root = RootEnvGuard::set(dir.path());

        use crate::services::provider_cli::registry::{
            LaunchArtifact, MigrationState, ProviderChannels, ProviderCliMigrationState,
            ProviderCliRegistry,
        };
        use chrono::Utc;
        let current = test_channel("/tmp/current-codex");
        let candidate = test_channel("/tmp/candidate-codex");
        let ms = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::AwaitingOperatorPromote,
            selected_agent_id: Some("codex-agent".to_string()),
            current_channel: Some(current.clone()),
            candidate_channel: Some(candidate.clone()),
            rollback_target: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            history: vec![],
        };
        save_migration_state(dir.path(), &ms).unwrap();
        let mut registry = ProviderCliRegistry::default();
        let mut channels = ProviderChannels {
            current: Some(current),
            candidate: Some(candidate),
            ..Default::default()
        };
        channels
            .agent_overrides
            .insert("codex-agent".to_string(), "candidate".to_string());
        registry.providers.insert("codex".to_string(), channels);
        save_registry(dir.path(), &registry).unwrap();

        // An active process on the old "current" channel is recorded in evidence but
        // no longer blocks promotion — the guard allows recreation unconditionally.
        crate::services::provider_cli::io::save_launch_artifact(
            dir.path(),
            &LaunchArtifact {
                provider: "codex".to_string(),
                agent_id: Some("codex-agent".to_string()),
                channel_id: None,
                session_key: Some("codex-agent-current-active".to_string()),
                channel: "current".to_string(),
                cli_path: "/tmp/current-codex".to_string(),
                canonical_path: "/tmp/current-codex".to_string(),
                cli_version: "test".to_string(),
                process_id: Some(std::process::id()),
                tmux_session: None,
                launched_at: Utc::now(),
            },
        )
        .unwrap();

        let result = cmd_promote("codex", Some("operator approval"));
        let state = load_migration_state(dir.path(), "codex").unwrap().unwrap();
        let registry = load_registry(dir.path()).unwrap().unwrap();

        // Promote succeeds even with an active old-channel session.
        assert!(result.is_ok());
        assert_eq!(state.state, MigrationState::ProviderAgentsMigrated);
        assert!(
            registry
                .providers
                .get("codex")
                .unwrap()
                .agent_overrides
                .is_empty()
        );
    }

    #[test]
    fn promote_from_canary_active_warns_when_candidate_launch_missing() {
        let dir = tempfile::tempdir().unwrap();
        let _root = RootEnvGuard::set(dir.path());

        use crate::services::provider_cli::registry::{
            LaunchArtifact, MigrationState, ProviderChannels, ProviderCliMigrationState,
            ProviderCliRegistry,
        };
        use chrono::{Duration, Utc};
        let current = test_channel("/tmp/current-codex");
        let candidate = test_channel("/tmp/candidate-codex");
        let ms = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::CanaryActive,
            selected_agent_id: Some("codex-agent".to_string()),
            current_channel: Some(current.clone()),
            candidate_channel: Some(candidate.clone()),
            rollback_target: None,
            started_at: Utc::now() - Duration::seconds(1),
            updated_at: Utc::now(),
            history: vec![],
        };
        save_migration_state(dir.path(), &ms).unwrap();
        let mut registry = ProviderCliRegistry::default();
        registry.providers.insert(
            "codex".to_string(),
            ProviderChannels {
                current: Some(current.clone()),
                candidate: Some(candidate.clone()),
                ..Default::default()
            },
        );
        save_registry(dir.path(), &registry).unwrap();

        // A missing post-activation candidate launch now records warning evidence but does not
        // block promotion; this covers quota-exhausted canary turns.
        crate::services::provider_cli::io::save_launch_artifact(
            dir.path(),
            &LaunchArtifact {
                provider: "codex".to_string(),
                agent_id: Some("codex-agent".to_string()),
                channel_id: None,
                session_key: Some("codex-agent-current-prev".to_string()),
                channel: "current".to_string(),
                cli_path: "/tmp/current-codex".to_string(),
                canonical_path: "/tmp/current-codex".to_string(),
                cli_version: "test".to_string(),
                process_id: None,
                tmux_session: None,
                launched_at: Utc::now() - Duration::seconds(120),
            },
        )
        .unwrap();

        let result = cmd_promote("codex", Some("operator approval"));
        let state = load_migration_state(dir.path(), "codex").unwrap().unwrap();
        let registry = load_registry(dir.path()).unwrap().unwrap();

        assert!(result.is_ok());
        assert_eq!(state.state, MigrationState::ProviderAgentsMigrated);
        let channels = registry.providers.get("codex").unwrap();
        assert_eq!(channels.current.as_ref(), Some(&candidate));
    }

    #[test]
    fn promote_from_canary_active_accepts_verified_candidate_launch() {
        let dir = tempfile::tempdir().unwrap();
        let _root = RootEnvGuard::set(dir.path());

        use crate::services::provider_cli::registry::{
            LaunchArtifact, MigrationState, ProviderChannels, ProviderCliMigrationState,
            ProviderCliRegistry,
        };
        use chrono::{Duration, Utc};
        let current = test_channel("/tmp/current-codex");
        let candidate = test_channel("/tmp/candidate-codex");
        let ms = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::CanaryActive,
            selected_agent_id: Some("codex-agent".to_string()),
            current_channel: Some(current.clone()),
            candidate_channel: Some(candidate.clone()),
            rollback_target: None,
            started_at: Utc::now() - Duration::seconds(2),
            updated_at: Utc::now() - Duration::seconds(1),
            history: vec![],
        };
        save_migration_state(dir.path(), &ms).unwrap();
        let mut registry = ProviderCliRegistry::default();
        let mut channels = ProviderChannels {
            current: Some(current),
            candidate: Some(candidate.clone()),
            ..Default::default()
        };
        channels
            .agent_overrides
            .insert("codex-agent".to_string(), "candidate".to_string());
        registry.providers.insert("codex".to_string(), channels);
        save_registry(dir.path(), &registry).unwrap();
        crate::services::provider_cli::io::save_launch_artifact(
            dir.path(),
            &LaunchArtifact {
                provider: "codex".to_string(),
                agent_id: Some("codex-agent".to_string()),
                channel_id: Some("123".to_string()),
                session_key: Some("codex-agent-session".to_string()),
                channel: "candidate".to_string(),
                cli_path: candidate.path.clone(),
                canonical_path: candidate.canonical_path.clone(),
                cli_version: candidate.version.clone(),
                process_id: None,
                tmux_session: Some("agentdesk-codex-agent".to_string()),
                launched_at: Utc::now(),
            },
        )
        .unwrap();

        let result = cmd_promote("codex", Some("operator approval"));
        let state = load_migration_state(dir.path(), "codex").unwrap().unwrap();
        let registry = load_registry(dir.path()).unwrap().unwrap();

        assert!(result.is_ok());
        assert_eq!(state.state, MigrationState::ProviderAgentsMigrated);
        let channels = registry.providers.get("codex").unwrap();
        assert_eq!(channels.current.as_ref(), Some(&candidate));
        assert!(channels.agent_overrides.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn promote_retry_after_terminal_state_save_failure_preserves_previous() {
        let dir = tempfile::tempdir().unwrap();
        let _root = RootEnvGuard::set(dir.path());

        use crate::services::provider_cli::paths::migration_state_path;
        use crate::services::provider_cli::registry::{
            MigrationState, ProviderChannels, ProviderCliMigrationState, ProviderCliRegistry,
        };
        use chrono::Utc;
        use std::os::unix::fs::PermissionsExt;

        let current = test_channel("/tmp/current-codex");
        let candidate = test_channel("/tmp/candidate-codex");
        let ms = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::AwaitingOperatorPromote,
            selected_agent_id: None,
            current_channel: Some(current.clone()),
            candidate_channel: Some(candidate.clone()),
            rollback_target: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            history: vec![],
        };
        save_migration_state(dir.path(), &ms).unwrap();
        let mut registry = ProviderCliRegistry::default();
        registry.providers.insert(
            "codex".to_string(),
            ProviderChannels {
                current: Some(current.clone()),
                candidate: Some(candidate.clone()),
                ..Default::default()
            },
        );
        save_registry(dir.path(), &registry).unwrap();

        let state_path = migration_state_path(dir.path(), "codex");
        let mut read_only = std::fs::metadata(&state_path).unwrap().permissions();
        read_only.set_mode(0o444);
        std::fs::set_permissions(&state_path, read_only).unwrap();

        let result = cmd_promote("codex", Some("operator approval"));

        let mut writable = std::fs::metadata(&state_path).unwrap().permissions();
        writable.set_mode(0o644);
        std::fs::set_permissions(&state_path, writable).unwrap();
        let state_after_failure = load_migration_state(dir.path(), "codex").unwrap().unwrap();
        let registry = load_registry(dir.path()).unwrap().unwrap();

        assert!(result.is_err());
        let channels = registry.providers.get("codex").unwrap();
        assert_eq!(
            state_after_failure.state,
            MigrationState::AwaitingOperatorPromote
        );
        assert_eq!(channels.previous.as_ref(), Some(&current));
        assert_eq!(channels.current.as_ref(), Some(&candidate));

        let retry = cmd_promote("codex", Some("retry after state save failure"));
        let state = load_migration_state(dir.path(), "codex").unwrap().unwrap();
        let registry = load_registry(dir.path()).unwrap().unwrap();
        let channels = registry.providers.get("codex").unwrap();

        assert!(retry.is_ok());
        assert_eq!(state.state, MigrationState::ProviderAgentsMigrated);
        assert_eq!(channels.previous.as_ref(), Some(&current));
        assert_eq!(channels.current.as_ref(), Some(&candidate));
    }

    #[cfg(unix)]
    #[test]
    fn promote_registry_failure_does_not_persist_terminal_state() {
        let dir = tempfile::tempdir().unwrap();
        let _root = RootEnvGuard::set(dir.path());

        use crate::services::provider_cli::paths::registry_path;
        use crate::services::provider_cli::registry::{
            MigrationState, ProviderChannels, ProviderCliMigrationState, ProviderCliRegistry,
        };
        use chrono::Utc;
        use std::os::unix::fs::PermissionsExt;

        let current = test_channel("/tmp/current-codex");
        let candidate = test_channel("/tmp/candidate-codex");
        let ms = ProviderCliMigrationState {
            schema_version: 1,
            provider: "codex".to_string(),
            state: MigrationState::AwaitingOperatorPromote,
            selected_agent_id: None,
            current_channel: Some(current.clone()),
            candidate_channel: Some(candidate.clone()),
            rollback_target: None,
            started_at: Utc::now(),
            updated_at: Utc::now(),
            history: vec![],
        };
        save_migration_state(dir.path(), &ms).unwrap();
        let mut registry = ProviderCliRegistry::default();
        registry.providers.insert(
            "codex".to_string(),
            ProviderChannels {
                current: Some(current.clone()),
                candidate: Some(candidate),
                ..Default::default()
            },
        );
        save_registry(dir.path(), &registry).unwrap();

        let registry_path = registry_path(dir.path());
        let mut read_only = std::fs::metadata(&registry_path).unwrap().permissions();
        read_only.set_mode(0o444);
        std::fs::set_permissions(&registry_path, read_only).unwrap();

        let result = cmd_promote("codex", Some("operator approval"));

        let mut writable = std::fs::metadata(&registry_path).unwrap().permissions();
        writable.set_mode(0o644);
        std::fs::set_permissions(&registry_path, writable).unwrap();
        let state = load_migration_state(dir.path(), "codex").unwrap().unwrap();
        let registry = load_registry(dir.path()).unwrap().unwrap();

        assert!(result.is_err());
        assert_eq!(state.state, MigrationState::AwaitingOperatorPromote);
        let channels = registry.providers.get("codex").unwrap();
        assert_eq!(channels.current.as_ref(), Some(&current));
    }
}
