use super::*;
use sqlx::Row as SqlxRow;

pub(crate) struct RunBotContext {
    pub(crate) global_active: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) global_finalizing: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) shutdown_remaining: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) health_registry: Arc<health::HealthRegistry>,
    pub(crate) api_port: u16,
    pub(crate) db: Option<crate::db::Db>,
    pub(crate) pg_pool: Option<sqlx::PgPool>,
    pub(crate) engine: Option<crate::engine::PolicyEngine>,
}

const DISCORD_GATEWAY_LEASE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const DISCORD_GATEWAY_LOCK_PREFIX: u64 = 0x0443_0000_0000_0000;

fn restored_fast_mode_enabled_channels_for_provider(
    bot_settings: &DiscordBotSettings,
    _provider: &ProviderKind,
) -> Vec<ChannelId> {
    let mut channels: Vec<ChannelId> = bot_settings
        .channel_fast_modes
        .iter()
        .filter_map(|(channel_id, enabled)| {
            if !*enabled {
                return None;
            }
            channel_id.parse::<u64>().ok().map(ChannelId::new)
        })
        .collect();
    channels.sort_unstable_by_key(|channel_id| channel_id.get());
    channels
}

fn restored_fast_mode_reset_entries(bot_settings: &DiscordBotSettings) -> Vec<String> {
    let mut entries: Vec<String> = bot_settings
        .channel_fast_mode_reset_pending
        .iter()
        .cloned()
        .collect();
    entries.sort_unstable();
    entries
}

fn restored_fast_mode_reset_channels(bot_settings: &DiscordBotSettings) -> Vec<ChannelId> {
    let mut channels: Vec<ChannelId> = bot_settings
        .channel_fast_mode_reset_pending
        .iter()
        .filter_map(|entry| {
            let raw_channel_id = entry
                .split_once(':')
                .map(|(_, channel_id)| channel_id)
                .unwrap_or(entry.as_str());
            raw_channel_id.parse::<u64>().ok().map(ChannelId::new)
        })
        .collect();
    channels.sort_unstable_by_key(|channel_id| channel_id.get());
    channels.dedup_by_key(|channel_id| channel_id.get());
    channels
}

fn discord_gateway_lock_id(token_hash: &str) -> i64 {
    // `discord_token_hash()` returns "discord_<16hex>". Strip the literal prefix
    // so the first 16 chars we sample are actual hex; otherwise the `is_ascii_hexdigit`
    // check fails on non-hex letters in the prefix and every bot collapses onto the
    // same fallback lock id, causing only one bot to acquire the singleton lease.
    let raw = token_hash.strip_prefix("discord_").unwrap_or(token_hash);
    let hex = raw
        .get(..16)
        .filter(|prefix| prefix.chars().all(|ch| ch.is_ascii_hexdigit()))
        .unwrap_or("0");
    let parsed = u64::from_str_radix(hex, 16).unwrap_or(0);
    let suffix = parsed & 0x0000_FFFF_FFFF_FFFF;
    (DISCORD_GATEWAY_LOCK_PREFIX | suffix) as i64
}

async fn try_acquire_discord_gateway_lease(
    pool: &sqlx::PgPool,
    token_hash: &str,
    provider: &ProviderKind,
) -> Result<Option<crate::db::postgres::AdvisoryLockLease>, String> {
    crate::db::postgres::AdvisoryLockLease::try_acquire(
        pool,
        discord_gateway_lock_id(token_hash),
        format!("discord gateway {}", provider.as_str()),
    )
    .await
}

fn restored_intervention_message_ids(item: &Intervention) -> Vec<u64> {
    let mut item_ids: Vec<u64> = item.source_message_ids.iter().map(|id| id.get()).collect();
    if item_ids.is_empty() {
        item_ids.push(item.message_id.get());
    } else if !item_ids.contains(&item.message_id.get()) {
        item_ids.push(item.message_id.get());
    }
    item_ids
}

fn enqueue_restored_intervention(
    existing_ids: &mut std::collections::HashSet<u64>,
    queue: &mut Vec<Intervention>,
    item: Intervention,
) -> bool {
    let item_ids = restored_intervention_message_ids(&item);
    // Persisted merged queue items may represent multiple source messages. If startup
    // catch-up already recovered only some of them, dropping the whole item would lose
    // the unseen messages because the merged text is no longer separable.
    if item_ids
        .iter()
        .all(|message_id| existing_ids.contains(message_id))
    {
        return false;
    }

    existing_ids.extend(item_ids);
    queue.push(item);
    true
}

fn spawn_startup_thread_map_validation(db: crate::db::Db, token: String) {
    tokio::spawn(async move {
        let (checked, cleared) =
            crate::server::routes::dispatches::validate_channel_thread_maps_on_startup(&db, &token)
                .await;
        if checked > 0 || cleared > 0 {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 THREAD-MAP: validated {checked} mapping(s), cleared {cleared} stale binding(s)"
            );
        }
    });
}

/// Suppress durable handoff turns saved before a restart.
/// Runs after tmux watcher restore and pending queue restore, but before
/// restart report flush so stale handoff files do not synthesize a new turn.
async fn execute_handoff_turns(
    _http: &Arc<serenity::Http>,
    _shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let handoffs = load_handoffs(provider);
    if handoffs.is_empty() {
        return;
    }
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 📎 Found {} handoff record(s) to suppress",
        handoffs.len()
    );

    for record in handoffs {
        let ts = chrono::Local::now().format("%H:%M:%S");
        let _ = update_handoff_state(provider, record.channel_id, "skipped");
        clear_handoff(provider, record.channel_id);
        tracing::info!(
            "  [{ts}] ⏭ Suppressed auto post-restart handoff for channel {}",
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
    let pg_pool = shared.pg_pool.as_ref();

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
                tracing::info!(
                    "  [{ts}] ⚠ #164: No dcserver.pid — skipping orphan dispatch recovery"
                );
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
    let orphans: Vec<(String, String, String, String, String)> = if let Some(pool) = pg_pool {
        match sqlx::query(
            "SELECT d.id, d.to_agent_id, d.kanban_card_id, d.title, d.dispatch_type
               FROM task_dispatches d
               JOIN kanban_cards kc ON kc.id = d.kanban_card_id
              WHERE d.status = 'pending'
                AND d.created_at < $1::timestamptz
                AND kc.assigned_agent_id = d.to_agent_id
                AND NOT EXISTS (
                    SELECT 1 FROM sessions s
                     WHERE s.agent_id = d.to_agent_id
                       AND s.status = 'working'
                )
                AND NOT EXISTS (
                    SELECT 1 FROM task_dispatches d2
                     WHERE d2.kanban_card_id = d.kanban_card_id
                       AND d2.status NOT IN ('cancelled', 'failed')
                       AND d2.created_at > d.created_at
                )",
        )
        .bind(&boot_time)
        .fetch_all(pool)
        .await
        {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|row| {
                    Some((
                        row.try_get::<String, _>("id").ok()?,
                        row.try_get::<String, _>("to_agent_id").ok()?,
                        row.try_get::<String, _>("kanban_card_id").ok()?,
                        row.try_get::<String, _>("title").ok()?,
                        row.try_get::<String, _>("dispatch_type").ok()?,
                    ))
                })
                .collect(),
            Err(error) => {
                tracing::warn!(
                    "[dispatch-recovery] failed to query postgres orphan dispatches: {error}"
                );
                return;
            }
        }
    } else {
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
    tracing::info!(
        "  [{ts}] 🔄 #164: Found {} orphan pending dispatch(es) to re-deliver",
        orphans.len()
    );

    let mut delivered = 0usize;
    for (dispatch_id, agent_id, card_id, title, dtype) in &orphans {
        // Clear any existing dispatch_notified marker — the 5-condition query already
        // validated this dispatch is truly orphan, so the marker (if any) is stale.
        {
            if let Some(pool) = pg_pool {
                sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                    .bind(format!("dispatch_notified:{dispatch_id}"))
                    .execute(pool)
                    .await
                    .ok();
            } else {
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
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}]   ↻ Re-delivering {dtype} dispatch {id} → {agent} (card {card})",
            id = &dispatch_id[..8],
            agent = agent_id,
            card = &card_id[..8.min(card_id.len())],
        );

        // send_dispatch_to_discord handles its own two-phase delivery guard
        // (reserving → send → notified), so no manual marker management needed here.
        let send_result = if let Some(pool) = pg_pool {
            crate::server::routes::dispatches::send_dispatch_to_discord_with_pg(
                db,
                Some(pool),
                agent_id,
                title,
                card_id,
                dispatch_id,
            )
            .await
        } else {
            crate::server::routes::dispatches::send_dispatch_to_discord(
                db,
                agent_id,
                title,
                card_id,
                dispatch_id,
            )
            .await
        };
        match send_result {
            Ok(()) => {
                delivered += 1;
            }
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}]   ⚠ Recovery delivery failed for {id}: {e}",
                    id = &dispatch_id[..8],
                );
            }
        }
    }

    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
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
        pg_pool,
        engine,
    } = context;

    if let Some(bot_name) = should_skip_agent_runtime_launch(token) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⏭ BOT-LAUNCH: skipping utility bot '{}' in run_bot() — not mapped to any agent channel",
            bot_name
        );
        shutdown_remaining.fetch_sub(1, Ordering::AcqRel);
        return;
    }

    let token_hash = settings::discord_token_hash(token);
    let gateway_lease = match pg_pool.as_ref() {
        Some(pool) => match try_acquire_discord_gateway_lease(pool, &token_hash, &provider).await {
            Ok(Some(lease)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔐 GATEWAY-LEASE: {} acquired singleton lease",
                    provider.display_name()
                );
                Some(lease)
            }
            Ok(None) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⏭ GATEWAY-LEASE: {} launch skipped — singleton lease held elsewhere",
                    provider.display_name()
                );
                shutdown_remaining.fetch_sub(1, Ordering::AcqRel);
                return;
            }
            Err(error) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⏭ GATEWAY-LEASE: {} launch skipped — failed to acquire singleton lease: {}",
                    provider.display_name(),
                    error
                );
                shutdown_remaining.fetch_sub(1, Ordering::AcqRel);
                return;
            }
        },
        None => None,
    };

    super::internal_api::init(
        db.clone(),
        pg_pool.clone(),
        engine.clone(),
        health_registry.clone(),
    );

    // Initialize debug logging from environment variable
    claude::init_debug_from_env();

    let mut bot_settings = load_bot_settings(token);
    bot_settings.provider = provider.clone();

    match bot_settings.owner_user_id {
        Some(owner_id) => tracing::info!("  ✓ Owner: {owner_id}"),
        None => tracing::info!(
            "  ⚠ No owner registered — configure discord.owner_id (or allow_all_users) before use"
        ),
    }

    let initial_skills = scan_skills(&provider, None);
    let skill_count = initial_skills.len();
    tracing::info!(
        "  ✓ {} bot ready — Skills loaded: {}",
        provider.display_name(),
        skill_count
    );

    // Cleanup stale Discord uploads on process start
    cleanup_old_uploads(UPLOAD_MAX_AGE);

    let provider_for_shutdown = provider.clone();
    let provider_for_error = provider.clone();
    let provider_for_framework = provider.clone();

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
    let restored_fast_mode_channels =
        restored_fast_mode_enabled_channels_for_provider(&bot_settings, &provider);
    let restored_fast_mode_reset_entries = restored_fast_mode_reset_entries(&bot_settings);
    let restored_fast_mode_reset_channels = restored_fast_mode_reset_channels(&bot_settings);

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
        fast_mode_channels: {
            let set = dashmap::DashSet::new();
            for channel_id in &restored_fast_mode_channels {
                set.insert(*channel_id);
            }
            set
        },
        fast_mode_session_reset_pending: {
            let set = dashmap::DashSet::new();
            for entry in &restored_fast_mode_reset_entries {
                set.insert(entry.clone());
            }
            set
        },
        model_session_reset_pending: {
            let set = dashmap::DashSet::new();
            for (channel_id, _) in &restored_model_overrides {
                set.insert(*channel_id);
            }
            set
        },
        session_reset_pending: {
            let set = dashmap::DashSet::new();
            for (channel_id, _) in &restored_model_overrides {
                set.insert(*channel_id);
            }
            for channel_id in &restored_fast_mode_reset_channels {
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
        token_hash: token_hash.clone(),
        api_port,
        db,
        pg_pool,
        engine,
        health_registry: Arc::downgrade(&health_registry),
        known_slash_commands: tokio::sync::OnceCell::new(),
    });

    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔑 dcserver generation: {}",
            shared.current_generation
        );
        if !restored_model_overrides.is_empty() {
            tracing::info!(
                "  [{ts}] 🧩 restored model overrides: {} channel(s)",
                restored_model_overrides.len()
            );
        }
        if !restored_fast_mode_channels.is_empty() {
            tracing::info!(
                "  [{ts}] ⚡ restored fast mode channels: {} channel(s)",
                restored_fast_mode_channels.len()
            );
        }
    }

    // Register this provider with the health check registry
    health_registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;

    let token_owned = token.to_string();
    let shared_clone = shared.clone();

    let mut slash_commands = vec![
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
        commands::cmd_fast(),
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
        commands::cmd_receipt(),
        commands::cmd_help(),
        commands::cmd_meeting(),
        commands::cmd_mcp_reload(),
    ]);

    let framework = poise::Framework::builder()
        .options(poise::FrameworkOptions {
            commands: slash_commands,
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
                        tracing::info!(
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
            let provider_for_setup = provider_for_framework.clone();
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
                        tracing::warn!(
                            "  ⚠ Failed to register commands in guild {}: {}",
                            guild.id, e
                        );
                    }
                }
                tracing::info!(
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
                let provider_for_deferred = provider_for_setup.clone();
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
                                    tracing::info!(
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
                                tracing::info!(
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
                let provider_for_skills = provider_for_setup.clone();
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
                            tracing::info!(
                                "  [{ts}] 🔄 Skills hot-reloaded: {count} skill(s) ({} files, mtime Δ)",
                                fp.0
                            );
                        }
                        last_fingerprint = fp;
                    }
                });

                // #799: MCP credential watcher (Claude only).
                // Watches ~/.claude/.credentials.json and ~/.claude/mcp.json and posts
                // a one-line notification to all active Claude sessions when one of
                // them changes, so the operator can run /mcp-reload to pick up
                // newly-authenticated MCP servers without losing context.
                if matches!(provider_for_setup, ProviderKind::Claude) {
                    let mcp_cfg = crate::config::load_graceful().mcp;
                    if mcp_cfg.watch_credentials {
                        let dedupe_window = std::time::Duration::from_secs(
                            mcp_cfg.credential_notify_dedupe_secs,
                        );
                        super::mcp_credential_watcher::spawn_watcher(
                            shared_for_tmux.clone(),
                            dedupe_window,
                            "🔔 MCP credential 변화 감지됨. 새 MCP 적용하려면 `/mcp-reload`.",
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
                let http_for_tmux = ctx.http.clone();
                let shared_for_tmux2 = shared_for_tmux.clone();
                let http_for_restart_reports = ctx.http.clone();
                let ctx_for_kickoff = ctx.clone();
                let token_for_kickoff = token_owned.clone();
                let shared_for_restart_reports = shared_for_tmux.clone();
                let provider_for_restore = provider_for_setup.clone();
                tokio::spawn(async move {
                    let is_utility_bot = {
                        let s = shared_for_tmux2.settings.read().await;
                        s.agent.is_some()
                    };
                    if is_utility_bot {
                        mark_reconcile_complete(&shared_for_tmux2);
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!("  [{ts}] ✓ Utility bot reconcile — skipped recovery");
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
                        let announce_bot_id_for_restore =
                            super::resolve_announce_bot_user_id(&shared_for_tmux2).await;
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
                            tracing::info!(
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
                                    if !super::is_allowed_turn_sender(
                                        &allowed_bot_ids_for_restore,
                                        announce_bot_id_for_restore,
                                        item.author_id.get(),
                                        true,
                                        &item.text,
                                    ) {
                                        skipped += 1;
                                        continue;
                                    }
                                    if enqueue_restored_intervention(
                                        &mut existing_ids,
                                        &mut queue,
                                        item,
                                    ) {
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
                            tracing::info!(
                                "  [{ts}] 📋 FLUSH: restored {added} pending queue item(s) from disk (skipped {skipped} duplicates)"
                            );
                        }

                        // P1-2: Warn about legacy queue files that cannot be restored
                        warn_legacy_pending_queue_files(&provider_for_restore);

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
                                            libsql_rusqlite::params![
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

                        // Suppress durable handoffs so restart does not synthesize a new turn.
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
                        tracing::info!("  [{ts}] ✓ Reconcile complete — intake open");
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
                        tracing::info!("  [{ts}] 🧹 THREAD-MAP: continuing validation in background");
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

                // Background: periodic GC for stale thread sessions in DB.
                // Normal idle/disconnected thread rows expire after 1 hour,
                // but rows still carrying an active_dispatch_id stay until the
                // 3-hour safety TTL so warm-resume sessions keep DB ownership.
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
                    provider: provider_for_setup,
                })
            })
        })
        .build();

    let intents = discord_gateway_intents();

    let mut client = serenity::ClientBuilder::new(token, intents)
        .framework(framework)
        .await
        .expect("Failed to create Discord client");

    let gateway_lease_task = gateway_lease.map(|mut lease| {
        let shared_for_lease = shared.clone();
        let provider_for_lease = provider.clone();
        let shard_manager = client.shard_manager.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(DISCORD_GATEWAY_LEASE_KEEPALIVE_INTERVAL);
            interval.tick().await;
            loop {
                interval.tick().await;

                if shared_for_lease
                    .shutting_down
                    .load(std::sync::atomic::Ordering::SeqCst)
                {
                    let _ = lease.unlock().await;
                    return;
                }

                if let Err(error) = lease.keepalive().await {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::error!(
                        "  [{ts}] ⛔ GATEWAY-LEASE: {} lost singleton lease: {} — self-fencing",
                        provider_for_lease.display_name(),
                        error
                    );

                    shared_for_lease
                        .bot_connected
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    shared_for_lease
                        .shutting_down
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    shared_for_lease
                        .restart_pending
                        .store(true, std::sync::atomic::Ordering::SeqCst);

                    for entry in shared_for_lease.tmux_watchers.iter() {
                        entry
                            .value()
                            .cancel
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }

                    let queue_count =
                        mailbox_restart_drain_all(&shared_for_lease, &provider_for_lease).await;
                    if queue_count > 0 {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 📋 GATEWAY-LEASE: persisted {queue_count} pending queue item(s) before self-fence"
                        );
                    }

                    let ids: std::collections::HashMap<u64, u64> = shared_for_lease
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    if !ids.is_empty() {
                        runtime_store::save_all_last_message_ids(provider_for_lease.as_str(), &ids);
                    }

                    shard_manager.shutdown_all().await;
                    return;
                }
            }
        })
    });

    // Graceful shutdown: on SIGTERM, cancel all tmux watchers before dying
    let shared_for_signal = shared.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};
            if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
                sigterm.recv().await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!("  [{ts}] 🛑 SIGTERM received — graceful shutdown");

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
                    tracing::info!(
                        "  [{ts3}] 📋 mailbox persisted {queue_count} pending queue item(s)"
                    );
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

                // ── Inflight state preservation for silent re-attach ──
                let inflight_states = inflight::load_inflight_states(&provider_for_shutdown);
                if !inflight_states.is_empty() {
                    let ts2 = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts2}] 👁 preserving {} inflight turn(s) for restart recovery",
                        inflight_states.len()
                    );
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
                        tracing::info!(
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
        tracing::warn!("  ✗ {} bot error: {e}", provider_for_error.display_name());
    }

    if let Some(handle) = gateway_lease_task {
        handle.abort();
        let _ = handle.await;
    }
}

/// Periodic GC: delete stale idle/disconnected thread sessions from DB via cleanup API.
async fn gc_stale_thread_sessions_via_api(api_port: u16) {
    let _ = api_port;
    match super::internal_api::gc_stale_thread_sessions().await {
        Ok(gc) if gc > 0 => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] 🧹 GC: removed {gc} stale thread session(s) from DB");
        }
        Ok(_) => {}
        Err(err) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ Thread session GC error: {err}");
        }
    }
}

/// Periodic GC: disconnect stale fixed-channel working sessions from the DB so
/// restart recovery cannot restore dead provider session IDs.
async fn gc_stale_fixed_working_sessions(shared: &Arc<SharedData>) {
    let cleared = if let Some(pool) = shared.pg_pool.as_ref() {
        crate::server::routes::dispatched_sessions::gc_stale_fixed_working_sessions_db_pg(pool)
            .await
    } else {
        let Some(db) = &shared.db else {
            return;
        };

        let conn = match db.lock() {
            Ok(c) => c,
            Err(e) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!("  [{ts}] ⚠ Fixed-session GC lock error: {e}");
                return;
            }
        };
        crate::server::routes::dispatched_sessions::gc_stale_fixed_working_sessions_db(&conn)
    };

    if cleared > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🧹 GC: disconnected {cleared} stale fixed-channel working session(s)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use poise::serenity_prelude::{MessageId, UserId};
    use std::collections::HashSet;
    use std::time::Instant;

    #[test]
    fn restored_fast_mode_channels_restore_for_mixed_provider_runtimes() {
        let mut settings = DiscordBotSettings::default();
        settings.channel_fast_modes.insert("123".to_string(), true);
        settings.channel_fast_modes.insert("456".to_string(), false);

        let claude_channels =
            restored_fast_mode_enabled_channels_for_provider(&settings, &ProviderKind::Claude);
        assert_eq!(claude_channels, vec![ChannelId::new(123)]);

        let gemini_channels =
            restored_fast_mode_enabled_channels_for_provider(&settings, &ProviderKind::Gemini);
        assert_eq!(gemini_channels, vec![ChannelId::new(123)]);
    }

    #[test]
    fn restored_fast_mode_reset_channels_restore_pending_entries_for_all_providers() {
        let mut settings = DiscordBotSettings::default();
        settings.channel_fast_modes.insert("123".to_string(), true);
        settings.channel_fast_modes.insert("456".to_string(), false);
        settings
            .channel_fast_mode_reset_pending
            .insert("codex:456".to_string());
        settings
            .channel_fast_mode_reset_pending
            .insert("123".to_string());

        assert_eq!(
            restored_fast_mode_reset_entries(&settings),
            vec!["123".to_string(), "codex:456".to_string()]
        );
        assert_eq!(
            restored_fast_mode_reset_channels(&settings),
            vec![ChannelId::new(123), ChannelId::new(456)]
        );
    }

    struct PgTestDatabase {
        admin_url: String,
        database_name: String,
    }

    impl PgTestDatabase {
        async fn create() -> Self {
            let admin_url = pg_test_admin_database_url();
            let database_name = format!("agentdesk_runtime_pg_{}", uuid::Uuid::new_v4().simple());
            let admin_pool = sqlx::PgPool::connect(&admin_url)
                .await
                .expect("connect postgres admin db");
            sqlx::query(&format!("CREATE DATABASE \"{database_name}\""))
                .execute(&admin_pool)
                .await
                .expect("create postgres runtime test db");
            admin_pool.close().await;

            Self {
                admin_url,
                database_name,
            }
        }

        async fn drop(self) {
            let admin_pool = sqlx::PgPool::connect(&self.admin_url)
                .await
                .expect("reconnect postgres admin db");
            sqlx::query(
                "SELECT pg_terminate_backend(pid)
                 FROM pg_stat_activity
                 WHERE datname = $1
                   AND pid <> pg_backend_pid()",
            )
            .bind(&self.database_name)
            .execute(&admin_pool)
            .await
            .expect("terminate postgres runtime test db sessions");
            sqlx::query(&format!(
                "DROP DATABASE IF EXISTS \"{}\"",
                self.database_name
            ))
            .execute(&admin_pool)
            .await
            .expect("drop postgres runtime test db");
            admin_pool.close().await;
        }
    }

    fn pg_test_base_database_url() -> String {
        if let Ok(base) = std::env::var("POSTGRES_TEST_DATABASE_URL_BASE") {
            let trimmed = base.trim();
            if !trimmed.is_empty() {
                return trimmed.trim_end_matches('/').to_string();
            }
        }

        let user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        let password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        let host = std::env::var("PGHOST")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "localhost".to_string());
        let port = std::env::var("PGPORT")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "5432".to_string());

        match password {
            Some(password) => format!("postgresql://{user}:{password}@{host}:{port}"),
            None => format!("postgresql://{user}@{host}:{port}"),
        }
    }

    fn pg_test_admin_database_url() -> String {
        let admin_db = std::env::var("POSTGRES_TEST_ADMIN_DB")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "postgres".to_string());
        format!("{}/{}", pg_test_base_database_url(), admin_db)
    }

    fn pg_runtime_test_config(test_db: &PgTestDatabase) -> crate::config::Config {
        let mut config = crate::config::Config::default();
        config.database.enabled = true;
        config.database.pool_max = 4;
        config.database.host = "localhost".to_string();
        config.database.port = std::env::var("PGPORT")
            .ok()
            .and_then(|raw| raw.parse::<u16>().ok())
            .unwrap_or(5432);
        config.database.dbname = test_db.database_name.clone();
        config.database.user = std::env::var("PGUSER")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .or_else(|| {
                std::env::var("USER")
                    .ok()
                    .filter(|value| !value.trim().is_empty())
            })
            .unwrap_or_else(|| "postgres".to_string());
        config.database.password = std::env::var("PGPASSWORD")
            .ok()
            .filter(|value| !value.trim().is_empty());
        config
    }

    fn make_intervention(message_id: u64, source_message_ids: &[u64], text: &str) -> Intervention {
        Intervention {
            author_id: UserId::new(42),
            message_id: MessageId::new(message_id),
            source_message_ids: source_message_ids
                .iter()
                .copied()
                .map(MessageId::new)
                .collect(),
            text: text.to_string(),
            mode: InterventionMode::Soft,
            created_at: Instant::now(),
            reply_context: None,
            has_reply_boundary: false,
            merge_consecutive: true,
        }
    }

    #[test]
    fn enqueue_restored_intervention_keeps_partially_overlapping_merged_item() {
        let mut existing_ids = HashSet::from([100u64]);
        let mut queue = Vec::new();

        assert!(enqueue_restored_intervention(
            &mut existing_ids,
            &mut queue,
            make_intervention(101, &[100, 101], "first\nsecond"),
        ));

        assert_eq!(queue.len(), 1);
        assert!(existing_ids.contains(&100));
        assert!(existing_ids.contains(&101));
    }

    #[test]
    fn enqueue_restored_intervention_skips_only_when_all_ids_are_known() {
        let mut existing_ids = HashSet::from([100u64, 101u64]);
        let mut queue = Vec::new();

        assert!(!enqueue_restored_intervention(
            &mut existing_ids,
            &mut queue,
            make_intervention(101, &[100, 101], "first\nsecond"),
        ));

        assert!(queue.is_empty());
    }

    #[test]
    fn discord_gateway_lock_id_is_stable_for_same_token_hash() {
        let token_hash = "9f86d081884c7d659a2feaa0c55ad015";
        assert_eq!(
            discord_gateway_lock_id(token_hash),
            discord_gateway_lock_id(token_hash)
        );
    }

    #[test]
    fn discord_gateway_lock_id_changes_for_different_token_hashes() {
        let left = discord_gateway_lock_id("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let right = discord_gateway_lock_id("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
        assert_ne!(left, right);
    }

    #[test]
    fn discord_gateway_lock_id_strips_discord_prefix_so_each_bot_gets_unique_id() {
        // `discord_token_hash` produces "discord_<8 bytes hex>" — without stripping
        // the prefix every bot collapses onto the fallback "0" lock id and only the
        // first bot to start can acquire the singleton lease.
        let claude_hash = super::super::settings::discord_token_hash("claude-bot-token-aaa");
        let codex_hash = super::super::settings::discord_token_hash("codex-bot-token-bbb");
        assert!(claude_hash.starts_with("discord_"));
        assert!(codex_hash.starts_with("discord_"));
        let claude_lock = discord_gateway_lock_id(&claude_hash);
        let codex_lock = discord_gateway_lock_id(&codex_hash);
        assert_ne!(claude_lock, codex_lock);
        // Neither should collapse onto the prefix-only fallback (= 0x0443_0000_0000_0000)
        let fallback_lock_id = (DISCORD_GATEWAY_LOCK_PREFIX) as i64;
        assert_ne!(claude_lock, fallback_lock_id);
        assert_ne!(codex_lock, fallback_lock_id);
    }

    #[tokio::test]
    async fn postgres_discord_gateway_lease_allows_only_one_live_runtime_per_token_hash() {
        let test_db = PgTestDatabase::create().await;
        let config = pg_runtime_test_config(&test_db);
        let pool = crate::db::postgres::connect_and_migrate(&config)
            .await
            .expect("connect and migrate postgres")
            .expect("postgres pool");

        let token_hash = "0123456789abcdef0123456789abcdef";
        let mut first = try_acquire_discord_gateway_lease(&pool, token_hash, &ProviderKind::Codex)
            .await
            .expect("acquire first discord gateway lease")
            .expect("first discord gateway lease holder");
        first.keepalive().await.expect("first lease keepalive");

        let second = try_acquire_discord_gateway_lease(&pool, token_hash, &ProviderKind::Codex)
            .await
            .expect("attempt second discord gateway lease");
        assert!(second.is_none(), "same token hash must stay singleton");

        first
            .unlock()
            .await
            .expect("unlock first discord gateway lease");

        let third = try_acquire_discord_gateway_lease(&pool, token_hash, &ProviderKind::Codex)
            .await
            .expect("acquire third discord gateway lease")
            .expect("third discord gateway lease holder");
        third
            .unlock()
            .await
            .expect("unlock third discord gateway lease");

        pool.close().await;
        test_db.drop().await;
    }

    #[tokio::test]
    async fn postgres_discord_gateway_lease_allows_parallel_runtimes_for_different_token_hashes() {
        let test_db = PgTestDatabase::create().await;
        let config = pg_runtime_test_config(&test_db);
        let pool = crate::db::postgres::connect_and_migrate(&config)
            .await
            .expect("connect and migrate postgres")
            .expect("postgres pool");

        let first = try_acquire_discord_gateway_lease(
            &pool,
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            &ProviderKind::Codex,
        )
        .await
        .expect("acquire first discord gateway lease")
        .expect("first discord gateway lease holder");
        let second = try_acquire_discord_gateway_lease(
            &pool,
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            &ProviderKind::Codex,
        )
        .await
        .expect("acquire second discord gateway lease")
        .expect("second discord gateway lease holder");

        second
            .unlock()
            .await
            .expect("unlock second discord gateway lease");
        first
            .unlock()
            .await
            .expect("unlock first discord gateway lease");

        pool.close().await;
        test_db.drop().await;
    }

    #[tokio::test]
    async fn postgres_discord_gateway_lease_fails_over_across_separate_runtime_pools() {
        let test_db = PgTestDatabase::create().await;
        let config = pg_runtime_test_config(&test_db);
        let pool_a = crate::db::postgres::connect_and_migrate(&config)
            .await
            .expect("connect and migrate postgres runtime pool A")
            .expect("postgres runtime pool A");
        let pool_b = crate::db::postgres::connect_and_migrate(&config)
            .await
            .expect("connect and migrate postgres runtime pool B")
            .expect("postgres runtime pool B");

        let token_hash = "feedfacefeedfacefeedfacefeedface";
        let holder_a = try_acquire_discord_gateway_lease(&pool_a, token_hash, &ProviderKind::Codex)
            .await
            .expect("acquire discord gateway lease on runtime pool A")
            .expect("runtime pool A should hold singleton lease");

        let denied_b = try_acquire_discord_gateway_lease(&pool_b, token_hash, &ProviderKind::Codex)
            .await
            .expect("attempt discord gateway lease on runtime pool B");
        assert!(
            denied_b.is_none(),
            "second runtime pool must be fenced while first holder is alive"
        );

        holder_a
            .unlock()
            .await
            .expect("unlock discord gateway lease on runtime pool A");

        let holder_b = try_acquire_discord_gateway_lease(&pool_b, token_hash, &ProviderKind::Codex)
            .await
            .expect("acquire discord gateway lease on runtime pool B after failover")
            .expect("runtime pool B should acquire lease after holder drop");
        holder_b
            .unlock()
            .await
            .expect("unlock discord gateway lease on runtime pool B");

        pool_b.close().await;
        pool_a.close().await;
        test_db.drop().await;
    }
}
