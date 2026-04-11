use super::*;

pub(crate) struct RunBotContext {
    pub(crate) global_active: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) global_finalizing: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) shutdown_remaining: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) health_registry: Arc<health::HealthRegistry>,
    pub(crate) api_port: u16,
    pub(crate) db: Option<crate::db::Db>,
    pub(crate) engine: Option<crate::engine::PolicyEngine>,
}

fn spawn_startup_thread_map_validation(db: crate::db::Db, token: String) {
    tokio::spawn(async move {
        let (checked, cleared) =
            crate::server::routes::dispatches::validate_channel_thread_maps_on_startup(&db, &token)
                .await;
        if checked > 0 || cleared > 0 {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 🧹 THREAD-MAP: validated {checked} mapping(s), cleared {cleared} stale binding(s)"
            );
        }
    });
}

/// Execute durable handoff turns saved before a restart.
/// Runs after tmux watcher restore and pending queue restore, but before
/// restart report flush. Skips channels that already have pending queue messages
/// (user intent takes priority over automatic follow-up).
async fn execute_handoff_turns(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let handoffs = load_handoffs(provider);
    if handoffs.is_empty() {
        return;
    }
    let settings_snapshot = shared.settings.read().await.clone();
    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] 📎 Found {} handoff record(s) to process",
        handoffs.len()
    );

    let current_gen = runtime_store::load_generation();

    for record in handoffs {
        let channel_id = ChannelId::new(record.channel_id);
        let ts = chrono::Local::now().format("%H:%M:%S");

        // Skip if from a different generation (stale)
        if record.born_generation > current_gen {
            println!(
                "  [{ts}] ⏭ Skipping handoff for channel {} (future generation {})",
                record.channel_id, record.born_generation
            );
            clear_handoff(provider, record.channel_id);
            continue;
        }

        // Skip if already executed/skipped/failed
        if record.state != "created" {
            println!(
                "  [{ts}] ⏭ Skipping handoff for channel {} (state={})",
                record.channel_id, record.state
            );
            clear_handoff(provider, record.channel_id);
            continue;
        }

        let is_dm = matches!(
            channel_id.to_channel(http).await,
            Ok(serenity::model::channel::Channel::Private(_))
        );
        let (allowlist_channel_id, provider_channel_name) =
            if let Some((pid, pname)) = resolve_thread_parent(http, channel_id).await {
                (pid, pname.or(record.channel_name.clone()))
            } else {
                (channel_id, record.channel_name.clone())
            };
        if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
            &settings_snapshot,
            provider,
            allowlist_channel_id,
            record.channel_name.as_deref(),
            provider_channel_name.as_deref(),
            is_dm,
        ) {
            println!(
                "  [{ts}] ⏭ Skipping handoff for channel {} — {reason}",
                record.channel_id
            );
            continue;
        }

        // Skip if pending queue messages exist (user intent takes priority)
        let has_pending = !mailbox_snapshot(shared, channel_id)
            .await
            .intervention_queue
            .is_empty();
        if has_pending {
            println!(
                "  [{ts}] ⏭ Skipping handoff for channel {} (pending queue has messages)",
                record.channel_id
            );
            let _ = update_handoff_state(provider, record.channel_id, "skipped");
            clear_handoff(provider, record.channel_id);
            continue;
        }

        // Skip if an active turn is already running
        let has_active = mailbox_has_active_turn(shared, channel_id).await;
        if has_active {
            println!(
                "  [{ts}] ⏭ Skipping handoff for channel {} (active turn running)",
                record.channel_id
            );
            let _ = update_handoff_state(provider, record.channel_id, "skipped");
            clear_handoff(provider, record.channel_id);
            continue;
        }

        // Check session/path readiness
        let has_session = {
            let data = shared.core.lock().await;
            data.sessions
                .get(&channel_id)
                .and_then(|s| s.current_path.as_ref())
                .is_some()
        };
        if !has_session {
            println!(
                "  [{ts}] ⏭ Skipping handoff for channel {} (no active session)",
                record.channel_id
            );
            let _ = update_handoff_state(provider, record.channel_id, "skipped");
            clear_handoff(provider, record.channel_id);
            continue;
        }

        // Mark as executing
        let _ = update_handoff_state(provider, record.channel_id, "executing");
        println!(
            "  [{ts}] ▶ Executing handoff for channel {} — {}",
            record.channel_id, record.intent
        );

        // Send a placeholder message in the channel
        let handoff_prompt = format!(
            "dcserver가 재시작되었습니다. 재시작 전 작업의 후속 조치를 이어서 진행해주세요.\n\n\
             ## 재시작 전 컨텍스트\n{}\n\n\
             ## 요청 사항\n{}",
            record.context, record.intent
        );

        let placeholder = match channel_id
            .send_message(
                http,
                serenity::CreateMessage::new().content(
                    "📎 **Post-restart handoff** — 재시작 후속 작업을 자동으로 이어받습니다.",
                ),
            )
            .await
        {
            Ok(msg) => msg,
            Err(e) => {
                println!(
                    "  [{ts}] ❌ Failed to send handoff placeholder for channel {}: {}",
                    record.channel_id, e
                );
                let _ = update_handoff_state(provider, record.channel_id, "failed");
                clear_handoff(provider, record.channel_id);
                continue;
            }
        };

        // Inject as an intervention so the next turn picks it up.
        mailbox_enqueue_intervention(
            shared,
            provider,
            channel_id,
            Intervention {
                author_id: serenity::UserId::new(1), // system-generated sentinel
                message_id: placeholder.id,
                text: handoff_prompt,
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
            },
        )
        .await;

        let _ = update_handoff_state(provider, record.channel_id, "completed");
        clear_handoff(provider, record.channel_id);
        println!(
            "  [{ts}] ✓ Handoff queued for channel {} (injected as intervention)",
            record.channel_id
        );
    }
}

/// #164: Re-deliver orphan pending dispatches after dcserver restart.
///
/// After a restart, dispatches in `pending` status may have been Discord-notified
/// but the in-memory intervention_queue was lost. Or the notification was interrupted
/// mid-flight. This function identifies truly orphan dispatches and re-delivers them.
///
/// **Safety**:
/// - Process-global once guard via `std::sync::Once` — safe across multiple provider instances
/// - Startup boot timestamp from dcserver.pid mtime — not wall clock
/// - Newer-dispatch check uses rowid (monotonic) instead of created_at (second-granularity)
/// - Five AND conditions must ALL be met before re-delivery (see issue #164)
async fn recover_orphan_pending_dispatches(shared: &Arc<SharedData>) {
    // Process-global once guard: prevents duplicate execution when multiple
    // provider instances (Claude + Codex) call this from their own setup paths.
    static ONCE: std::sync::Once = std::sync::Once::new();
    let mut should_run = false;
    ONCE.call_once(|| should_run = true);
    if !should_run {
        return;
    }

    let db = match shared.db.as_ref() {
        Some(d) => d,
        None => return,
    };

    // Boot timestamp from dcserver.pid mtime — represents actual process start,
    // not a wall-clock offset that could mis-classify old pending dispatches.
    let boot_time: String = {
        let pid_path =
            crate::cli::agentdesk_runtime_root().map(|r| r.join("runtime").join("dcserver.pid"));
        let mtime = pid_path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
        match mtime {
            Some(t) => {
                let dt: chrono::DateTime<chrono::Utc> = t.into();
                dt.format("%Y-%m-%d %H:%M:%S").to_string()
            }
            None => {
                // No pid file — cannot determine boot time safely, skip recovery
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ⚠ #164: No dcserver.pid — skipping orphan dispatch recovery");
                return;
            }
        }
    };

    // Query orphan pending dispatches with all 5 safety conditions:
    // 1. status = 'pending'
    // 2. card is assigned to the dispatch target agent
    // 3. agent has NO working session (idle)
    // 4. created_at < boot_time (pre-restart, using pid mtime)
    // 5. no newer dispatch for the same card (using rowid for monotonic ordering,
    //    avoids same-second ambiguity with created_at)
    let orphans: Vec<(String, String, String, String, String)> = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(_) => return,
        };
        let mut stmt = match conn.prepare(
            "SELECT d.id, d.to_agent_id, d.kanban_card_id, d.title, d.dispatch_type
             FROM task_dispatches d
             JOIN kanban_cards kc ON kc.id = d.kanban_card_id
             WHERE d.status = 'pending'
               AND d.created_at < ?1
               AND kc.assigned_agent_id = d.to_agent_id
               AND NOT EXISTS (
                 SELECT 1 FROM sessions s
                 WHERE s.agent_id = d.to_agent_id
                   AND s.status = 'working'
               )
               AND NOT EXISTS (
                 SELECT 1 FROM task_dispatches d2
                 WHERE d2.kanban_card_id = d.kanban_card_id
                   AND d2.rowid > d.rowid
                   AND d2.status NOT IN ('cancelled', 'failed')
               )",
        ) {
            Ok(s) => s,
            Err(_) => return,
        };
        stmt.query_map([&boot_time], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .ok()
        .map(|rows| rows.filter_map(|r| r.ok()).collect())
        .unwrap_or_default()
    };

    if orphans.is_empty() {
        return;
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] 🔄 #164: Found {} orphan pending dispatch(es) to re-deliver",
        orphans.len()
    );

    let mut delivered = 0usize;
    for (dispatch_id, agent_id, card_id, title, dtype) in &orphans {
        // Clear any existing dispatch_notified marker — the 5-condition query already
        // validated this dispatch is truly orphan, so the marker (if any) is stale.
        {
            let conn = match db.lock() {
                Ok(c) => c,
                Err(_) => continue,
            };
            conn.execute(
                "DELETE FROM kv_meta WHERE key = ?1",
                [&format!("dispatch_notified:{dispatch_id}")],
            )
            .ok();
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}]   ↻ Re-delivering {dtype} dispatch {id} → {agent} (card {card})",
            id = &dispatch_id[..8],
            agent = agent_id,
            card = &card_id[..8.min(card_id.len())],
        );

        // send_dispatch_to_discord handles its own two-phase delivery guard
        // (reserving → send → notified), so no manual marker management needed here.
        match crate::server::routes::dispatches::send_dispatch_to_discord(
            db,
            agent_id,
            title,
            card_id,
            dispatch_id,
        )
        .await
        {
            Ok(()) => {
                delivered += 1;
            }
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}]   ⚠ Recovery delivery failed for {id}: {e}",
                    id = &dispatch_id[..8],
                );
            }
        }
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] ✓ #164: Re-delivered {delivered}/{} orphan dispatch(es)",
        orphans.len()
    );
}

pub(super) fn discord_gateway_intents() -> serenity::GatewayIntents {
    serenity::GatewayIntents::GUILDS
        | serenity::GatewayIntents::GUILD_MESSAGES
        | serenity::GatewayIntents::GUILD_MESSAGE_REACTIONS
        | serenity::GatewayIntents::DIRECT_MESSAGES
        | serenity::GatewayIntents::DIRECT_MESSAGE_REACTIONS
        | serenity::GatewayIntents::MESSAGE_CONTENT
}

fn should_skip_agent_runtime_launch(token: &str) -> Option<String> {
    let bot = agentdesk_config::find_discord_bot_by_token(token)?;
    let agent_bot_names = agentdesk_config::collect_agent_bot_names();
    if !agent_bot_names.is_empty() && !agent_bot_names.contains(&bot.name) {
        return Some(bot.name);
    }
    None
}

/// Entry point: start the Discord bot
pub(crate) async fn run_bot(token: &str, provider: ProviderKind, context: RunBotContext) {
    let RunBotContext {
        global_active,
        global_finalizing,
        shutdown_remaining,
        health_registry,
        api_port,
        db,
        engine,
    } = context;

    if let Some(bot_name) = should_skip_agent_runtime_launch(token) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] ⏭ BOT-LAUNCH: skipping utility bot '{}' in run_bot() — not mapped to any agent channel",
            bot_name
        );
        shutdown_remaining.fetch_sub(1, Ordering::AcqRel);
        return;
    }

    super::internal_api::init(db.clone(), engine.clone(), health_registry.clone());

    // Initialize debug logging from environment variable
    claude::init_debug_from_env();

    let mut bot_settings = load_bot_settings(token);
    bot_settings.provider = provider.clone();

    match bot_settings.owner_user_id {
        Some(owner_id) => println!("  ✓ Owner: {owner_id}"),
        None => println!("  ⚠ No owner registered — first user will be registered as owner"),
    }

    let initial_skills = scan_skills(&provider, None);
    let skill_count = initial_skills.len();
    println!(
        "  ✓ {} bot ready — Skills loaded: {}",
        provider.display_name(),
        skill_count
    );

    // Cleanup stale Discord uploads on process start
    cleanup_old_uploads(UPLOAD_MAX_AGE);

    let provider_for_shutdown = provider.clone();
    let provider_for_error = provider.clone();

    let restored_model_overrides: Vec<(ChannelId, String)> = bot_settings
        .channel_model_overrides
        .iter()
        .filter_map(|(channel_id, model)| {
            channel_id
                .parse::<u64>()
                .ok()
                .map(|id| (ChannelId::new(id), model.clone()))
        })
        .collect();

    let shared = Arc::new(SharedData {
        core: Mutex::new(CoreState {
            sessions: HashMap::new(),
            active_meetings: HashMap::new(),
        }),
        mailboxes: ChannelMailboxRegistry::default(),
        settings: tokio::sync::RwLock::new(bot_settings),
        api_timestamps: dashmap::DashMap::new(),
        skills_cache: tokio::sync::RwLock::new(initial_skills),
        tmux_watchers: dashmap::DashMap::new(),
        recovering_channels: dashmap::DashMap::new(),
        shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        finalizing_turns: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        current_generation: runtime_store::load_generation(),
        restart_pending: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        reconcile_done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        deferred_hook_backlog: std::sync::atomic::AtomicUsize::new(0),
        recovery_started_at: std::time::Instant::now(),
        recovery_duration_ms: std::sync::atomic::AtomicU64::new(0),
        global_active,
        global_finalizing,
        shutdown_remaining,
        shutdown_counted: std::sync::atomic::AtomicBool::new(false),
        intake_dedup: dashmap::DashMap::new(),
        dispatch_thread_parents: dashmap::DashMap::new(),
        bot_connected: std::sync::atomic::AtomicBool::new(false),
        last_turn_at: std::sync::Mutex::new(None),
        model_overrides: {
            let map = dashmap::DashMap::new();
            for (channel_id, model) in &restored_model_overrides {
                map.insert(*channel_id, model.clone());
            }
            map
        },
        model_session_reset_pending: {
            let set = dashmap::DashSet::new();
            for (channel_id, _) in &restored_model_overrides {
                set.insert(*channel_id);
            }
            set
        },
        model_picker_pending: dashmap::DashMap::new(),
        dispatch_role_overrides: dashmap::DashMap::new(),
        last_message_ids: dashmap::DashMap::new(),
        turn_start_times: dashmap::DashMap::new(),
        cached_serenity_ctx: tokio::sync::OnceCell::new(),
        cached_bot_token: tokio::sync::OnceCell::new(),
        token_hash: settings::discord_token_hash(token),
        api_port,
        db,
        engine,
        known_slash_commands: tokio::sync::OnceCell::new(),
    });

    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] 🔑 dcserver generation: {}",
            shared.current_generation
        );
        if !restored_model_overrides.is_empty() {
            println!(
                "  [{ts}] 🧩 restored model overrides: {} channel(s)",
                restored_model_overrides.len()
            );
        }
    }

    // Register this provider with the health check registry
    health_registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;

    let token_owned = token.to_string();
    let shared_clone = shared.clone();

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: vec![
                commands::cmd_start(),
                commands::cmd_pwd(),
                commands::cmd_status(),
                commands::cmd_inflight(),
                commands::cmd_clear(),
                commands::cmd_stop(),
                commands::cmd_down(),
                commands::cmd_shell(),
                commands::cmd_cc(),
                commands::cmd_metrics(),
                commands::cmd_model(),
                commands::cmd_queue(),
                commands::cmd_health(),
                commands::cmd_allowedtools(),
                commands::cmd_allowed(),
                commands::cmd_debug(),
                commands::cmd_allowall(),
                commands::cmd_adduser(),
                commands::cmd_removeuser(),
                commands::cmd_receipt(),
                commands::cmd_help(),
                commands::cmd_meeting(),
            ],
            command_check: Some(|ctx| {
                Box::pin(async move {
                    let settings_snapshot = { ctx.data().shared.settings.read().await.clone() };
                    let allowed = provider_handles_channel(
                        ctx.serenity_context(),
                        &ctx.data().provider,
                        &settings_snapshot,
                        ctx.channel_id(),
                    )
                    .await;
                    if !allowed {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!(
                            "  [{ts}] ⏭ CMD-GUARD: skipping /{} in channel {} for provider {}",
                            ctx.command().name,
                            ctx.channel_id(),
                            ctx.data().provider.as_str()
                        );
                    }
                    Ok(allowed)
                })
            }),
            event_handler: |ctx, event, _framework, data| Box::pin(handle_event(ctx, event, data)),
            ..Default::default()
        })
        .setup(move |ctx, _ready, framework| {
            let shared_for_migrate = shared_clone.clone();
            let health_registry_for_setup = health_registry.clone();
            let provider_for_setup = provider.clone();
            let token_for_ready = token_owned.clone();
            Box::pin(async move {
                // Register in each guild for instant slash command propagation
                // (register_globally can take up to 1 hour)
                let commands = &framework.options().commands;
                // Populate known slash command names for router fallback logic
                let cmd_names: std::collections::HashSet<String> =
                    commands.iter().map(|c| c.name.clone()).collect();
                let _ = shared_for_migrate.known_slash_commands.set(cmd_names);
                for guild in &_ready.guilds {
                    if let Err(e) =
                        poise::builtins::register_in_guild(ctx, commands, guild.id).await
                    {
                        eprintln!(
                            "  ⚠ Failed to register commands in guild {}: {}",
                            guild.id, e
                        );
                    }
                }
                println!(
                    "  ✓ Bot connected — Registered commands in {} guild(s)",
                    _ready.guilds.len()
                );
                shared_for_migrate
                    .bot_connected
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = shared_for_migrate.cached_serenity_ctx.set(ctx.clone());
                let _ = shared_for_migrate.cached_bot_token.set(token_for_ready.clone());
                health_registry_for_setup
                    .register_http(provider_for_setup.as_str().to_string(), ctx.http.clone())
                    .await;

                // Enrich role_map.json with channelId for reliable name→ID resolution
                enrich_role_map_with_channel_ids();

                let shared_for_tmux = shared_for_migrate.clone();

                // Background: poll for deferred restart marker when idle
                let shared_for_deferred = shared_for_tmux.clone();
                let provider_for_deferred = provider.clone();
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(DEFERRED_RESTART_POLL_INTERVAL).await;
                        // Detect restart_pending marker and set the in-memory flag
                        // so the router queues new messages instead of starting turns.
                        if !shared_for_deferred.restart_pending.load(Ordering::Relaxed) {
                            if let Some(root) = crate::agentdesk_runtime_root() {
                                if root.join("restart_pending").exists() {
                                    shared_for_deferred
                                        .restart_pending
                                        .store(true, Ordering::SeqCst);
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!(
                                        "  [{ts}] ⏸ DRAIN: restart_pending detected, entering drain mode — new turns blocked"
                                    );
                                }
                            }
                        }
                        // Use process-global counters so we wait for ALL providers
                        let g_active = shared_for_deferred.global_active.load(Ordering::Relaxed);
                        let g_finalizing = shared_for_deferred
                            .global_finalizing
                            .load(Ordering::Relaxed);
                        if g_active == 0
                            && g_finalizing == 0
                            && shared_for_deferred.restart_pending.load(Ordering::Relaxed)
                        {
                            let queue_count = mailbox_restart_drain_all(
                                &shared_for_deferred,
                                &provider_for_deferred,
                            )
                            .await;
                            if queue_count > 0 {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                println!(
                                    "  [{ts}] 📋 DRAIN: mailbox persisted {queue_count} pending queue item(s) before deferred restart"
                                );
                            }
                            check_deferred_restart(&shared_for_deferred);
                            // This provider has saved and decremented — stop polling
                            return;
                        }
                    }
                });

                // Background: hot-reload skills on file changes (30s polling)
                // Scans home-level AND all active project-level skill directories.
                let shared_for_skills = shared_for_tmux.clone();
                let provider_for_skills = provider.clone();
                tokio::spawn(async move {
                    let mut last_fingerprint: (usize, u64) = (0, 0); // (file_count, max_mtime_epoch)
                    loop {
                        tokio::time::sleep(tokio::time::Duration::from_secs(30)).await;
                        // Collect unique project paths from active sessions
                        let project_paths: Vec<String> = {
                            let data = shared_for_skills.core.lock().await;
                            let mut paths: Vec<String> = data
                                .sessions
                                .values()
                                .filter_map(|s| s.current_path.clone())
                                .collect();
                            paths.sort();
                            paths.dedup();
                            paths
                        };
                        let fp =
                            skill_dir_fingerprint_with_projects(&provider_for_skills, &project_paths);
                        if fp != last_fingerprint && last_fingerprint != (0, 0) {
                            // Merge home + all project skills (scan_skills deduplicates by name)
                            let mut merged = scan_skills(&provider_for_skills, None);
                            let mut seen: std::collections::HashSet<String> =
                                merged.iter().map(|(n, _)| n.clone()).collect();
                            for path in &project_paths {
                                for skill in scan_skills(&provider_for_skills, Some(path)) {
                                    if seen.insert(skill.0.clone()) {
                                        merged.push(skill);
                                    }
                                }
                            }
                            merged.sort_by(|a, b| a.0.cmp(&b.0));
                            let count = merged.len();
                            *shared_for_skills.skills_cache.write().await = merged;
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!(
                                "  [{ts}] 🔄 Skills hot-reloaded: {count} skill(s) ({} files, mtime Δ)",
                                fp.0
                            );
                        }
                        last_fingerprint = fp;
                    }
                });

                // Restore inflight turns FIRST, then flush restart reports.
                // Recovery skips channels that have a pending restart report,
                // so the report must still be on disk when recovery runs.
                // After recovery completes, the flush loop starts and delivers/clears reports.
                let http_for_tmux = ctx.http.clone();
                let shared_for_tmux2 = shared_for_tmux.clone();
                let http_for_restart_reports = ctx.http.clone();
                let ctx_for_kickoff = ctx.clone();
                let token_for_kickoff = token_owned.clone();
                let shared_for_restart_reports = shared_for_tmux.clone();
                let provider_for_restore = provider.clone();
                tokio::spawn(async move {
                    let is_utility_bot = {
                        let s = shared_for_tmux2.settings.read().await;
                        s.agent.is_some()
                    };
                    if is_utility_bot {
                        mark_reconcile_complete(&shared_for_tmux2);
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}] ✓ Utility bot reconcile — skipped recovery");
                    } else {
                        // #429: Recover restart-gap messages first so new user input gets queued
                        // within seconds of bot ready instead of waiting behind slower
                        // Discord API-heavy inflight/thread-map recovery passes.
                        catch_up_missed_messages(
                            &http_for_tmux,
                            &shared_for_tmux2,
                            &provider_for_restore,
                        )
                        .await;

                        gc_stale_fixed_working_sessions(&shared_for_tmux2).await;
                        restore_inflight_turns(
                            &http_for_tmux,
                            &shared_for_tmux2,
                            &provider_for_restore,
                        )
                        .await;

                        // Restore pending intervention queues saved during previous SIGTERM
                        let (restored_queues, restored_overrides) =
                            load_pending_queues(&provider_for_restore, &shared_for_tmux2.token_hash);
                        let allowed_bot_ids_for_restore: Vec<u64> = {
                            let settings = shared_for_tmux2.settings.read().await;
                            settings.allowed_bot_ids.clone()
                        };
                        // P1-1: Restore dispatch_role_overrides from queue snapshots
                        for (thread_channel_id, alt_channel_id) in &restored_overrides {
                            if !matches!(
                                resolve_runtime_channel_binding_status(
                                    &http_for_tmux,
                                    *thread_channel_id,
                                )
                                .await,
                                RuntimeChannelBindingStatus::Owned
                            ) {
                                continue;
                            }
                            shared_for_tmux2
                                .dispatch_role_overrides
                                .insert(*thread_channel_id, *alt_channel_id);
                        }
                        if !restored_overrides.is_empty() {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!(
                                "  [{ts}] 📋 FLUSH: restored {} dispatch_role_override(s) from queue snapshots",
                                restored_overrides.len()
                            );
                        }
                        if !restored_queues.is_empty() {
                            let mut added = 0usize;
                            let mut skipped = 0usize;
                            for (channel_id, items) in restored_queues {
                                if !matches!(
                                    resolve_runtime_channel_binding_status(&http_for_tmux, channel_id)
                                        .await,
                                    RuntimeChannelBindingStatus::Owned
                                ) {
                                    skipped += items.len();
                                    continue;
                                }
                                let snapshot = mailbox_snapshot(&shared_for_tmux2, channel_id).await;
                                let mut existing_ids = recovery_known_message_ids(&snapshot);
                                let mut queue = snapshot.intervention_queue;
                                for item in items {
                                    if allowed_bot_ids_for_restore.contains(&item.author_id.get())
                                        && !should_process_allowed_bot_turn_text(&item.text)
                                    {
                                        skipped += 1;
                                        continue;
                                    }
                                    if existing_ids.insert(item.message_id.get()) {
                                        queue.push(item);
                                        added += 1;
                                    } else {
                                        skipped += 1;
                                    }
                                }
                                mailbox_replace_queue(
                                    &shared_for_tmux2,
                                    &provider_for_restore,
                                    channel_id,
                                    queue,
                                )
                                .await;
                            }
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!(
                                "  [{ts}] 📋 FLUSH: restored {added} pending queue item(s) from disk (skipped {skipped} duplicates)"
                            );
                        }

                        // P1-2: Warn about legacy queue files that cannot be restored
                        warn_legacy_pending_queue_files(&provider_for_restore);

                        // #429: thread-map validation in background — non-blocking
                        if let Some(ref db) = shared_for_tmux2.db {
                            let db_bg = db.clone();
                            let token_bg = token_for_kickoff.clone();
                            tokio::spawn(async move {
                                let (checked, cleared) =
                                    crate::server::routes::dispatches::validate_channel_thread_maps_on_startup(
                                        &db_bg,
                                        &token_bg,
                                    )
                                    .await;
                                if checked > 0 || cleared > 0 {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!(
                                        "  [{ts}] 🧹 THREAD-MAP: validated {checked} mapping(s), cleared {cleared} stale binding(s)"
                                    );
                                }
                            });
                        }

                        // #226: Collect channels that recovery already handled (spawned + ended watchers).
                        // restore_tmux_watchers must skip these to prevent duplicate watcher creation.
                        // The issue: recovery watcher starts → session ends quickly → watcher removes
                        // itself from DashMap → restore_tmux_watchers sees empty slot → creates second watcher.
                        #[cfg(unix)]
                        {
                            // Mark all channels that recovery touched as "recently handled"
                            // by inserting a recovery_handled marker in kv_meta.
                            // restore_tmux_watchers checks this and skips those channels.
                            if let Some(ref db) = shared_for_tmux2.db {
                                if let Ok(conn) = db.lock() {
                                    let recovery_channels: Vec<u64> = shared_for_tmux2
                                        .recovering_channels
                                        .iter()
                                        .map(|entry| entry.key().get())
                                        .collect();
                                    for ch in &recovery_channels {
                                        conn.execute(
                                            "INSERT OR REPLACE INTO kv_meta (key, value) VALUES (?1, ?2)",
                                            rusqlite::params![
                                                format!("recovery_handled_channel:{ch}"),
                                                chrono::Utc::now().timestamp().to_string(),
                                            ],
                                        )
                                        .ok();
                                    }
                                }
                            }

                            restore_tmux_watchers(&http_for_tmux, &shared_for_tmux2).await;
                            cleanup_orphan_tmux_sessions(&shared_for_tmux2).await;

                            // Clean up recovery markers
                            if let Some(ref db) = shared_for_tmux2.db {
                                if let Ok(conn) = db.lock() {
                                    conn.execute(
                                        "DELETE FROM kv_meta WHERE key LIKE 'recovery_handled_channel:%'",
                                        [],
                                    )
                                    .ok();
                                }
                            }
                        }

                        // Execute durable handoffs (post-restart follow-up work)
                        execute_handoff_turns(
                            &http_for_restart_reports,
                            &shared_for_restart_reports,
                            &provider_for_restore,
                        )
                        .await;

                        // #164: Re-deliver orphan pending dispatches from before restart
                        recover_orphan_pending_dispatches(&shared_for_restart_reports).await;

                        // Kick off turns for channels that have queued messages but no
                        // active turn. Without this, restored pending queues and handoff
                        // injections sit idle until the next user message arrives.
                        kickoff_idle_queues(
                            &ctx_for_kickoff,
                            &shared_for_restart_reports,
                            &token_for_kickoff,
                            &provider_for_restore,
                        )
                        .await;

                        // #122: Reconcile phase complete — open intake
                        mark_reconcile_complete(&shared_for_restart_reports);
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}] ✓ Reconcile complete — intake open");
                    } // end of !is_utility_bot recovery block

                    // Kick off again to drain messages queued during reconcile window
                    kickoff_idle_queues(
                        &ctx_for_kickoff,
                        &shared_for_restart_reports,
                        &token_for_kickoff,
                        &provider_for_restore,
                    )
                    .await;

                    // Thread-map validation is best-effort hygiene and can spend
                    // multiple REST round-trips on startup. Do not block intake
                    // reopening or queued-turn kickoff on it.
                    if let Some(db) = shared_for_tmux2.db.clone() {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}] 🧹 THREAD-MAP: continuing validation in background");
                        spawn_startup_thread_map_validation(db, token_for_kickoff.clone());
                    }

                    // NOW flush restart reports (recovery is done, safe to delete them)
                    flush_restart_reports(
                        &http_for_restart_reports,
                        &shared_for_restart_reports,
                        &provider_for_restore,
                    )
                    .await;
                    // Continue flushing in a loop for any reports created later
                    loop {
                        tokio::time::sleep(RESTART_REPORT_FLUSH_INTERVAL).await;
                        flush_restart_reports(
                            &http_for_restart_reports,
                            &shared_for_restart_reports,
                            &provider_for_restore,
                        )
                        .await;
                    }
                });

                // Background: periodic cleanup for stale Discord upload files
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(UPLOAD_CLEANUP_INTERVAL).await;
                        cleanup_old_uploads(UPLOAD_MAX_AGE);
                    }
                });

                // Background: periodic reaper for dead tmux sessions that
                // still show as working in the DB (catches watcher gaps)
                #[cfg(unix)]
                {
                    let shared_for_reaper = shared_clone.clone();
                    tokio::spawn(async move {
                        // Initial delay: let startup recovery finish first
                        tokio::time::sleep(tokio::time::Duration::from_secs(90)).await;
                        loop {
                            reap_dead_tmux_sessions(&shared_for_reaper).await;
                            tokio::time::sleep(DEAD_SESSION_REAP_INTERVAL).await;
                        }
                    });
                }

                // Background: periodic GC for stale thread sessions in DB
                // (idle/disconnected thread sessions older than 1 hour)
                {
                    let api_port = shared_clone.api_port;
                    let shared_for_session_gc = shared_clone.clone();
                    tokio::spawn(async move {
                        // Run every 10 minutes, initial delay 2 minutes
                        tokio::time::sleep(tokio::time::Duration::from_secs(120)).await;
                        loop {
                            gc_stale_fixed_working_sessions(&shared_for_session_gc).await;
                            gc_stale_thread_sessions_via_api(api_port).await;
                            tokio::time::sleep(tokio::time::Duration::from_secs(600)).await;
                        }
                    });
                }

                Ok(Data {
                    shared: shared_clone,
                    token: token_owned,
                    provider,
                })
            })
        })
        .build();

    let intents = discord_gateway_intents();

    let mut client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .await
        .expect("Failed to create Discord client");

    // Graceful shutdown: on SIGTERM, cancel all tmux watchers before dying
    let shared_for_signal = shared.clone();
    let token_for_signal = token.to_string();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                sigterm.recv().await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] 🛑 SIGTERM received — graceful shutdown");

                // Set global shutdown flag
                shared_for_signal
                    .shutting_down
                    .store(true, std::sync::atomic::Ordering::SeqCst);

                // Block dequeue and put router into drain mode so no new
                // queue/checkpoint mutations occur during shutdown.
                shared_for_signal
                    .restart_pending
                    .store(true, std::sync::atomic::Ordering::SeqCst);

                // Cancel all active tmux watchers (quiet exit, no "session ended" messages)
                for entry in shared_for_signal.tmux_watchers.iter() {
                    entry
                        .value()
                        .cancel
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }

                // Grace period for watchers to see cancel flag and exit cleanly.
                // Active turns may also finish during this window.
                tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

                // ── Critical state persistence (MUST run before any I/O) ──
                // Save pending queues and last_message_ids FIRST, before any
                // network calls that might block/timeout and prevent saving.

                let queue_count =
                    mailbox_restart_drain_all(&shared_for_signal, &provider_for_shutdown).await;
                if queue_count > 0 {
                    let ts3 = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts3}] 📋 mailbox persisted {queue_count} pending queue item(s)");
                }

                // Persist last_message_ids for catch-up polling after restart
                {
                    let ids: std::collections::HashMap<u64, u64> = shared_for_signal
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    if !ids.is_empty() {
                        runtime_store::save_all_last_message_ids(
                            provider_for_shutdown.as_str(),
                            &ids,
                        );
                    }
                }

                // ── Inflight state, restart reports & placeholder updates ──
                let inflight_states = inflight::load_inflight_states(&provider_for_shutdown);

                // Save restart reports FIRST (disk-only, guaranteed to complete)
                // before any HTTP calls that might hang/timeout.
                for state in &inflight_states {
                    let existing = restart_report::load_restart_report(
                        &provider_for_shutdown,
                        state.channel_id,
                    );
                    if existing.as_ref().map(|r| r.status.as_str()) == Some("pending") {
                        continue;
                    }
                    let mut report = restart_report::RestartCompletionReport::new(
                        provider_for_shutdown.clone(),
                        state.channel_id,
                        "sigterm",
                        "dcserver가 SIGTERM으로 종료되었습니다. 재시작 후 작업을 이어받습니다.",
                    );
                    report.current_msg_id = Some(state.current_msg_id);
                    report.channel_name = state.channel_name.clone();
                    report.user_msg_id = Some(state.user_msg_id);
                    if let Err(e) = restart_report::save_restart_report(&report) {
                        eprintln!(
                            "  ⚠ failed to save restart report for channel {}: {e}",
                            state.channel_id
                        );
                    }
                }
                if !inflight_states.is_empty() {
                    let ts2 = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts2}] 📝 saved {} restart report(s) for inflight channels",
                        inflight_states.len()
                    );
                }

                // Best-effort: update placeholder messages with restart notice.
                // Each edit gets a 3-second timeout to avoid blocking shutdown.
                if !inflight_states.is_empty() {
                    let http = serenity::Http::new(&token_for_signal);
                    for state in &inflight_states {
                        let channel = ChannelId::new(state.channel_id);
                        let msg_id = MessageId::new(state.current_msg_id);
                        let restart_notice = if state.full_response.trim().is_empty() {
                            "⚠️ dcserver 재시작으로 중단됨 — 곧 복원됩니다".to_string()
                        } else {
                            let partial = formatting::format_for_discord_with_provider(
                                state.full_response.trim(),
                                &provider_for_shutdown,
                            );
                            format!("{partial}\n\n⚠️ dcserver 재시작으로 중단됨 — 곧 복원됩니다")
                        };
                        let edit_fut = channel.edit_message(
                            &http,
                            msg_id,
                            EditMessage::new().content(&restart_notice),
                        );
                        match tokio::time::timeout(tokio::time::Duration::from_secs(3), edit_fut)
                            .await
                        {
                            Ok(Ok(_)) => {
                                let ts_ok = chrono::Local::now().format("%H:%M:%S");
                                println!(
                                    "  [{ts_ok}] ✓ Updated placeholder msg {} in channel {}",
                                    state.current_msg_id, state.channel_id
                                );
                            }
                            Ok(Err(e)) => {
                                eprintln!(
                                    "  ⚠ Failed to update placeholder msg {} in channel {}: {e}",
                                    state.current_msg_id, state.channel_id
                                );
                            }
                            Err(_) => {
                                eprintln!(
                                    "  ⚠ Timeout updating placeholder msg {} in channel {}",
                                    state.current_msg_id, state.channel_id
                                );
                            }
                        }
                    }
                }

                // ── Final state snapshot (belt-and-suspenders) ──
                // During the HTTP placeholder edits above, active turns may have
                // finished and mutated queues/last_message_ids. Re-save to capture
                // any changes that occurred after the initial save.
                {
                    let queue_count =
                        mailbox_restart_drain_all(&shared_for_signal, &provider_for_shutdown).await;
                    if queue_count > 0 {
                        let ts4 = chrono::Local::now().format("%H:%M:%S");
                        println!(
                            "  [{ts4}] 📋 mailbox final drain: {queue_count} pending queue item(s)"
                        );
                    }
                }
                {
                    let ids: std::collections::HashMap<u64, u64> = shared_for_signal
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    if !ids.is_empty() {
                        runtime_store::save_all_last_message_ids(
                            provider_for_shutdown.as_str(),
                            &ids,
                        );
                    }
                }

                // Wait for all providers to finish saving before exiting.
                // CAS guard: skip if this provider already decremented via deferred restart path.
                if shared_for_signal
                    .shutdown_counted
                    .compare_exchange(
                        false,
                        true,
                        std::sync::atomic::Ordering::AcqRel,
                        std::sync::atomic::Ordering::Relaxed,
                    )
                    .is_ok()
                {
                    if shared_for_signal
                        .shutdown_remaining
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
                        == 1
                    {
                        std::process::exit(0);
                    }
                }
            }
        }
    });

    if let Err(e) = client.start().await {
        eprintln!("  ✗ {} bot error: {e}", provider_for_error.display_name());
    }
}

/// Periodic GC: delete stale idle/disconnected thread sessions from DB via cleanup API.
async fn gc_stale_thread_sessions_via_api(api_port: u16) {
    let _ = api_port;
    match super::internal_api::gc_stale_thread_sessions() {
        Ok(gc) if gc > 0 => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] 🧹 GC: removed {gc} stale thread session(s) from DB");
        }
        Ok(_) => {}
        Err(err) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!("  [{ts}] ⚠ Thread session GC error: {err}");
        }
    }
}

/// Periodic GC: disconnect stale fixed-channel working sessions from the DB so
/// restart recovery cannot restore dead provider session IDs.
async fn gc_stale_fixed_working_sessions(shared: &Arc<SharedData>) {
    let Some(db) = &shared.db else {
        return;
    };

    let cleared = {
        let conn = match db.lock() {
            Ok(c) => c,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                eprintln!("  [{ts}] ⚠ Fixed-session GC lock error: {e}");
                return;
            }
        };
        crate::server::routes::dispatched_sessions::gc_stale_fixed_working_sessions_db(&conn)
    };

    if cleared > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] 🧹 GC: disconnected {cleared} stale fixed-channel working session(s)");
    }
}
