use super::*;
use sqlx::Row as SqlxRow;

mod orphan_recovery;
mod queued_placeholders;
mod restored_state;
mod session_gc;
mod startup_doctor;

#[allow(unused_imports)]
pub(in crate::services::discord) use self::queued_placeholders::{
    FilteredQueuedPlaceholders, StalePlaceholderDeleter, collect_live_queue_message_ids,
    delete_stale_queued_placeholder_cards, delete_stale_queued_placeholder_cards_with,
    filter_restored_queued_placeholders,
};
#[allow(unused_imports)]
use self::{orphan_recovery::*, restored_state::*, session_gc::*, startup_doctor::*};

pub(crate) struct RunBotContext {
    pub(crate) global_active: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) global_finalizing: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) shutdown_remaining: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) startup_reconcile_remaining: Arc<std::sync::atomic::AtomicUsize>,
    pub(crate) startup_doctor_started: Arc<std::sync::atomic::AtomicBool>,
    pub(crate) health_registry: Arc<health::HealthRegistry>,
    pub(crate) api_port: u16,
    pub(crate) pg_pool: Option<sqlx::PgPool>,
    pub(crate) engine: Option<crate::engine::PolicyEngine>,
    pub(crate) placeholder_live_events_enabled: bool,
    pub(crate) status_panel_v2_enabled: bool,
}

const DISCORD_GATEWAY_LEASE_KEEPALIVE_INTERVAL: Duration = Duration::from_secs(15);
const DISCORD_GATEWAY_LOCK_PREFIX: u64 = 0x0443_0000_0000_0000;
static STARTUP_THREAD_MAP_VALIDATION_STARTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn voice_auto_join_provider_map(
    cfg: &crate::config::Config,
) -> std::collections::HashMap<String, (String, Option<String>)> {
    let mut map = std::collections::HashMap::new();
    for agent in &cfg.agents {
        for (slot_provider, channel) in agent.channels.iter() {
            let Some(channel) = channel else { continue };
            let Some(channel_id) = channel.channel_id() else {
                continue;
            };
            let provider = channel
                .provider()
                .unwrap_or_else(|| slot_provider.to_string());
            map.insert(channel_id.to_string(), (provider, None));
        }
        if let Some(voice_channel_id) = agent
            .voice
            .channel_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            let provider = agent
                .voice
                .foreground
                .provider
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .unwrap_or(agent.provider.as_str());
            map.insert(
                voice_channel_id.to_string(),
                (provider.to_string(), Some(agent.id.clone())),
            );
        }
    }
    map
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

fn spawn_startup_thread_map_validation(pg_pool: Option<sqlx::PgPool>, token: String) {
    tokio::spawn(async move {
        let (checked, cleared) =
            crate::services::dispatches::discord_delivery::validate_channel_thread_maps_on_startup_with_backends(
                None,
                pg_pool.as_ref(),
                &token,
            )
            .await;
        if checked > 0 || cleared > 0 {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 THREAD-MAP: validated {checked} mapping(s), cleared {cleared} stale binding(s)"
            );
        }
    });
}

/// Remove the retired durable handoff tree without parsing or executing it.
/// Current recovery paths no longer write these JSON records, so any files here
/// are legacy residue and must not affect boot behavior.
fn purge_legacy_durable_handoffs() {
    let Some(root) = super::runtime_store::legacy_discord_handoff_root() else {
        return;
    };
    if !root.exists() {
        return;
    }
    match std::fs::remove_dir_all(&root) {
        Ok(()) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 Removed retired durable handoff directory: {}",
                root.display()
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ Failed to remove retired durable handoff directory {}: {error}",
                root.display()
            );
        }
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

    let pg_pool = shared.pg_pool.as_ref();
    clear_stale_session_dispatch_links(pg_pool).await;

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
                       AND s.status IN ('turn_active', 'working')
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
        return;
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
    for (dispatch_id, agent_id, card_id, _title, dtype) in &orphans {
        // Clear any existing dispatch_notified marker — the 5-condition query already
        // validated this dispatch is truly orphan, so the marker (if any) is stale.
        {
            let dispatch_notified_key = format!("dispatch_notified:{dispatch_id}");
            if super::internal_api::delete_kv_value(&dispatch_notified_key).is_err() {
                if let Some(pool) = pg_pool {
                    sqlx::query("DELETE FROM kv_meta WHERE key = $1")
                        .bind(&dispatch_notified_key)
                        .execute(pool)
                        .await
                        .ok();
                }
            }
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}]   ↻ Recovering {dtype} dispatch {id} → {agent} (card {card})",
            id = &dispatch_id[..8],
            agent = agent_id,
            card = &card_id[..8.min(card_id.len())],
        );

        let recovery_result = if let Some(pool) = pg_pool {
            crate::db::dispatches::outbox::requeue_dispatch_notify_pg(pool, dispatch_id)
                .await
                .map(|queued| {
                    if !queued {
                        tracing::info!(
                            "  [{}]   · Skipped orphan recovery for {id} (dispatch not requeueable)",
                            chrono::Local::now().format("%H:%M:%S"),
                            id = &dispatch_id[..8],
                        );
                    }
                    queued
                })
        } else {
            continue;
        };
        match recovery_result {
            Ok(true) => {
                delivered += 1;
            }
            Ok(false) => {}
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

async fn clear_stale_session_dispatch_links(pg_pool: Option<&sqlx::PgPool>) {
    let Some(pool) = pg_pool else {
        return;
    };

    match sqlx::query(
        "UPDATE sessions s
            SET status = CASE
                    WHEN s.status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working') THEN 'idle'
                    ELSE s.status
                END,
                active_dispatch_id = NULL,
                session_info = 'Cleared stale terminal dispatch link',
                last_heartbeat = NOW()
           FROM task_dispatches d
          WHERE s.active_dispatch_id = d.id
            AND d.status IN ('completed', 'failed', 'cancelled')
      RETURNING s.session_key, d.id AS dispatch_id, d.status AS dispatch_status",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => {
            if !rows.is_empty() {
                let sample = rows
                    .iter()
                    .take(5)
                    .filter_map(|row| {
                        let session_key = row.try_get::<String, _>("session_key").ok()?;
                        let dispatch_id = row.try_get::<String, _>("dispatch_id").ok()?;
                        let dispatch_status = row.try_get::<String, _>("dispatch_status").ok()?;
                        Some(format!("{session_key}:{dispatch_id}:{dispatch_status}"))
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                tracing::warn!(
                    cleared = rows.len(),
                    sample = %sample,
                    "cleared stale terminal active_dispatch_id links during startup recovery"
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to clear stale terminal active_dispatch_id links during startup recovery"
            );
        }
    }

    match sqlx::query(
        "WITH stale AS (
             SELECT s.session_key
               FROM sessions s
              WHERE s.active_dispatch_id IS NOT NULL
                AND NOT EXISTS (
                    SELECT 1 FROM task_dispatches d WHERE d.id = s.active_dispatch_id
                )
         )
         UPDATE sessions s
            SET status = CASE
                    WHEN s.status IN ('turn_active', 'awaiting_bg', 'awaiting_user', 'working') THEN 'idle'
                    ELSE s.status
                END,
                active_dispatch_id = NULL,
                session_info = 'Cleared missing dispatch link',
                last_heartbeat = NOW()
           FROM stale
          WHERE s.session_key = stale.session_key
      RETURNING s.session_key",
    )
    .fetch_all(pool)
    .await
    {
        Ok(rows) => {
            if !rows.is_empty() {
                let sample = rows
                    .iter()
                    .take(5)
                    .filter_map(|row| row.try_get::<String, _>("session_key").ok())
                    .collect::<Vec<_>>()
                    .join(", ");
                tracing::warn!(
                    cleared = rows.len(),
                    sample = %sample,
                    "cleared missing active_dispatch_id links during startup recovery"
                );
            }
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                "failed to clear missing active_dispatch_id links during startup recovery"
            );
        }
    }
}

pub(super) fn discord_gateway_intents() -> serenity::GatewayIntents {
    serenity::GatewayIntents::GUILDS
        | serenity::GatewayIntents::GUILD_MESSAGES
        | serenity::GatewayIntents::GUILD_MESSAGE_REACTIONS
        | serenity::GatewayIntents::GUILD_VOICE_STATES
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

#[derive(Debug, PartialEq, Eq)]
enum StartupDoctorBarrier {
    Waiting(usize),
    Released,
    AlreadyReleased,
}

fn startup_doctor_barrier_arrive(
    remaining: &std::sync::atomic::AtomicUsize,
    started: &std::sync::atomic::AtomicBool,
) -> StartupDoctorBarrier {
    let mut current = remaining.load(Ordering::Acquire);
    loop {
        if current == 0 {
            return StartupDoctorBarrier::AlreadyReleased;
        }
        let next = current - 1;
        match remaining.compare_exchange(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) if next > 0 => return StartupDoctorBarrier::Waiting(next),
            Ok(_) => {
                return match started.compare_exchange(
                    false,
                    true,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                ) {
                    Ok(_) => StartupDoctorBarrier::Released,
                    Err(_) => StartupDoctorBarrier::AlreadyReleased,
                };
            }
            Err(observed) => current = observed,
        }
    }
}

/// Maximum time the startup_doctor will wait for the local HTTP server to
/// finish binding before it begins running self-probe checks. Without this
/// gate, every fresh boot races the doctor against axum's `bind` call and
/// latches a permanent `unhealthy` artifact via cascading Connection-refused
/// failures (see issue #2096).
const STARTUP_DOCTOR_HTTP_BIND_TIMEOUT: Duration = Duration::from_secs(30);
const STARTUP_DOCTOR_HTTP_BIND_POLL_INTERVAL: Duration = Duration::from_millis(200);
const STARTUP_DOCTOR_HTTP_BIND_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

/// Poll the loopback HTTP server until it accepts a TCP connection or the
/// deadline expires. We deliberately probe the raw TCP bind rather than an
/// HTTP route so this gate is independent of which routes are mounted by the
/// time the doctor wants to run.
async fn wait_for_local_http_bind(api_port: u16) {
    let start = tokio::time::Instant::now();
    let addr = format!("127.0.0.1:{api_port}");
    loop {
        if let Ok(Ok(_stream)) = tokio::time::timeout(
            STARTUP_DOCTOR_HTTP_BIND_PROBE_TIMEOUT,
            tokio::net::TcpStream::connect(&addr),
        )
        .await
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let elapsed_ms = start.elapsed().as_millis();
            tracing::info!("  [{ts}] ✓ startup_doctor http bind ready ({addr}, {elapsed_ms}ms)");
            return;
        }
        if start.elapsed() >= STARTUP_DOCTOR_HTTP_BIND_TIMEOUT {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ startup_doctor http bind not observed within {:?} ({addr}) — running anyway",
                STARTUP_DOCTOR_HTTP_BIND_TIMEOUT
            );
            return;
        }
        tokio::time::sleep(STARTUP_DOCTOR_HTTP_BIND_POLL_INTERVAL).await;
    }
}

async fn run_startup_diagnostic_after_reconcile_barrier(
    remaining: Arc<std::sync::atomic::AtomicUsize>,
    started: Arc<std::sync::atomic::AtomicBool>,
    health_registry: Arc<health::HealthRegistry>,
    api_port: u16,
) {
    match startup_doctor_barrier_arrive(&remaining, &started) {
        StartupDoctorBarrier::Waiting(waiting) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏳ startup_doctor waiting for {waiting} provider reconcile(s)"
            );
            return;
        }
        StartupDoctorBarrier::AlreadyReleased => return,
        StartupDoctorBarrier::Released => {}
    }

    if health_registry.registered_provider_count().await == 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        let startup_doctor = tokio::task::spawn_blocking(|| {
            crate::cli::doctor::startup::record_startup_diagnostic_skipped(
                "no_provider_runtimes_registered",
            )
        })
        .await;
        match startup_doctor {
            Ok(Ok(Some(path))) => {
                tracing::info!(
                    "  [{ts}] ⏭ startup_doctor skipped — no provider runtimes registered; wrote {}",
                    path.display()
                );
            }
            Ok(Ok(None)) => {
                tracing::info!(
                    "  [{ts}] ⏭ startup_doctor skipped — no provider runtimes registered; already recorded for this boot"
                );
            }
            Ok(Err(error)) => {
                tracing::warn!(
                    "  [{ts}] ⚠ startup_doctor skipped but artifact write failed: {error}"
                );
            }
            Err(error) => {
                tracing::warn!(
                    "  [{ts}] ⚠ startup_doctor skipped but artifact task failed: {error}"
                );
            }
        }
        return;
    }

    // #2096: the doctor's `server` / `discord_bot` / `health_*` checks all
    // hit the loopback HTTP server. If we run before axum binds the port we
    // latch six cascading Connection-refused failures into the artifact and
    // every subsequent `/api/health` call returns 503 until the next boot.
    wait_for_local_http_bind(api_port).await;

    let startup_doctor =
        tokio::task::spawn_blocking(crate::cli::doctor::startup::run_startup_diagnostic_once).await;
    match startup_doctor {
        Ok(Ok(Some(path))) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ✓ startup_doctor wrote {}", path.display());
        }
        Ok(Ok(None)) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] ✓ startup_doctor already recorded for this boot");
        }
        Ok(Err(error)) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ startup_doctor_failed: {error}");
        }
        Err(error) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!("  [{ts}] ⚠ startup_doctor_failed: {error}");
        }
    }
}

/// Entry point: start the Discord bot
pub(crate) async fn run_bot(token: &str, provider: ProviderKind, context: RunBotContext) {
    let RunBotContext {
        global_active,
        global_finalizing,
        shutdown_remaining,
        startup_reconcile_remaining,
        startup_doctor_started,
        health_registry,
        api_port,
        pg_pool,
        engine,
        placeholder_live_events_enabled,
        status_panel_v2_enabled,
    } = context;

    if let Some(bot_name) = should_skip_agent_runtime_launch(token) {
        run_startup_diagnostic_after_reconcile_barrier(
            startup_reconcile_remaining,
            startup_doctor_started,
            health_registry,
            api_port,
        )
        .await;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ⏭ BOT-LAUNCH: skipping utility bot '{}' in run_bot() — not mapped to any agent channel",
            bot_name
        );
        shutdown_remaining.fetch_sub(1, Ordering::AcqRel);
        return;
    }

    let token_hash = settings::discord_token_hash(token);

    // Phase 5.1 of intake-node-routing (issue #2007): build SharedData and
    // spawn the intake_worker poll loop BEFORE the gateway lease check.
    // Standby nodes (lease held elsewhere) still need a live worker to
    // claim `intake_outbox` rows targeted at this `instance_id` — that is
    // the entire point of routing intake to a worker node. Previously the
    // worker spawn lived inside the poise setup callback, which only
    // executes on the lease-holding leader, so standby workers never
    // started.
    super::internal_api::init(api_port, pg_pool.clone());

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

    let voice_config = crate::config::load_graceful().voice;
    let voice_barge_in = Arc::new(voice_barge_in::VoiceBargeInRuntime::from_voice_config(
        &voice_config,
    ));

    run_bot_rehydrate_voice_handoffs(&pg_pool).await;

    // Cleanup stale Discord uploads on process start
    cleanup_old_uploads(UPLOAD_MAX_AGE);

    let provider_for_shutdown = provider.clone();
    let provider_for_error = provider.clone();
    let provider_for_framework = provider.clone();
    let startup_reconcile_remaining_for_client_start = startup_reconcile_remaining.clone();
    let startup_doctor_started_for_client_start = startup_doctor_started.clone();
    let health_registry_for_client_start = health_registry.clone();

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
    let restored_codex_goals_channels = restored_codex_goals_enabled_channels(&bot_settings);
    let restored_codex_goals_reset_channels = restored_codex_goals_reset_channels(&bot_settings);

    let shared = run_bot_build_shared_data(
        bot_settings,
        initial_skills,
        &provider,
        &token_hash,
        &voice_barge_in,
        global_active,
        global_finalizing,
        &shutdown_remaining,
        &health_registry,
        pg_pool,
        engine,
        api_port,
        placeholder_live_events_enabled,
        status_panel_v2_enabled,
        &restored_model_overrides,
        &restored_fast_mode_channels,
        &restored_fast_mode_reset_entries,
        &restored_fast_mode_reset_channels,
        &restored_codex_goals_channels,
        &restored_codex_goals_reset_channels,
    );
    super::tui_prompt_relay::spawn_tui_prompt_relay(shared.clone(), provider.clone());

    // Phase 5.2 of intake-node-routing (issue #2009): populate
    // `cached_bot_token` BEFORE the gateway lease check so the
    // standby-side response path (`turn_bridge` tmux watcher,
    // placeholder edits) can build a REST `Arc<Http>` via
    // `shared.serenity_http_or_token_fallback()` even when
    // `cached_serenity_ctx` stays empty (no gateway runtime).
    //
    // On the leader the OnceCell is also set later inside the poise
    // setup callback — that second `set` is a no-op (`OnceCell::set`
    // returns Err on already-set), preserving the leader's existing
    // semantics.
    let _ = shared.cached_bot_token.set(token.to_string());

    let voice_receiver =
        run_bot_init_voice_workers(&voice_config, &voice_barge_in, &shared, &provider);

    // Phase 5.1 of intake-node-routing (issue #2007): only spawn the
    // intake_worker poll loop when routing is explicitly enforced. In the
    // default disabled/observe modes there are no owned rows to drain, and
    // starting one poller per configured Discord agent can exhaust the shared
    // Postgres pool before the HTTP server finishes booting.
    //
    // The worker uses `serenity::http::Http::new(token)` (REST-only,
    // no IDENTIFY) so it never contends for the gateway lease. It
    // only touches `shared.{core, settings, pg_pool, dispatch_thread_parents}`,
    // none of which depend on a live gateway shard.
    //
    // Cancellation rides on `shared.shutting_down`. On the leader, the
    // gateway-lease loss handler and SIGTERM handler flip that flag.
    // On standby today no signal handler is wired; the worker exits
    // when launchd kills the process during deploy. A follow-up could
    // add SIGTERM handling on the standby path for graceful drain.
    run_bot_maybe_spawn_intake_worker(&shared, token, &provider);

    // After optional worker setup, do the gateway lease check. Standby nodes
    // (lease held elsewhere) early-return below; when intake routing is
    // enforced, the detached worker task keeps polling using `Arc<SharedData>`.
    let gateway_lease = match run_bot_acquire_gateway_lease(
        &shared,
        &token_hash,
        &provider,
        &startup_reconcile_remaining,
        &startup_doctor_started,
        &health_registry,
        api_port,
    )
    .await
    {
        GatewayLeaseOutcome::Proceed(lease) => lease,
        GatewayLeaseOutcome::Skip => {
            // Standby / lease-held-elsewhere / acquire-error: the diagnostic
            // already ran inside the helper. Decrement the shutdown barrier
            // and abort startup exactly as the original early-returns did.
            shutdown_remaining.fetch_sub(1, Ordering::AcqRel);
            return;
        }
    };

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
    let voice_config_for_setup = voice_config.clone();
    let voice_receiver_for_setup = voice_receiver.clone();

    let slash_commands = run_bot_build_slash_commands();

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
            let voice_config_for_setup = voice_config_for_setup.clone();
            let voice_receiver_for_setup = voice_receiver_for_setup.clone();
            Box::pin(async move {
                // Register in each guild for instant slash command propagation
                // (register_globally can take up to 1 hour)
                let commands = &framework.options().commands;
                // Populate known slash command names for router fallback logic
                let cmd_names: std::collections::HashSet<String> =
                    commands.iter().map(|c| c.name.clone()).collect();
                let _ = shared_for_migrate
                    .known_slash_commands
                    .set(cmd_names.clone());
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
                audit_or_prune_global_slash_commands(ctx, cmd_names.clone()).await;
                tracing::info!(
                    "  ✓ Bot connected — Registered commands in {} guild(s)",
                    _ready.guilds.len()
                );
                shared_for_migrate
                    .bot_connected
                    .store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = shared_for_migrate.cached_serenity_ctx.set(ctx.clone());
                let _ = shared_for_migrate.cached_bot_token.set(token_for_ready.clone());
                super::drain_pending_queue_exit_placeholder_clears(&shared_for_migrate).await;
                health_registry_for_setup
                    .register_http(provider_for_setup.as_str().to_string(), ctx.http.clone())
                    .await;

                // Enrich role_map.json with channelId for reliable name→ID resolution
                enrich_role_map_with_channel_ids();

                let shared_for_tmux = shared_for_migrate.clone();

                // Background: poll for deferred restart marker when idle
                run_bot_spawn_deferred_restart_poller(&shared_for_tmux, &provider_for_setup);

                // (Phase 5.1 of intake-node-routing — issue #2007: the
                // intake_worker poll loop is now spawned in `run_bot()`
                // before the gateway lease check, so cluster-standby
                // nodes also drain their share of `intake_outbox`. No
                // worker bootstrap belongs here anymore.)

                // Background: hot-reload skills on file changes (30s polling)
                // Scans home-level AND all active project-level skill directories.
                run_bot_spawn_skills_hot_reload(&shared_for_tmux, &provider_for_setup);

                // #799: MCP credential watcher (Claude only).
                // Watches ~/.claude/.credentials.json and ~/.claude/mcp.json and posts
                // a one-line notification to all active Claude sessions when one of
                // them changes, so the operator can run /restart to pick up
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
                run_bot_spawn_recovery_and_flush_restart_reports(
                    ctx,
                    &shared_for_tmux,
                    &token_owned,
                    &provider_for_setup,
                    &startup_reconcile_remaining,
                    &startup_doctor_started,
                    &health_registry_for_setup,
                    api_port,
                );

                // Background: periodic cleanup for stale Discord upload files
                run_bot_spawn_upload_cleanup();

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
                super::idle_detector::spawn_idle_detector(
                    shared_clone.clone(),
                    provider_for_setup.clone(),
                );

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
                run_bot_spawn_stale_session_gc(&shared_clone);

                run_bot_spawn_voice_auto_join(
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
            })
        })
        .build();

    let intents = discord_gateway_intents();

    let client = commands::register_songbird(serenity::ClientBuilder::new(token, intents))
        .framework(framework)
        .await
        .expect("Failed to create Discord client");

    let gateway_lease_task = gateway_lease.map(|lease| {
        run_bot_spawn_gateway_lease_keepalive(
            lease,
            &shared,
            &provider,
            client.shard_manager.clone(),
        )
    });

    // Graceful shutdown: on SIGTERM, persist queue/inflight/last_message state
    // and quick-exit. tmux/TUI processes survive — the next dcserver instance
    // rehydrates the channel bindings (see rehydrate_existing_claude_tui_bindings;
    // polled every CLAUDE_IDLE_REHYDRATE_POLL_INTERVAL ≈ 5s) and resumes transcript
    // tailing from the persisted last_offset.
    run_bot_spawn_sigterm_handler(&shared, provider_for_shutdown);

    run_bot_run_gateway_backend(
        client,
        &provider_for_error,
        gateway_lease_task,
        startup_reconcile_remaining_for_client_start,
        startup_doctor_started_for_client_start,
        health_registry_for_client_start,
        api_port,
    )
    .await;
}

/// Background: poll for the deferred restart marker when idle (leader-only).
/// Behavior-preserving extraction of the inline spawn from run_bot's setup
/// callback. Both clones are used only inside the spawn; the JoinHandle is
/// discarded exactly as the inline code did.
fn run_bot_spawn_deferred_restart_poller(
    shared_for_tmux: &Arc<SharedData>,
    provider_for_setup: &ProviderKind,
) {
    let shared_for_deferred = shared_for_tmux.clone();
    let provider_for_deferred = provider_for_setup.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(DEFERRED_RESTART_POLL_INTERVAL).await;
            // Quick-exit restart (#2713): dcserver no longer waits
            // for all active turns to drain. The marker is a deploy
            // request to persist cheap local state and exit; managed
            // TUI/tmux sessions survive and the next process
            // rehydrates transcript tailing from runtime state.
            if let Some(root) = crate::agentdesk_runtime_root() {
                let marker = root.join("restart_pending");
                if marker.exists() {
                    shared_for_deferred
                        .restart_pending
                        .store(true, Ordering::SeqCst);
                    shared_for_deferred
                        .shutting_down
                        .store(true, Ordering::SeqCst);
                    if shared_for_deferred
                        .shutdown_counted
                        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
                        .is_err()
                    {
                        continue;
                    }
                    let drain =
                        mailbox_restart_drain_all(&shared_for_deferred, &provider_for_deferred)
                            .await;
                    let queue_count = drain.queued_count;
                    if !drain.persistence_errors.is_empty() {
                        tracing::error!(
                            failures = drain.persistence_errors.len(),
                            "restart_pending quick exit continuing after pending-queue persistence failure(s)"
                        );
                    }
                    let ids: std::collections::HashMap<u64, u64> = shared_for_deferred
                        .last_message_ids
                        .iter()
                        .map(|entry| (entry.key().get(), *entry.value()))
                        .collect();
                    if !ids.is_empty() {
                        runtime_store::save_all_last_message_ids(
                            provider_for_deferred.as_str(),
                            &ids,
                        );
                    }
                    // Quick-exit must preserve inflight state with
                    // bumped mtime + DrainRestart marker. Without
                    // this, repeated quick-exits (e.g. destructive
                    // E2E scenarios that restart release multiple
                    // times) leave file mtime frozen at first save,
                    // and stale-removal trips after 1800s even
                    // while the tmux pane is still alive. Mirrors
                    // the graceful-shutdown preserve block below.
                    let inflight_states_qe = inflight::load_inflight_states(&provider_for_deferred);
                    if !inflight_states_qe.is_empty() {
                        let ts2 = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts2}] 👁 preserving {} inflight turn(s) for restart recovery",
                            inflight_states_qe.len()
                        );
                        let marked_qe = inflight::mark_all_inflight_states_restart_mode(
                            &provider_for_deferred,
                            crate::services::discord::InflightRestartMode::DrainRestart,
                        );
                        tracing::info!(
                            "  [{ts2}] 🔖 marked {marked_qe} inflight turn(s) as drain_restart"
                        );
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 🔄 restart_pending detected — quick exit after persisting {queue_count} queued item(s)"
                    );
                    if shared_for_deferred
                        .shutdown_remaining
                        .fetch_sub(1, Ordering::AcqRel)
                        == 1
                    {
                        let _ = std::fs::remove_file(&marker);
                        std::process::exit(0);
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
                let drain =
                    mailbox_restart_drain_all(&shared_for_deferred, &provider_for_deferred).await;
                let queue_count = drain.queued_count;
                if !drain.persistence_errors.is_empty() {
                    tracing::error!(
                        failures = drain.persistence_errors.len(),
                        "deferred restart observed pending-queue persistence failure(s)"
                    );
                }
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
}

/// Background: hot-reload skills on file changes (30s polling). Scans
/// home-level AND all active project-level skill directories. Behavior-
/// preserving extraction; JoinHandle discarded as inline.
fn run_bot_spawn_skills_hot_reload(
    shared_for_tmux: &Arc<SharedData>,
    provider_for_setup: &ProviderKind,
) {
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
            let fp = skill_dir_fingerprint_with_projects(&provider_for_skills, &project_paths);
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
}

/// Restore inflight turns FIRST, then flush restart reports (leader-only).
/// Recovery skips channels that have a pending restart report, so the report
/// must still be on disk when recovery runs. After recovery completes, the
/// flush loop starts and delivers/clears reports. Behavior-preserving
/// extraction; JoinHandle discarded as inline. `api_port` is captured by the
/// spawn (used by run_startup_diagnostic_after_reconcile_barrier).
fn run_bot_spawn_recovery_and_flush_restart_reports(
    ctx: &serenity::Context,
    shared_for_tmux: &Arc<SharedData>,
    token_owned: &str,
    provider_for_setup: &ProviderKind,
    startup_reconcile_remaining: &Arc<std::sync::atomic::AtomicUsize>,
    startup_doctor_started: &Arc<std::sync::atomic::AtomicBool>,
    health_registry_for_setup: &Arc<health::HealthRegistry>,
    api_port: u16,
) {
    let http_for_tmux = ctx.http.clone();
    let shared_for_tmux2 = shared_for_tmux.clone();
    let http_for_restart_reports = ctx.http.clone();
    let ctx_for_kickoff = ctx.clone();
    let token_for_kickoff = token_owned.to_string();
    let shared_for_restart_reports = shared_for_tmux.clone();
    let provider_for_restore = provider_for_setup.clone();
    let startup_reconcile_remaining_for_restore = startup_reconcile_remaining.clone();
    let startup_doctor_started_for_restore = startup_doctor_started.clone();
    let health_registry_for_startup_doctor = health_registry_for_setup.clone();
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
            catch_up_missed_messages(&http_for_tmux, &shared_for_tmux2, &provider_for_restore)
                .await;

            gc_stale_fixed_working_sessions(&shared_for_tmux2).await;

            // Restore pending intervention queues saved during previous SIGTERM
            // before inflight turn recovery. Drain-mode queue snapshots are the
            // source of truth for restart-gap user input; if inflight recovery
            // recreates an active turn first, the active message id can make a
            // persisted queue item look "already known" and incorrectly drop it.
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
                    resolve_runtime_channel_binding_status(&http_for_tmux, *thread_channel_id)
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
                let mut skipped_unowned = 0usize;
                let mut skipped_sender = 0usize;
                let mut skipped_duplicate = 0usize;
                for (channel_id, items) in restored_queues {
                    if !matches!(
                        resolve_runtime_channel_binding_status(&http_for_tmux, channel_id).await,
                        RuntimeChannelBindingStatus::Owned
                    ) {
                        skipped_unowned += items.len();
                        continue;
                    }
                    let snapshot = mailbox_snapshot(&shared_for_tmux2, channel_id).await;
                    let mut existing_ids = queued_message_ids(&snapshot);
                    let mut queue = snapshot.intervention_queue;
                    for item in items {
                        if !super::is_allowed_turn_sender(
                            &allowed_bot_ids_for_restore,
                            announce_bot_id_for_restore,
                            item.author_id.get(),
                            item.author_is_bot,
                            &item.text,
                        ) {
                            skipped_sender += 1;
                            continue;
                        }
                        if enqueue_restored_intervention(&mut existing_ids, &mut queue, item) {
                            added += 1;
                        } else {
                            skipped_duplicate += 1;
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
                let skipped = skipped_unowned + skipped_sender + skipped_duplicate;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 📋 FLUSH: restored {added} pending queue item(s) from disk (skipped {skipped}: unowned={skipped_unowned}, sender={skipped_sender}, duplicate={skipped_duplicate})"
                );
            }

            // codex review round-3 P2 (#1332): restore the
            // `queued_placeholders` mapping from disk BEFORE
            // `kickoff_idle_queues` so the restored mailbox queue
            // entries pick up the existing `📬 메시지 대기 중`
            // Discord cards instead of stranding them and posting
            // duplicate placeholders. Must run AFTER the mailbox
            // queue is restored (above) and BEFORE
            // `kickoff_idle_queues` / `restore_inflight_turns` so
            // the live-queue filter (round-6 P2) can reject any
            // mapping whose source message id is no longer in any
            // currently-queued intervention.
            // codex review round-7 P2 (#1332): collect stale
            // `📬` card tuples during the filter pass and call
            // `delete_message` on each AFTER `kickoff_idle_queues`
            // returns. Inline deletion before kickoff would
            // gate startup intake on per-card HTTP latency
            // (and surface 404s for cards posted by an old
            // bot identity). Best-effort, post-kickoff is
            // strictly safer.
            let mut stale_cards_to_delete: Vec<(ChannelId, MessageId, MessageId)> = Vec::new();
            let restored_queued_placeholders =
                super::queued_placeholders_store::load_queued_placeholders(
                    &provider_for_restore,
                    &shared_for_tmux2.token_hash,
                );
            if !restored_queued_placeholders.is_empty() {
                // codex review round-6 P2 (#1332): when startup
                // skips/supersedes a restored or catch-up queue
                // item before this point (channel no longer
                // owned, sender no longer allowed, duplicate or
                // cap pruning, …), its persisted queued-
                // placeholder mapping has no live queue entry to
                // attach to. Inserting it unconditionally would
                // strand the `📬` card + sidecar row forever:
                // no future dispatch or queue-exit event would
                // reference that user message id. Filter the
                // loaded mappings against the live mailbox queue
                // and DELETE the on-disk + in-memory state for
                // every mapping whose user message id is no
                // longer queued.
                let live_queue_ids = collect_live_queue_message_ids(&shared_for_tmux2).await;
                let filter_outcome = filter_restored_queued_placeholders(
                    restored_queued_placeholders,
                    &live_queue_ids,
                );
                for (key, placeholder_msg_id) in &filter_outcome.live {
                    shared_for_tmux2
                        .queued
                        .queued_placeholders
                        .insert(*key, *placeholder_msg_id);
                }
                // Re-snapshot every channel that had at least
                // one stale mapping pruned so the on-disk file
                // matches the filtered in-memory state. Empty
                // channels are removed via the snapshot helper
                // (the `entries.is_empty()` branch deletes the
                // file). Without this rewrite, the next restart
                // would re-load the same stale mapping and the
                // leak would compound across restarts.
                for channel_id in &filter_outcome.channels_with_stale {
                    super::queued_placeholders_store::persist_channel_from_map(
                        &shared_for_tmux2.queued.queued_placeholders,
                        &shared_for_tmux2.provider,
                        &shared_for_tmux2.token_hash,
                        *channel_id,
                    );
                }
                let live_count = filter_outcome.live.len();
                let stale_count = filter_outcome.stale_count;
                let ts = chrono::Local::now().format("%H:%M:%S");
                if stale_count > 0 {
                    tracing::warn!(
                        "  [{ts}] 📋 FLUSH: restored {live_count} queued-placeholder mapping(s) from disk; pruned {stale_count} stale mapping(s) with no live queue entry"
                    );
                } else {
                    tracing::info!(
                        "  [{ts}] 📋 FLUSH: restored {live_count} queued-placeholder mapping(s) from disk"
                    );
                }
                // codex review round-7 P2 (#1332): capture
                // the visible-card tuples so the post-kickoff
                // cleanup loop can dismiss them via Discord's
                // delete_message API. Without this, the
                // round-6 disk-rewrite leaves the cards
                // stranded on the channel.
                stale_cards_to_delete = filter_outcome.stale_cards;
            }

            // #2437 (#2427 C wire) boot-time generation
            // invalidate. Remove non-planned-restart inflight
            // rows whose `restart_generation` does not match
            // the current generation so recovery does not
            // revive a row whose tmux session no longer
            // exists. Must run BEFORE `restore_inflight_turns`
            // — otherwise recovery would attempt to revive
            // ghost rows and the placeholder sweeper would
            // eventually have to time-guess them at 1800s.
            // Planned-restart / hot-swap rows survive (their
            // generation gate in `stale_removal_reason`
            // already handles them with longer retention).
            let invalidated = super::inflight::invalidate_stale_generation(
                &provider_for_restore,
                shared_for_tmux2.current_generation,
            );
            if invalidated > 0 {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🧹 inflight: invalidated {} stale-generation row(s) for {} (current generation {}) — #2437",
                    invalidated,
                    provider_for_restore.as_str(),
                    shared_for_tmux2.current_generation,
                );
            }

            restore_inflight_turns(&http_for_tmux, &shared_for_tmux2, &provider_for_restore).await;

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
                let recovery_channels: Vec<u64> = shared_for_tmux2
                    .recovering_channels
                    .iter()
                    .map(|entry| entry.key().get())
                    .collect();
                super::tmux::store_recovery_handled_channels(&shared_for_tmux2, &recovery_channels)
                    .await;

                restore_tmux_watchers(&http_for_tmux, &shared_for_tmux2).await;
                cleanup_orphan_tmux_sessions(&shared_for_tmux2).await;

                // Clean up recovery markers
                super::tmux::clear_recovery_handled_channels(&shared_for_tmux2).await;
            }

            // Remove retired durable handoffs so stale legacy JSON cannot
            // influence startup.
            purge_legacy_durable_handoffs();

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

            // codex review round-7 P2 (#1332): now that the
            // gateway has had a chance to settle and live
            // queues have been kicked off, best-effort
            // delete any `📬 메시지 대기 중` Discord cards
            // whose mapping the round-6 filter pruned.
            // Without this loop the cards stay forever (the
            // owning mapping was just removed, so no future
            // dispatch / queue-exit event can reach them).
            delete_stale_queued_placeholder_cards(&http_for_tmux, &stale_cards_to_delete).await;

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
        if shared_for_tmux2.pg_pool.is_some()
            && STARTUP_THREAD_MAP_VALIDATION_STARTED
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
        {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!("  [{ts}] 🧹 THREAD-MAP: continuing validation in background");
            spawn_startup_thread_map_validation(
                shared_for_tmux2.pg_pool.clone(),
                token_for_kickoff.clone(),
            );
        }

        run_startup_diagnostic_after_reconcile_barrier(
            startup_reconcile_remaining_for_restore,
            startup_doctor_started_for_restore,
            health_registry_for_startup_doctor,
            api_port,
        )
        .await;

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
}

/// Background: periodic cleanup for stale Discord upload files. No captures;
/// behavior-preserving extraction.
fn run_bot_spawn_upload_cleanup() {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(UPLOAD_CLEANUP_INTERVAL).await;
            cleanup_old_uploads(UPLOAD_MAX_AGE);
        }
    });
}

/// Background: periodic GC for stale thread sessions in DB. Normal
/// idle/disconnected thread rows expire after 1 hour, but rows still carrying
/// an active_dispatch_id stay until the 3-hour safety TTL so warm-resume
/// sessions keep DB ownership. Behavior-preserving extraction.
fn run_bot_spawn_stale_session_gc(shared_clone: &Arc<SharedData>) {
    let shared_for_session_gc = shared_clone.clone();
    tokio::spawn(async move {
        // Run every 10 minutes, initial delay 2 minutes
        tokio::time::sleep(tokio::time::Duration::from_secs(120)).await;
        loop {
            gc_stale_fixed_working_sessions(&shared_for_session_gc).await;
            gc_stale_thread_sessions(&shared_for_session_gc).await;
            tokio::time::sleep(tokio::time::Duration::from_secs(600)).await;
        }
    });
}

/// Auto-join configured voice channels (leader-only). The enable/non-empty
/// guard lives inside so the call site is unconditional. Async because it
/// reads `shared_clone.settings` before spawning. Behavior-preserving
/// extraction; the await point matches the inline block exactly.
async fn run_bot_spawn_voice_auto_join(
    ctx: &serenity::Context,
    voice_config_for_setup: &crate::voice::VoiceConfig,
    voice_receiver_for_setup: &crate::voice::VoiceReceiver,
    shared_clone: &Arc<SharedData>,
    provider_for_setup: &ProviderKind,
) {
    if voice_config_for_setup.enabled
        && !voice_config_for_setup
            .auto_join_channel_ids_with_lobby()
            .is_empty()
    {
        let ctx_for_voice = ctx.clone();
        let receiver_for_voice = voice_receiver_for_setup.clone();
        let config_for_voice = voice_config_for_setup.clone();
        let barge_in_for_voice = shared_clone.voice_barge_in.clone();
        let pairings_for_voice = shared_clone.voice_pairings.clone();
        let provider_for_voice = provider_for_setup.clone();
        let agent_for_voice = {
            let settings = shared_clone.settings.read().await;
            settings.agent.clone()
        };
        // #2054 v7: agent voice binding은 channel_id → provider+agent
        // 매핑을 build 해서 같은 provider의 다른 에이전트 봇까지
        // 같은 voice 채널에 진입하는 중복 STT/TTS를 차단한다.
        let cfg = crate::config::load_graceful();
        let channel_provider_map = voice_auto_join_provider_map(&cfg);
        tokio::spawn(async move {
            commands::auto_join_voice_channels(
                ctx_for_voice,
                receiver_for_voice,
                config_for_voice,
                barge_in_for_voice,
                pairings_for_voice,
                provider_for_voice,
                agent_for_voice,
                channel_provider_map,
            )
            .await;
        });
    }
}

// ── run_bot startup-phase helpers (decomposition of the run_bot
// god-function, issue #3038). These are behavior-preserving extractions:
// each helper runs the exact statements it replaced, in the same order,
// and run_bot calls them in the same order with the same threaded state.
// INITIALIZATION/SPAWN ORDER IS LOAD-BEARING — do not reorder. ──

/// #2274: rehydrate the process-local voice-background handoff store from the
/// durable PG side table. Best-effort — a PG error is logged and the
/// terminal-delivery path falls back to a per-call `load_handoff_durable`
/// lookup. Runs early in run_bot, before upload cleanup and SharedData build.
async fn run_bot_rehydrate_voice_handoffs(pg_pool: &Option<sqlx::PgPool>) {
    if let Some(pool) = pg_pool.as_ref() {
        match crate::voice::announce_meta::rehydrate_handoffs_from_pg(pool).await {
            Ok(count) => {
                if count > 0 {
                    tracing::info!(
                        rehydrated = count,
                        "voice_background_handoff_meta rehydrated from durable PG store"
                    );
                } else {
                    tracing::debug!("voice_background_handoff_meta rehydrate found no live rows");
                }
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "voice_background_handoff_meta rehydrate failed; terminal-delivery will fall back to per-call durable load"
                );
            }
        }
    }
}

/// Outcome of the gateway singleton-lease acquisition phase.
enum GatewayLeaseOutcome {
    /// Either the lease was acquired (`Some`) or there is no PG pool (`None`,
    /// the standalone/no-DB path). Either way, startup proceeds.
    Proceed(Option<crate::db::postgres::AdvisoryLockLease>),
    /// Lease is held elsewhere, or acquisition failed. The startup diagnostic
    /// has already run; run_bot must decrement the shutdown barrier and return.
    Skip,
}

/// Build all owned `SharedData` fields and wrap in an `Arc`. Side-effecting
/// initializers (`TurnFinalizer::spawn`, `StatusPanelController::spawn`,
/// `runtime_store::load_generation`, `load_queue_exit_placeholder_clears`,
/// the `inflight_signals` broadcast channel) run here in the exact same order
/// as the original inline struct literal. `bot_settings`, `initial_skills`,
/// `global_active`, `global_finalizing`, `pg_pool`, and `engine` are consumed
/// by move; the `restored_*` slices are borrowed (they are reused later in
/// run_bot for logging and session-reset bootstrap).
#[allow(clippy::too_many_arguments)]
fn run_bot_build_shared_data(
    bot_settings: DiscordBotSettings,
    initial_skills: Vec<(String, String)>,
    provider: &ProviderKind,
    token_hash: &str,
    voice_barge_in: &Arc<voice_barge_in::VoiceBargeInRuntime>,
    global_active: Arc<std::sync::atomic::AtomicUsize>,
    global_finalizing: Arc<std::sync::atomic::AtomicUsize>,
    shutdown_remaining: &Arc<std::sync::atomic::AtomicUsize>,
    health_registry: &Arc<health::HealthRegistry>,
    pg_pool: Option<sqlx::PgPool>,
    engine: Option<crate::engine::PolicyEngine>,
    api_port: u16,
    placeholder_live_events_enabled: bool,
    status_panel_v2_enabled: bool,
    restored_model_overrides: &[(ChannelId, String)],
    restored_fast_mode_channels: &[ChannelId],
    restored_fast_mode_reset_entries: &[String],
    restored_fast_mode_reset_channels: &[ChannelId],
    restored_codex_goals_channels: &[ChannelId],
    restored_codex_goals_reset_channels: &[ChannelId],
) -> Arc<SharedData> {
    Arc::new(SharedData {
        core: Mutex::new(CoreState {
            sessions: HashMap::new(),
            active_meetings: HashMap::new(),
        }),
        mailboxes: ChannelMailboxRegistry::default(),
        settings: tokio::sync::RwLock::new(bot_settings),
        api_timestamps: dashmap::DashMap::new(),
        skills_cache: tokio::sync::RwLock::new(initial_skills),
        tmux_watchers: super::TmuxWatcherRegistry::new(),
        tmux_relay_coords: dashmap::DashMap::new(),
        placeholder_cleanup: Arc::new(
            super::placeholder_cleanup::PlaceholderCleanupRegistry::default(),
        ),
        placeholder_controller: Arc::new(
            super::placeholder_controller::PlaceholderController::default(),
        ),
        placeholder_live_events: Arc::new(
            super::placeholder_live_events::PlaceholderLiveEvents::default(),
        ),
        placeholder_live_events_enabled,
        status_panel_v2_enabled,
        // #3038 S1: wrapped verbatim at the first-member position (evaluation-order preserved).
        queued: QueuedPlaceholderState {
            queued_placeholders: dashmap::DashMap::new(),
            queue_exit_placeholder_clears: {
                let map = dashmap::DashMap::new();
                for (key, placeholder_msg_id) in
                    super::queued_placeholders_store::load_queue_exit_placeholder_clears(
                        provider, token_hash,
                    )
                {
                    map.insert(key, placeholder_msg_id);
                }
                map
            },
            queued_placeholders_persist_locks: dashmap::DashMap::new(),
        },
        answer_flush_barrier: std::sync::Arc::new(
            super::answer_flush_barrier::AnswerFlushBarrier::default(),
        ),
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
        turn_finalizer: super::turn_finalizer::TurnFinalizer::spawn(),
        status_panel_controller: super::status_panel_controller::StatusPanelController::spawn(
            status_panel_v2_enabled,
        ),
        global_finalizing,
        shutdown_remaining: shutdown_remaining.clone(),
        shutdown_counted: std::sync::atomic::AtomicBool::new(false),
        intake_dedup: dashmap::DashMap::new(),
        dispatch_thread_parents: dashmap::DashMap::new(),
        bot_connected: std::sync::atomic::AtomicBool::new(false),
        last_turn_at: std::sync::Mutex::new(None),
        model_overrides: {
            let map = dashmap::DashMap::new();
            for (channel_id, model) in restored_model_overrides {
                map.insert(*channel_id, model.clone());
            }
            map
        },
        fast_mode_channels: {
            let set = dashmap::DashSet::new();
            for channel_id in restored_fast_mode_channels {
                set.insert(*channel_id);
            }
            set
        },
        fast_mode_session_reset_pending: {
            let set = dashmap::DashSet::new();
            for entry in restored_fast_mode_reset_entries {
                set.insert(entry.clone());
            }
            set
        },
        codex_goals_channels: {
            let set = dashmap::DashSet::new();
            for channel_id in restored_codex_goals_channels {
                set.insert(*channel_id);
            }
            set
        },
        codex_goals_session_reset_pending: {
            let set = dashmap::DashSet::new();
            for channel_id in restored_codex_goals_reset_channels {
                set.insert(*channel_id);
            }
            set
        },
        model_session_reset_pending: dashmap::DashSet::new(),
        session_reset_pending: bootstrap_session_reset_pending_channels(
            restored_model_overrides,
            restored_fast_mode_reset_channels,
            restored_codex_goals_reset_channels,
        ),
        model_picker_pending: dashmap::DashMap::new(),
        dispatch_role_overrides: dashmap::DashMap::new(),
        voice_barge_in: voice_barge_in.clone(),
        voice_pairings: Arc::new(voice_routing::VoiceChannelPairingStore::load_default()),
        last_message_ids: dashmap::DashMap::new(),
        catch_up_retry_pending: dashmap::DashMap::new(),
        turn_start_times: dashmap::DashMap::new(),
        channel_rosters: dashmap::DashMap::new(),
        cached_serenity_ctx: tokio::sync::OnceCell::new(),
        cached_bot_token: tokio::sync::OnceCell::new(),
        token_hash: token_hash.to_string(),
        provider: provider.clone(),
        api_port,
        pg_pool,
        engine,
        health_registry: Arc::downgrade(health_registry),
        known_slash_commands: tokio::sync::OnceCell::new(),
        // #2448: capacity 256 gives ~hundreds of in-flight turns headroom
        // before a slow listener triggers `RecvError::Lagged`. The standby
        // relay subscriber falls back to file polling on lag.
        inflight_signals: tokio::sync::broadcast::channel(256).0,
    })
}

/// Build the voice receive hook (when barge-in is enabled), construct the
/// `VoiceReceiver`, and spawn the barge-in sensitivity-TTL-reset and progress
/// workers. Returns the `VoiceReceiver` so run_bot can hand it to the poise
/// framework setup. Runs after SharedData is built and before the intake
/// worker spawn — order preserved.
fn run_bot_init_voice_workers(
    voice_config: &crate::voice::VoiceConfig,
    voice_barge_in: &Arc<voice_barge_in::VoiceBargeInRuntime>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) -> crate::voice::VoiceReceiver {
    let voice_hook: Option<Arc<dyn crate::voice::VoiceReceiveHook>> =
        voice_barge_in.enabled().then(|| {
            Arc::new(voice_barge_in::DiscordVoiceBargeInHook::new(
                voice_barge_in.clone(),
                shared.clone(),
                provider.clone(),
            )) as Arc<dyn crate::voice::VoiceReceiveHook>
        });
    let voice_receiver =
        crate::voice::VoiceReceiver::from_voice_config_with_hook(voice_config, voice_hook);
    voice_barge_in.spawn_sensitivity_ttl_reset(shared.shutting_down.clone());
    voice_barge_in.spawn_progress_worker(shared.clone(), shared.shutting_down.clone());
    voice_receiver
}

/// Phase 5.1 of intake-node-routing (issue #2007): when intake routing is in
/// Enforce mode and a PG pool exists, spawn the REST-only intake_worker poll
/// loop (resolves `target_instance_id` inside the task to avoid racing
/// `cluster::bootstrap`). No-op in disabled/observe modes. Spawned after the
/// voice workers and before the gateway lease check — order preserved.
fn run_bot_maybe_spawn_intake_worker(
    shared: &Arc<SharedData>,
    token: &str,
    provider: &ProviderKind,
) {
    if matches!(
        crate::services::cluster::intake_router_hook::IntakeRoutingMode::from_env(),
        crate::services::cluster::intake_router_hook::IntakeRoutingMode::Enforce
    ) {
        if let Some(pool_for_intake_worker) = shared.pg_pool.clone() {
            let intake_worker_http = std::sync::Arc::new(serenity::http::Http::new(token));
            let intake_worker_shared = shared.clone();
            let intake_worker_token = token.to_string();
            let intake_worker_provider = provider.as_str().to_string();
            let intake_worker_cancel = shared.shutting_down.clone();
            // The intake_worker spawn runs concurrently with `cluster::bootstrap`
            // which is the writer of `SELF_INSTANCE_ID`. Resolving
            // `target_instance_id` eagerly here would race and pick up the
            // hostname+PID fallback (e.g. `itismyfieldui-Macmini-46662`)
            // instead of the configured cluster id (e.g. `mac-mini-release`).
            // The leader hook (`intake_router_hook::try_route_intake`) resolves
            // the same function later, by which time bootstrap has populated
            // the OnceLock — the two ids must match or every claim misses.
            // Bridge the race by awaiting the OnceLock inside the spawned task
            // before the worker logs "poll loop started".
            tokio::spawn(async move {
                let resolved_target_id =
                    crate::services::cluster::node_registry::wait_for_self_instance_id(
                        std::time::Duration::from_secs(30),
                    )
                    .await;
                // claim_owner appends provider so multi-bot deployments
                // surface which token's worker holds a row in
                // observability dashboards.
                let resolved_claim_owner =
                    format!("{}:{}", resolved_target_id, intake_worker_provider);
                crate::services::cluster::intake_worker::run_intake_worker_loop(
                    pool_for_intake_worker,
                    intake_worker_http,
                    intake_worker_shared,
                    intake_worker_token,
                    resolved_target_id,
                    intake_worker_provider,
                    resolved_claim_owner,
                    crate::services::cluster::intake_worker::IntakeWorkerConfig::default(),
                    intake_worker_cancel,
                )
                .await;
            });
        } else {
            tracing::info!(
                "[intake_worker] postgres pool unavailable — intake-node-routing worker not started"
            );
        }
    }
}

/// Acquire the Discord gateway singleton lease (advisory lock) when a PG pool
/// is present. Returns `Proceed(Some(lease))` on success, `Proceed(None)` when
/// there is no PG pool (standalone path), or `Skip` when the lease is held
/// elsewhere / acquisition failed. On the `Skip` paths this runs the
/// post-reconcile startup diagnostic exactly as the original early-returns did,
/// before returning; run_bot then decrements the shutdown barrier and returns.
#[allow(clippy::too_many_arguments)]
async fn run_bot_acquire_gateway_lease(
    shared: &Arc<SharedData>,
    token_hash: &str,
    provider: &ProviderKind,
    startup_reconcile_remaining: &Arc<std::sync::atomic::AtomicUsize>,
    startup_doctor_started: &Arc<std::sync::atomic::AtomicBool>,
    health_registry: &Arc<health::HealthRegistry>,
    api_port: u16,
) -> GatewayLeaseOutcome {
    match shared.pg_pool.as_ref() {
        Some(pool) => match try_acquire_discord_gateway_lease(pool, token_hash, provider).await {
            Ok(Some(lease)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔐 GATEWAY-LEASE: {} acquired singleton lease",
                    provider.display_name()
                );
                GatewayLeaseOutcome::Proceed(Some(lease))
            }
            Ok(None) => {
                run_startup_diagnostic_after_reconcile_barrier(
                    startup_reconcile_remaining.clone(),
                    startup_doctor_started.clone(),
                    health_registry.clone(),
                    api_port,
                )
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⏭ GATEWAY-LEASE: {} launch skipped — singleton lease held elsewhere",
                    provider.display_name()
                );
                GatewayLeaseOutcome::Skip
            }
            Err(error) => {
                run_startup_diagnostic_after_reconcile_barrier(
                    startup_reconcile_remaining.clone(),
                    startup_doctor_started.clone(),
                    health_registry.clone(),
                    api_port,
                )
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⏭ GATEWAY-LEASE: {} launch skipped — failed to acquire singleton lease: {}",
                    provider.display_name(),
                    error
                );
                GatewayLeaseOutcome::Skip
            }
        },
        None => GatewayLeaseOutcome::Proceed(None),
    }
}

/// Build the full ordered list of poise slash commands registered by the bot.
/// Order is preserved exactly as it was inline in run_bot.
fn run_bot_build_slash_commands() -> Vec<poise::Command<Data, Error>> {
    let mut slash_commands = vec![
        commands::cmd_start(),
        commands::cmd_pwd(),
        commands::cmd_status(),
        commands::cmd_inflight(),
        commands::cmd_clear(),
        commands::cmd_stop(),
        commands::cmd_down(),
        commands::cmd_shell(),
        commands::cmd_skill(),
        commands::cmd_cc(),
        commands::cmd_metrics(),
        commands::cmd_model(),
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

/// Spawn the gateway singleton-lease keepalive loop. On lease loss this
/// self-fences: flips shutdown flags, cancels tmux watchers, drains pending
/// queues, persists last_message_ids, and shuts down all shards. Spawned
/// after the client is built (needs `shard_manager`) and before the gateway
/// backend run. Returns the JoinHandle so run_bot can abort it on backend exit.
fn run_bot_spawn_gateway_lease_keepalive(
    mut lease: crate::db::postgres::AdvisoryLockLease,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    shard_manager: Arc<serenity::gateway::ShardManager>,
) -> tokio::task::JoinHandle<()> {
    let shared_for_lease = shared.clone();
    let provider_for_lease = provider.clone();
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

                let drain = mailbox_restart_drain_all(&shared_for_lease, &provider_for_lease).await;
                let queue_count = drain.queued_count;
                if !drain.persistence_errors.is_empty() {
                    tracing::error!(
                        failures = drain.persistence_errors.len(),
                        "gateway lease self-fence observed pending-queue persistence failure(s)"
                    );
                }
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
}

/// Spawn the SIGTERM graceful-shutdown handler. On SIGTERM it persists queue /
/// inflight / last_message state then quick-exits; tmux/TUI processes survive
/// for the next dcserver instance to rehydrate. Spawned after the lease
/// keepalive task and before the gateway backend run.
fn run_bot_spawn_sigterm_handler(shared: &Arc<SharedData>, provider_for_shutdown: ProviderKind) {
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

                // ── Critical state persistence (MUST run before any I/O) ──
                // Save pending queues and last_message_ids FIRST, before any
                // network calls that might block/timeout and prevent saving.

                let drain =
                    mailbox_restart_drain_all(&shared_for_signal, &provider_for_shutdown).await;
                let queue_count = drain.queued_count;
                if !drain.persistence_errors.is_empty() {
                    tracing::error!(
                        failures = drain.persistence_errors.len(),
                        "SIGTERM initial drain observed pending-queue persistence failure(s)"
                    );
                }
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
                    let marked = inflight::mark_all_inflight_states_restart_mode(
                        &provider_for_shutdown,
                        crate::services::discord::InflightRestartMode::DrainRestart,
                    );
                    tracing::info!(
                        "  [{ts2}] 🔖 marked {marked} inflight turn(s) as drain_restart"
                    );
                }

                // ── Final state snapshot (belt-and-suspenders) ──
                // During the HTTP placeholder edits above, active turns may have
                // finished and mutated queues/last_message_ids. Re-save to capture
                // any changes that occurred after the initial save.
                {
                    let drain =
                        mailbox_restart_drain_all(&shared_for_signal, &provider_for_shutdown).await;
                    let queue_count = drain.queued_count;
                    if !drain.persistence_errors.is_empty() {
                        tracing::error!(
                            failures = drain.persistence_errors.len(),
                            "SIGTERM final drain observed pending-queue persistence failure(s)"
                        );
                    }
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
}

/// Run the Discord gateway backend (`client.start()`) to completion, classify
/// the exit, run the post-reconcile startup diagnostic on failure, then abort
/// and join the gateway-lease keepalive task. This is the final event-loop
/// entry of run_bot. Consumes `client`.
#[allow(clippy::too_many_arguments)]
async fn run_bot_run_gateway_backend(
    mut client: serenity::Client,
    provider_for_error: &ProviderKind,
    gateway_lease_task: Option<tokio::task::JoinHandle<()>>,
    startup_reconcile_remaining_for_client_start: Arc<std::sync::atomic::AtomicUsize>,
    startup_doctor_started_for_client_start: Arc<std::sync::atomic::AtomicBool>,
    health_registry_for_client_start: Arc<health::HealthRegistry>,
    api_port: u16,
) {
    let gateway_backend_task = tokio::spawn(async move { client.start().await });
    let gateway_backend_failed = match gateway_backend_task.await {
        Ok(Ok(())) => {
            tracing::warn!(
                "  ✗ {} gateway backend exited without error",
                provider_for_error.display_name()
            );
            true
        }
        Ok(Err(error)) => {
            tracing::warn!(
                "  ✗ {} bot error: {error}",
                provider_for_error.display_name()
            );
            true
        }
        Err(join_error) if join_error.is_panic() => {
            tracing::error!(
                "  ✗ {} gateway backend task panicked: {join_error}",
                provider_for_error.display_name()
            );
            true
        }
        Err(join_error) => {
            tracing::warn!(
                "  ✗ {} gateway backend task ended unexpectedly: {join_error}",
                provider_for_error.display_name()
            );
            true
        }
    };
    if gateway_backend_failed {
        run_startup_diagnostic_after_reconcile_barrier(
            startup_reconcile_remaining_for_client_start,
            startup_doctor_started_for_client_start,
            health_registry_for_client_start,
            api_port,
        )
        .await;
    }

    if let Some(handle) = gateway_lease_task {
        handle.abort();
        let _ = handle.await;
    }
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

/// Periodic GC: delete stale idle/disconnected thread sessions from DB.
async fn gc_stale_thread_sessions(shared: &Arc<SharedData>) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!("  [{ts}] ⚠ Thread session GC skipped: postgres pool unavailable");
        return;
    };
    let deleted_keys = crate::db::dispatched_sessions::gc_stale_thread_sessions_pg(pool).await;
    if deleted_keys.is_empty() {
        return;
    }
    // Option A: kill the orphan thread tmux sessions whose DB rows we just
    // removed. Their inner CLI commonly stays at an interactive prompt (pane
    // never dies), so the dead-pane reaper skips them, and with the row gone
    // the 8h idle-kill policy can never reach them either — they would leak
    // forever. The effective grace becomes the GC TTL (1h no-dispatch / 3h).
    let killed = reap_orphan_thread_tmux(&deleted_keys).await;
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] 🧹 GC: removed {} stale thread session(s) from DB, killed {} orphan tmux",
        deleted_keys.len(),
        killed
    );
}

/// Kill the tmux sessions whose stale thread rows were just GC'd. Only touches
/// sessions this runtime owns (owner marker) and that still exist locally, so a
/// co-located dev/release instance can't kill the other's sessions.
async fn reap_orphan_thread_tmux(deleted_keys: &[String]) -> usize {
    #[cfg(unix)]
    {
        let marker = crate::services::tmux_common::current_tmux_owner_marker();
        let keys = deleted_keys.to_vec();
        tokio::task::spawn_blocking(move || {
            let mut killed = 0usize;
            for key in &keys {
                let Some(tmux_name) = deleted_thread_tmux_reap_candidate(
                    key,
                    &marker,
                    super::tmux::session_belongs_to_current_runtime,
                    crate::services::platform::tmux::has_session,
                ) else {
                    continue;
                };
                if crate::services::platform::tmux::kill_session(
                    tmux_name,
                    "stale thread session GC — DB row removed",
                ) {
                    killed += 1;
                }
            }
            killed
        })
        .await
        .unwrap_or(0)
    }
    #[cfg(not(unix))]
    {
        let _ = deleted_keys;
        0
    }
}

#[cfg(unix)]
fn deleted_thread_tmux_reap_candidate<'a>(
    session_key: &'a str,
    current_owner_marker: &str,
    belongs_to_current_runtime: impl Fn(&str, &str) -> bool,
    has_session: impl Fn(&str) -> bool,
) -> Option<&'a str> {
    // session_key format is `hostname:tmux_name`.
    let (_, tmux_name) = session_key.split_once(':')?;
    let (_, channel_name) =
        crate::services::provider::parse_provider_and_channel_from_tmux_name(tmux_name)?;
    super::adk_session::parse_thread_channel_id_from_name(&channel_name)?;
    if !belongs_to_current_runtime(tmux_name, current_owner_marker) {
        return None;
    }
    if !has_session(tmux_name) {
        return None;
    }
    Some(tmux_name)
}

#[cfg(all(test, unix))]
mod thread_session_gc_tests {
    use super::deleted_thread_tmux_reap_candidate;
    use std::collections::HashSet;

    #[test]
    fn thread_gc_reap_candidate_requires_thread_owner_marker_and_existing_tmux() {
        let marker = "runtime-a";
        let owned_thread = "AgentDesk-codex-adk-cdx-t1500628371829428350";
        let foreign_thread = "AgentDesk-codex-adk-cdx-t1500628371829428351";
        let main_channel = "AgentDesk-codex-adk-cdx";
        let missing_thread = "AgentDesk-codex-adk-cdx-t1500628371829428352";
        let existing: HashSet<&str> = [owned_thread, foreign_thread, main_channel].into();
        let owned: HashSet<&str> = [owned_thread, main_channel].into();

        let belongs_to_current_runtime =
            |name: &str, owner_marker: &str| owner_marker == marker && owned.contains(name);
        let has_session = |name: &str| existing.contains(name);

        assert_eq!(
            deleted_thread_tmux_reap_candidate(
                &format!("host:{owned_thread}"),
                marker,
                &belongs_to_current_runtime,
                &has_session,
            ),
            Some(owned_thread)
        );
        assert_eq!(
            deleted_thread_tmux_reap_candidate(
                &format!("host:{foreign_thread}"),
                marker,
                &belongs_to_current_runtime,
                &has_session,
            ),
            None,
            "foreign owner marker must prevent killing another runtime's thread tmux"
        );
        assert_eq!(
            deleted_thread_tmux_reap_candidate(
                &format!("host:{main_channel}"),
                marker,
                &belongs_to_current_runtime,
                &has_session,
            ),
            None,
            "thread GC must not kill fixed-channel tmux names"
        );
        assert_eq!(
            deleted_thread_tmux_reap_candidate(
                &format!("host:{missing_thread}"),
                marker,
                &belongs_to_current_runtime,
                &has_session,
            ),
            None,
            "missing local tmux session is not a reap target"
        );
        assert_eq!(
            deleted_thread_tmux_reap_candidate(
                "malformed-session-key",
                marker,
                &belongs_to_current_runtime,
                &has_session,
            ),
            None
        );
    }
}

/// Periodic GC: disconnect stale fixed-channel working sessions from the DB so
/// restart recovery cannot restore dead provider session IDs.
async fn gc_stale_fixed_working_sessions(shared: &Arc<SharedData>) {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return;
    };
    let cleared = crate::db::dispatched_sessions::gc_stale_fixed_working_sessions_db_pg(pool).await;

    if cleared > 0 {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🧹 GC: disconnected {cleared} stale fixed-channel working session(s)"
        );
    }
}

#[cfg(test)]
mod bootstrap_tests {
    use super::*;
    use std::collections::{HashMap, HashSet, VecDeque};

    fn sorted_channel_ids(channels: Vec<ChannelId>) -> Vec<u64> {
        channels
            .into_iter()
            .map(|channel_id| channel_id.get())
            .collect()
    }

    fn sorted_placeholder_pairs(
        pairs: Vec<((ChannelId, MessageId), MessageId)>,
    ) -> Vec<(u64, u64, u64)> {
        let mut pairs: Vec<(u64, u64, u64)> = pairs
            .into_iter()
            .map(|((channel_id, user_msg_id), placeholder_msg_id)| {
                (
                    channel_id.get(),
                    user_msg_id.get(),
                    placeholder_msg_id.get(),
                )
            })
            .collect();
        pairs.sort_unstable();
        pairs
    }

    fn sorted_stale_cards(cards: Vec<(ChannelId, MessageId, MessageId)>) -> Vec<(u64, u64, u64)> {
        let mut cards: Vec<(u64, u64, u64)> = cards
            .into_iter()
            .map(|(channel_id, user_msg_id, placeholder_msg_id)| {
                (
                    channel_id.get(),
                    user_msg_id.get(),
                    placeholder_msg_id.get(),
                )
            })
            .collect();
        cards.sort_unstable();
        cards
    }

    #[test]
    fn startup_doctor_barrier_arrive_decrements_once_until_release() {
        let remaining = std::sync::atomic::AtomicUsize::new(2);
        let started = std::sync::atomic::AtomicBool::new(false);

        assert_eq!(
            startup_doctor_barrier_arrive(&remaining, &started),
            StartupDoctorBarrier::Waiting(1)
        );
        assert_eq!(remaining.load(std::sync::atomic::Ordering::Acquire), 1);
        assert!(!started.load(std::sync::atomic::Ordering::Acquire));

        assert_eq!(
            startup_doctor_barrier_arrive(&remaining, &started),
            StartupDoctorBarrier::Released
        );
        assert_eq!(remaining.load(std::sync::atomic::Ordering::Acquire), 0);
        assert!(started.load(std::sync::atomic::Ordering::Acquire));

        assert_eq!(
            startup_doctor_barrier_arrive(&remaining, &started),
            StartupDoctorBarrier::AlreadyReleased
        );
        assert_eq!(
            remaining.load(std::sync::atomic::Ordering::Acquire),
            0,
            "arriving after release must not decrement below zero"
        );
    }

    #[test]
    fn startup_doctor_barrier_arrive_handles_prestarted_release_once() {
        let remaining = std::sync::atomic::AtomicUsize::new(1);
        let started = std::sync::atomic::AtomicBool::new(true);

        assert_eq!(
            startup_doctor_barrier_arrive(&remaining, &started),
            StartupDoctorBarrier::AlreadyReleased
        );
        assert_eq!(
            remaining.load(std::sync::atomic::Ordering::Acquire),
            0,
            "the final waiter still consumes exactly one remaining slot"
        );

        assert_eq!(
            startup_doctor_barrier_arrive(&remaining, &started),
            StartupDoctorBarrier::AlreadyReleased
        );
        assert_eq!(remaining.load(std::sync::atomic::Ordering::Acquire), 0);
    }

    #[test]
    fn restored_settings_filters_sort_parse_and_drop_disabled_entries() {
        let mut settings = DiscordBotSettings::default();
        settings.channel_fast_modes.insert("300".to_string(), true);
        settings.channel_fast_modes.insert("100".to_string(), true);
        settings.channel_fast_modes.insert("200".to_string(), false);
        settings
            .channel_fast_modes
            .insert("not-a-channel".to_string(), true);
        settings
            .channel_fast_mode_reset_pending
            .insert("codex:500".to_string());
        settings
            .channel_fast_mode_reset_pending
            .insert("400".to_string());
        settings
            .channel_fast_mode_reset_pending
            .insert("claude:400".to_string());
        settings
            .channel_fast_mode_reset_pending
            .insert("bad-reset-entry".to_string());
        settings.channel_codex_goals.insert("700".to_string(), true);
        settings.channel_codex_goals.insert("600".to_string(), true);
        settings
            .channel_codex_goals
            .insert("800".to_string(), false);
        settings
            .channel_codex_goals
            .insert("bad-goals".to_string(), true);
        settings
            .channel_codex_goals_reset_pending
            .insert("900".to_string());
        settings
            .channel_codex_goals_reset_pending
            .insert("850".to_string());
        settings
            .channel_codex_goals_reset_pending
            .insert("bad-reset".to_string());

        assert_eq!(
            sorted_channel_ids(restored_fast_mode_enabled_channels_for_provider(
                &settings,
                &ProviderKind::Codex,
            )),
            vec![100, 300]
        );
        assert_eq!(
            restored_fast_mode_reset_entries(&settings),
            vec![
                "400".to_string(),
                "bad-reset-entry".to_string(),
                "claude:400".to_string(),
                "codex:500".to_string(),
            ]
        );
        assert_eq!(
            sorted_channel_ids(restored_fast_mode_reset_channels(&settings)),
            vec![400, 500]
        );
        assert_eq!(
            sorted_channel_ids(restored_codex_goals_enabled_channels(&settings)),
            vec![600, 700]
        );
        assert_eq!(
            sorted_channel_ids(restored_codex_goals_reset_channels(&settings)),
            vec![850, 900]
        );
    }

    #[test]
    fn filter_restored_queued_placeholders_preserves_live_and_reports_stale() {
        let channel_live = ChannelId::new(10);
        let channel_stale = ChannelId::new(20);
        let mut loaded = HashMap::new();
        loaded.insert((channel_live, MessageId::new(100)), MessageId::new(1_000));
        loaded.insert((channel_live, MessageId::new(101)), MessageId::new(1_001));
        loaded.insert((channel_stale, MessageId::new(200)), MessageId::new(2_000));

        let mut live_queue_ids = HashMap::new();
        live_queue_ids.insert(channel_live, HashSet::from([100_u64]));

        let outcome = filter_restored_queued_placeholders(loaded, &live_queue_ids);

        assert_eq!(outcome.stale_count, 2);
        assert_eq!(
            outcome
                .channels_with_stale
                .iter()
                .map(|channel_id| channel_id.get())
                .collect::<HashSet<_>>(),
            HashSet::from([10, 20])
        );
        assert_eq!(
            sorted_placeholder_pairs(outcome.live),
            vec![(10, 100, 1_000)]
        );
        assert_eq!(
            sorted_stale_cards(outcome.stale_cards),
            vec![(10, 101, 1_001), (20, 200, 2_000)]
        );
    }

    #[test]
    fn discord_gateway_intents_snapshot_matches_bootstrap_contract() {
        let intents = discord_gateway_intents();
        let expected = serenity::GatewayIntents::GUILDS
            | serenity::GatewayIntents::GUILD_MESSAGES
            | serenity::GatewayIntents::GUILD_MESSAGE_REACTIONS
            | serenity::GatewayIntents::GUILD_VOICE_STATES
            | serenity::GatewayIntents::DIRECT_MESSAGES
            | serenity::GatewayIntents::DIRECT_MESSAGE_REACTIONS
            | serenity::GatewayIntents::MESSAGE_CONTENT;

        assert_eq!(intents, expected);
    }

    struct RecordingStalePlaceholderDeleter {
        calls: std::sync::Mutex<Vec<(u64, u64)>>,
        results: std::sync::Mutex<VecDeque<Result<(), String>>>,
    }

    impl RecordingStalePlaceholderDeleter {
        fn new(results: impl IntoIterator<Item = Result<(), String>>) -> Self {
            Self {
                calls: std::sync::Mutex::new(Vec::new()),
                results: std::sync::Mutex::new(results.into_iter().collect()),
            }
        }

        fn calls(&self) -> Vec<(u64, u64)> {
            self.calls
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .clone()
        }
    }

    impl StalePlaceholderDeleter for RecordingStalePlaceholderDeleter {
        fn delete<'a>(
            &'a self,
            channel_id: ChannelId,
            placeholder_msg_id: MessageId,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send + 'a>>
        {
            Box::pin(async move {
                self.calls
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .push((channel_id.get(), placeholder_msg_id.get()));
                self.results
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner())
                    .pop_front()
                    .unwrap_or(Ok(()))
            })
        }
    }

    #[tokio::test]
    async fn delete_stale_queued_placeholder_cards_with_deletes_only_supplied_stale_cards() {
        let deleter = RecordingStalePlaceholderDeleter::new([Ok(()), Err("gone".to_string())]);
        let stale_cards = vec![
            (
                ChannelId::new(10),
                MessageId::new(100),
                MessageId::new(1_000),
            ),
            (
                ChannelId::new(20),
                MessageId::new(200),
                MessageId::new(2_000),
            ),
        ];

        delete_stale_queued_placeholder_cards_with(&deleter, &stale_cards).await;

        assert_eq!(deleter.calls(), vec![(10, 1_000), (20, 2_000)]);

        let empty_deleter = RecordingStalePlaceholderDeleter::new([]);
        delete_stale_queued_placeholder_cards_with(&empty_deleter, &[]).await;
        assert!(
            empty_deleter.calls().is_empty(),
            "empty stale-card input must preserve all visible cards"
        );
    }

    #[tokio::test(flavor = "current_thread", start_paused = false)]
    async fn wait_for_local_http_bind_returns_quickly_when_port_is_bound() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let port = listener.local_addr().expect("local addr").port();
        let started = std::time::Instant::now();
        wait_for_local_http_bind(port).await;
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "bound port should resolve well under the {:?} bind timeout (elapsed {:?})",
            STARTUP_DOCTOR_HTTP_BIND_TIMEOUT,
            started.elapsed()
        );
        drop(listener);
    }

    #[test]
    fn bootstrap_session_reset_pending_excludes_restored_model_overrides() {
        let model_only_channel = ChannelId::new(123);
        let fast_mode_reset_channel = ChannelId::new(456);
        let goals_reset_channel = ChannelId::new(789);
        let restored_model_overrides = vec![(model_only_channel, "gpt-5.5".to_string())];

        let pending = bootstrap_session_reset_pending_channels(
            &restored_model_overrides,
            &[fast_mode_reset_channel],
            &[goals_reset_channel],
        );

        assert!(
            !pending.contains(&model_only_channel),
            "restoring a persisted model override must not force a fresh provider session"
        );
        assert!(pending.contains(&fast_mode_reset_channel));
        assert!(pending.contains(&goals_reset_channel));
    }

    #[test]
    fn voice_auto_join_provider_map_includes_agent_voice_channel() {
        let cfg: crate::config::Config = serde_yaml::from_str(
            r#"
server:
  port: 8791
agents:
- id: project-agentdesk
  name: AgentDesk
  provider: claude
  voice:
    channel_id: '999'
    foreground:
      provider: codex
  channels:
    codex:
      id: '123'
      name: adk-cdx
      provider: codex
"#,
        )
        .expect("config parses");

        let map = voice_auto_join_provider_map(&cfg);

        assert_eq!(map.get("123").map(|value| value.0.as_str()), Some("codex"));
        assert_eq!(map.get("123").and_then(|value| value.1.as_deref()), None);
        assert_eq!(map.get("999").map(|value| value.0.as_str()), Some("codex"));
        assert_eq!(
            map.get("999").and_then(|value| value.1.as_deref()),
            Some("project-agentdesk")
        );
    }

    #[test]
    fn legacy_durable_handoff_cleanup_removes_retired_json_tree() {
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        struct EnvReset(Option<std::ffi::OsString>);
        impl Drop for EnvReset {
            fn drop(&mut self) {
                match self.0.take() {
                    Some(value) => unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", value) },
                    None => unsafe { std::env::remove_var("AGENTDESK_ROOT_DIR") },
                }
            }
        }
        let _reset = EnvReset(previous_root);

        let tmp = tempfile::tempdir().unwrap();
        unsafe { std::env::set_var("AGENTDESK_ROOT_DIR", tmp.path()) };
        let handoff_root = tmp
            .path()
            .join("runtime")
            .join("discord_handoff")
            .join("codex");
        std::fs::create_dir_all(&handoff_root).unwrap();
        std::fs::write(handoff_root.join("1486333430516945008.json"), "{}").unwrap();

        purge_legacy_durable_handoffs();

        assert!(
            !tmp.path().join("runtime").join("discord_handoff").exists(),
            "legacy handoff JSON must be removed without being parsed or consumed"
        );
    }
}
