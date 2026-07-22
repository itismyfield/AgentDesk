use super::*;

#[allow(clippy::too_many_arguments)]
pub(super) async fn run_bot_framework_setup(
    ctx: &serenity::Context,
    ready: &serenity::Ready,
    framework: &poise::Framework<Data, Error>,
    shared_for_migrate: Arc<SharedData>,
    shared_clone: Arc<SharedData>,
    health_registry_for_setup: Arc<health::HealthRegistry>,
    provider_for_setup: ProviderKind,
    token_for_ready: String,
    token_owned: String,
    voice_config_for_setup: crate::voice::VoiceConfig,
    voice_receiver_for_setup: crate::voice::VoiceReceiver,
    startup_reconcile_remaining: Arc<std::sync::atomic::AtomicUsize>,
    startup_doctor_started: Arc<std::sync::atomic::AtomicBool>,
    api_port: u16,
) -> Result<Data, Error> {
    // Register in each guild for instant slash command propagation
    // (register_globally can take up to 1 hour)
    let commands = &framework.options().commands;
    // Populate known slash command names for router fallback logic
    let cmd_names: std::collections::HashSet<String> =
        commands.iter().map(|c| c.name.clone()).collect();
    let _ = shared_for_migrate
        .known_slash_commands
        .set(cmd_names.clone());
    for guild in &ready.guilds {
        if let Err(e) = poise::builtins::register_in_guild(ctx, commands, guild.id).await {
            tracing::warn!(
                "  ⚠ Failed to register commands in guild {}: {}",
                guild.id,
                e
            );
        }
    }
    audit_or_prune_global_slash_commands(ctx, cmd_names.clone()).await;
    tracing::info!(
        "  ✓ Bot connected — Registered commands in {} guild(s)",
        ready.guilds.len()
    );
    shared_for_migrate
        .bot_connected
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = shared_for_migrate.http.cached_serenity_ctx.set(ctx.clone());
    let _ = shared_for_migrate
        .http
        .cached_bot_token
        .set(token_for_ready.clone());
    super::spawn_turn_completion_idle_queue_listener(
        shared_for_migrate.clone(),
        provider_for_setup.clone(),
    );
    super::drain_pending_queue_exit_placeholder_clears(&shared_for_migrate).await;
    health_registry_for_setup
        .register_http(provider_for_setup.as_str().to_string(), ctx.http.clone())
        .await;

    // Enrich role_map.json with channelId for reliable name→ID resolution
    enrich_role_map_with_channel_ids();

    let shared_for_tmux = shared_for_migrate.clone();

    // (Phase 5.1 of intake-node-routing — issue #2007: `run_bot()` starts the
    // intake worker only after the lease result authorizes a registered gateway
    // or standby runtime. No worker bootstrap belongs here.)

    // Background: hot-reload skills on file changes (30s polling)
    // Scans home-level AND all active project-level skill directories.
    super::spawns::run_bot_spawn_skills_hot_reload(&shared_for_tmux, &provider_for_setup);

    // #799: MCP credential watcher (Claude only).
    // Watches ~/.claude.json and ~/.claude/.mcp.json (the MCP server registries)
    // and posts a one-line notification to all active Claude sessions when the
    // registered server set changes, so the operator can run /restart to pick up
    // newly-authenticated MCP servers without losing context. The OAuth token file
    // (~/.claude/.credentials.json) is intentionally not watched (#3554) — its
    // refresh churn is not an MCP change.
    if matches!(provider_for_setup, ProviderKind::Claude) {
        let mcp_cfg = crate::config::load_graceful().mcp;
        if mcp_cfg.watch_credentials {
            let dedupe_window =
                std::time::Duration::from_secs(mcp_cfg.credential_notify_dedupe_secs);
            super::mcp_credential_watcher::spawn_watcher(
                shared_for_tmux.clone(),
                dedupe_window,
                "🔔 MCP credential 변화 감지됨. 새 MCP 적용하려면 `/restart`.",
            );
        } else {
            tracing::info!(
                "MCP credential watcher disabled via config (mcp.watch_credentials=false)"
            );
        }
    }

    // Restore inflight turns FIRST, then flush restart reports.
    // Recovery skips channels that have a pending restart report,
    // so the report must still be on disk when recovery runs.
    // After recovery completes, the flush loop starts and delivers/clears reports.
    super::recovery_flush::run_bot_spawn_recovery_and_flush_restart_reports(
        ctx,
        &shared_for_tmux,
        &token_owned,
        &provider_for_setup,
        &startup_reconcile_remaining,
        &startup_doctor_started,
        &health_registry_for_setup,
        api_port,
    );
    super::spawns::run_bot_spawn_periodic_catch_up(
        ctx.http.clone(),
        &shared_for_tmux,
        &provider_for_setup,
    );

    // Background: periodic cleanup for stale Discord upload files
    super::spawns::run_bot_spawn_upload_cleanup();

    // #1115 placeholder stall sweeper: safety net for placeholders
    // whose owning turn task is stuck or dead. Edits Discord
    // messages into stalled / abandoned states based on the
    // inflight state file mtime.
    //
    // #2438 (#2427 final): thresholds relaxed to safety-net
    // tone (stall 300s, abandon 1800s, initial 180s) after
    // the four explicit-signal wires landed.
    super::placeholder_sweeper::spawn_placeholder_sweeper(
        ctx.http.clone(),
        shared_clone.clone(),
        provider_for_setup.clone(),
    );

    // #3412/#4342 startup reclaim: independent bounded passes finalize prior-
    // generation frozen panels after the boot settle window and later delete
    // current-bot `...` placeholders left unlinked by a POST-before-persist
    // crash. Both passes reload durable protections and fail closed on identity.
    super::startup_reclaim::spawn_startup_reclaim_sweep(
        ctx.http.clone(),
        shared_clone.clone(),
        provider_for_setup.clone(),
        health_registry_for_setup.started_at_unix(),
    );

    // #3607: durable UI-only reconciliation for terminal-delivered turns whose
    // TUI quiescence confirmation timed out. The sidecar survives restart; the
    // sweeper picks up existing records on its first immediate tick.
    super::terminal_ui_obligation::spawn_terminal_ui_obligation_sweeper(
        ctx.http.clone(),
        shared_clone.clone(),
        provider_for_setup.clone(),
    );

    // #2436 (#2427 B wire): heartbeat-gap → explicit
    // inflight cleanup. Faster cadence than the placeholder
    // sweeper so a silently hung watcher loop is evicted
    // before the time-based safety net has to act.
    super::inflight_heartbeat_sweeper::spawn_heartbeat_sweeper(
        shared_clone.clone(),
        provider_for_setup.clone(),
    );

    // #1446 stall-deadlock recovery: complementary to the
    // placeholder sweeper — scans attached watchers for the
    // `attached=true && desynced=true && inflight stale` triad
    // and force-cleans the channel so THREAD-GUARD does not
    // queue the parent forever. Strictly more conservative
    // thresholds than the THREAD-GUARD's intake-time check.
    super::health::spawn_stall_watchdog(
        health_registry_for_setup.clone(),
        provider_for_setup.clone(),
    );

    // #1031 server-level idle detection (Option A — turn idle
    // heuristic). Periodically scans each provider's active
    // mailboxes and registers `system-detected:idle` monitoring
    // entries when watcher heartbeat (#982) has not advanced
    // within the configured threshold.
    super::idle_detector::spawn_idle_detector(shared_clone.clone(), provider_for_setup.clone());

    // Background: periodic reaper for dead tmux sessions that
    // still show as working in the DB (catches watcher gaps)
    #[cfg(unix)]
    super::spawns::run_bot_spawn_dead_tmux_reaper(&shared_clone);

    // Background: periodic GC for stale thread sessions in DB.
    // Normal idle/disconnected thread rows expire after 1 hour,
    // but rows still carrying an active_dispatch_id stay until the
    // 3-hour safety TTL so warm-resume sessions keep DB ownership.
    super::spawns::run_bot_spawn_stale_session_gc(&shared_clone);

    super::voice::run_bot_spawn_voice_auto_join(
        ctx,
        &voice_config_for_setup,
        &voice_receiver_for_setup,
        &shared_clone,
        &provider_for_setup,
    )
    .await;

    Ok(Data {
        shared: shared_clone,
        token: token_owned,
        provider: provider_for_setup,
        voice_config: voice_config_for_setup,
        voice_receiver: voice_receiver_for_setup,
    })
}

/// Build the full ordered list of poise slash commands registered by the bot.
/// Order is preserved exactly as it was inline in run_bot.
pub(super) fn run_bot_build_slash_commands() -> Vec<poise::Command<Data, Error>> {
    let mut slash_commands = vec![
        commands::cmd_start(),
        commands::cmd_pwd(),
        commands::cmd_status(),
        commands::cmd_inflight(),
        commands::cmd_clear(),
        commands::cmd_stop(),
        commands::cmd_cancel_queued(),
        commands::cmd_down(),
        commands::cmd_shell(),
        commands::cmd_skill(),
        commands::cmd_cc(),
        commands::cmd_metrics(),
        commands::cmd_model(),
        commands::cmd_node(),
        commands::cmd_sidecar(),
        commands::cmd_fast(),
        commands::cmd_goals(),
        commands::cmd_effort(),
        commands::cmd_compact(),
        commands::cmd_cost(),
        commands::cmd_context(),
        commands::cmd_adk(),
        commands::cmd_voice(),
        commands::cmd_vc_join(),
        commands::cmd_vc_leave(),
    ];
    slash_commands.extend([
        commands::cmd_queue(),
        commands::cmd_health(),
        commands::cmd_sessions(),
        commands::cmd_deletesession(),
        commands::cmd_allowedtools(),
        commands::cmd_allowed(),
        commands::cmd_debug(),
        commands::cmd_allowall(),
        commands::cmd_adduser(),
        commands::cmd_removeuser(),
        commands::cmd_usage(),
        commands::cmd_receipt(),
        commands::cmd_help(),
        commands::cmd_meeting(),
        commands::cmd_restart(),
        commands::cmd_deadlock_recover(),
        commands::cmd_machine_flip(),
        commands::cmd_stuck_pr_rebase(),
        commands::cmd_adk_phase(),
    ]);
    slash_commands
}

async fn audit_or_prune_global_slash_commands(
    ctx: &serenity::Context,
    current_guild_command_names: std::collections::HashSet<String>,
) {
    let globals = match serenity::Command::get_global_commands(ctx).await {
        Ok(commands) => commands,
        Err(error) => {
            tracing::warn!("failed to list global slash commands for pruning: {error}");
            return;
        }
    };

    if globals.is_empty() {
        return;
    }

    let prune_enabled = std::env::var("AGENTDESK_PRUNE_GLOBAL_SLASH_COMMANDS")
        .map(|value| matches!(value.trim(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);

    for command in globals {
        let command_name = command.name.clone();
        if current_guild_command_names.contains(&command_name) {
            tracing::info!(
                command = %command_name,
                command_id = command.id.get(),
                "global slash command duplicates a guild command"
            );
            continue;
        }
        if !prune_enabled {
            tracing::warn!(
                command = %command_name,
                command_id = command.id.get(),
                "stale global slash command detected; set AGENTDESK_PRUNE_GLOBAL_SLASH_COMMANDS=1 to delete"
            );
            continue;
        }
        if let Err(error) = serenity::Command::delete_global_command(ctx, command.id).await {
            tracing::warn!(
                command = %command_name,
                command_id = command.id.get(),
                error = %error,
                "failed to delete stale global slash command"
            );
        } else {
            tracing::info!(
                command = %command_name,
                command_id = command.id.get(),
                "deleted stale global slash command; guild commands remain authoritative"
            );
        }
    }
}

#[cfg(test)]
mod slash_command_registration_tests {
    use super::*;

    #[test]
    fn node_command_is_registered() {
        assert!(
            run_bot_build_slash_commands()
                .iter()
                .any(|command| command.name == "node"),
            "/node must be present in the slash command registration vec"
        );
    }
}
