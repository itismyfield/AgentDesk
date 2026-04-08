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

async fn execute_handoff_turns(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let skip_handoff = |provider: &ProviderKind, channel_id: u64| {
        let _ = update_handoff_state(provider, channel_id, "skipped");
        clear_handoff(provider, channel_id);
    };
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
        let (eff_id, eff_name) =
            if let Some((pid, pname)) = resolve_thread_parent(http, channel_id).await {
                (pid, pname.or(record.channel_name.clone()))
            } else {
                (channel_id, record.channel_name.clone())
            };
        if let Err(reason) = validate_bot_channel_routing(
            &settings_snapshot,
            provider,
            eff_id,
            eff_name.as_deref(),
            is_dm,
        ) {
            println!(
                "  [{ts}] ⏭ Skipping handoff for channel {} — {reason}",
                record.channel_id
            );
            skip_handoff(provider, record.channel_id);
            continue;
        }

        // Skip if pending queue messages exist (user intent takes priority)
        let has_pending = {
            let data = shared.core.lock().await;
            data.intervention_queue
                .get(&channel_id)
                .map(|q| !q.is_empty())
                .unwrap_or(false)
        };
        if has_pending {
            println!(
                "  [{ts}] ⏭ Skipping handoff for channel {} (pending queue has messages)",
                record.channel_id
            );
            skip_handoff(provider, record.channel_id);
            continue;
        }

        // Skip if an active turn is already running
        let has_active = {
            let data = shared.core.lock().await;
            data.cancel_tokens.contains_key(&channel_id)
        };
        if has_active {
            println!(
                "  [{ts}] ⏭ Skipping handoff for channel {} (active turn running)",
                record.channel_id
            );
            skip_handoff(provider, record.channel_id);
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
            skip_handoff(provider, record.channel_id);
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
        {
            let mut data = shared.core.lock().await;
            let queue = data.intervention_queue.entry(channel_id).or_default();
            queue.push(Intervention {
                author_id: serenity::UserId::new(1), // system-generated sentinel
                message_id: placeholder.id,
                text: handoff_prompt,
                mode: InterventionMode::Soft,
                created_at: Instant::now(),
            });
        }

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

/// Kick off turns for channels that have queued interventions but no active
/// turn running. This bridges the gap where restored pending queues or
/// handoff injections sit idle because no turn-completion event triggers
/// the dequeue chain.
pub(super) fn scan_skills(
    provider: &ProviderKind,
    project_path: Option<&str>,
) -> Vec<(String, String)> {
    if let Some(root) = crate::config::runtime_root() {
        let _ = crate::runtime_layout::sync_managed_skills(&root);
    }

    let mut skills: Vec<(String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    match provider {
        ProviderKind::Claude => {
            for (name, desc) in BUILTIN_SKILLS {
                seen.insert(name.to_string());
                skills.push((name.to_string(), desc.to_string()));
            }

            let dirs_to_scan = collect_provider_skill_roots(provider, project_path);

            for dir in dirs_to_scan {
                if !dir.is_dir() {
                    continue;
                }
                let Ok(entries) = fs::read_dir(&dir) else {
                    continue;
                };
                for entry in entries.filter_map(|e| e.ok()) {
                    let path = entry.path();
                    if path.extension().map(|e| e == "md").unwrap_or(false)
                        && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                    {
                        let name = stem.to_string();
                        if seen.insert(name.clone()) {
                            let desc = fs::read_to_string(&path)
                                .ok()
                                .map(|content| extract_skill_description(&content))
                                .unwrap_or_else(|| format!("Skill: {}", name));
                            skills.push((name, desc));
                        }
                    }
                }
            }
        }
        ProviderKind::Codex | ProviderKind::Gemini | ProviderKind::Qwen => {
            scan_directory_skills(
                collect_provider_skill_roots(provider, project_path),
                &mut seen,
                &mut skills,
            );
        }
        ProviderKind::Unsupported(_) => {}
    }

    skills.sort_by(|a, b| a.0.cmp(&b.0));
    skills
}

/// Compute a lightweight fingerprint of skill directories: (file_count, max_mtime_epoch).
/// Used by the hot-reload poll to detect additions, modifications, and deletions.
fn skill_dir_fingerprint(provider: &ProviderKind) -> (usize, u64) {
    let mut count = 0usize;
    let mut max_mtime = 0u64;

    let mut dirs = collect_provider_skill_roots(provider, None);
    if provider_supports_directory_skills(provider)
        && let Some(root) = crate::config::runtime_root()
    {
        dirs.push(crate::runtime_layout::managed_skills_root(&root));
    }

    fn walk_mtime(dir: &Path, count: &mut usize, max_mtime: &mut u64) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_mtime(&path, count, max_mtime);
            } else if path.extension().map(|e| e == "md").unwrap_or(false) {
                *count += 1;
                if let Ok(meta) = fs::metadata(&path)
                    && let Ok(mt) = meta.modified()
                {
                    let epoch = mt
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    if epoch > *max_mtime {
                        *max_mtime = epoch;
                    }
                }
            }
        }
    }

    for dir in &dirs {
        walk_mtime(dir, &mut count, &mut max_mtime);
    }

    (count, max_mtime)
}

/// Like `skill_dir_fingerprint` but also includes project-level skill directories.
fn skill_dir_fingerprint_with_projects(
    provider: &ProviderKind,
    project_paths: &[String],
) -> (usize, u64) {
    let (mut count, mut max_mtime) = skill_dir_fingerprint(provider);

    fn walk_mtime(dir: &Path, count: &mut usize, max_mtime: &mut u64) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk_mtime(&path, count, max_mtime);
            } else if path.extension().map(|e| e == "md").unwrap_or(false) {
                *count += 1;
                if let Ok(meta) = fs::metadata(&path)
                    && let Ok(mt) = meta.modified()
                {
                    let epoch = mt
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    if epoch > *max_mtime {
                        *max_mtime = epoch;
                    }
                }
            }
        }
    }

    for path in project_paths {
        let Some(proj_dir) = provider_project_skill_dir(provider, path) else {
            continue;
        };
        if proj_dir.is_dir() {
            walk_mtime(&proj_dir, &mut count, &mut max_mtime);
        }
    }

    (count, max_mtime)
}

fn provider_supports_directory_skills(provider: &ProviderKind) -> bool {
    matches!(
        provider,
        ProviderKind::Claude | ProviderKind::Codex | ProviderKind::Gemini | ProviderKind::Qwen
    )
}

fn provider_home_skill_dir(provider: &ProviderKind, home: &Path) -> Option<std::path::PathBuf> {
    match provider {
        ProviderKind::Claude => Some(home.join(".claude").join("commands")),
        ProviderKind::Codex => Some(home.join(".codex").join("skills")),
        ProviderKind::Gemini => Some(home.join(".gemini").join("skills")),
        ProviderKind::Qwen => Some(home.join(".qwen").join("skills")),
        ProviderKind::Unsupported(_) => None,
    }
}

fn provider_project_skill_dir(
    provider: &ProviderKind,
    project_path: &str,
) -> Option<std::path::PathBuf> {
    let project_root = Path::new(project_path);
    match provider {
        ProviderKind::Claude => Some(project_root.join(".claude").join("commands")),
        ProviderKind::Codex => Some(project_root.join(".codex").join("skills")),
        ProviderKind::Gemini => Some(project_root.join(".gemini").join("skills")),
        ProviderKind::Qwen => Some(project_root.join(".qwen").join("skills")),
        ProviderKind::Unsupported(_) => None,
    }
}

fn collect_provider_skill_roots(
    provider: &ProviderKind,
    project_path: Option<&str>,
) -> Vec<std::path::PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir()
        && let Some(path) = provider_home_skill_dir(provider, &home)
    {
        roots.push(path);
    }
    if let Some(project_path) = project_path
        && let Some(path) = provider_project_skill_dir(provider, project_path)
    {
        roots.push(path);
    }
    roots
}

fn scan_directory_skills(
    roots: Vec<std::path::PathBuf>,
    seen: &mut std::collections::HashSet<String>,
    skills: &mut Vec<(String, String)>,
) {
    for root in roots {
        if !root.is_dir() {
            continue;
        }
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let path = entry.path();
            collect_directory_skill(&path, seen, skills);

            if !path.is_dir() {
                continue;
            }
            let Ok(nested) = fs::read_dir(&path) else {
                continue;
            };
            for child in nested.filter_map(|e| e.ok()) {
                collect_directory_skill(&child.path(), seen, skills);
            }
        }
    }
}

fn collect_directory_skill(
    path: &Path,
    seen: &mut std::collections::HashSet<String>,
    skills: &mut Vec<(String, String)>,
) {
    let Some(skill_path) = resolve_codex_skill_file(path) else {
        return;
    };
    let Some(name) = skill_path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|s| s.to_str())
    else {
        return;
    };
    let name = name.to_string();
    if !seen.insert(name.clone()) {
        return;
    }
    let desc = fs::read_to_string(&skill_path)
        .ok()
        .map(|content| extract_skill_description(&content))
        .unwrap_or_else(|| format!("Skill: {}", name));
    skills.push((name, desc));
}

fn resolve_codex_skill_file(path: &Path) -> Option<std::path::PathBuf> {
    if path.is_dir() {
        let skill_path = path.join("SKILL.md");
        if skill_path.is_file() {
            return Some(skill_path);
        }
    }
    None
}

/// Entry point: start the Discord bot
pub(crate) async fn run_bot(token: &str, provider: ProviderKind, context: RunBotContext) {
    // Initialize debug logging from environment variable
    claude::init_debug_from_env();
    let RunBotContext {
        global_active,
        global_finalizing,
        shutdown_remaining,
        health_registry,
        api_port,
        db,
        engine,
    } = context;

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
            cancel_tokens: HashMap::new(),
            active_request_owner: HashMap::new(),
            intervention_queue: HashMap::new(),
            active_meetings: HashMap::new(),
        }),
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
                let cmd_names: std::collections::HashSet<String> = commands
                    .iter()
                    .map(|c| c.name.clone())
                    .collect();
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
                shared_for_migrate.bot_connected.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = shared_for_migrate.cached_serenity_ctx.set(ctx.clone());
                let _ = shared_for_migrate.cached_bot_token.set(token_for_ready.clone());
                health_registry_for_setup.register_http(provider_for_setup.as_str().to_string(), ctx.http.clone()).await;

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
                        if !shared_for_deferred.restart_pending.load(Ordering::Relaxed)
                            && let Some(root) = crate::agentdesk_runtime_root()
                            && root.join("restart_pending").exists()
                        {
                            shared_for_deferred
                                .restart_pending
                                .store(true, Ordering::SeqCst);
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!(
                                "  [{ts}] ⏸ DRAIN: restart_pending detected, entering drain mode — new turns blocked"
                            );
                        }
                        // Use process-global counters so we wait for ALL providers
                        let g_active = shared_for_deferred.global_active.load(Ordering::Relaxed);
                        let g_finalizing = shared_for_deferred.global_finalizing.load(Ordering::Relaxed);
                        if g_active == 0 && g_finalizing == 0 && shared_for_deferred.restart_pending.load(Ordering::Relaxed) {
                            // Save pending queues before exiting so they survive restart
                            {
                                let data = shared_for_deferred.core.lock().await;
                                let queue_count: usize =
                                    data.intervention_queue.values().map(|q| q.len()).sum();
                                if queue_count > 0 {
                                    save_pending_queues(
                                        &provider_for_deferred,
                                        &shared_for_deferred.token_hash,
                                        &data.intervention_queue,
                                        &shared_for_deferred.dispatch_role_overrides,
                                    );
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!("  [{ts}] 📋 DRAIN: saved {queue_count} pending queue item(s) before deferred restart");
                                }
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
                            let mut paths: Vec<String> = data.sessions.values()
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
                            println!("  [{ts}] 🔄 Skills hot-reloaded: {count} skill(s) ({} files, mtime Δ)", fp.0);
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
                    gc_stale_fixed_working_sessions(&shared_for_tmux2).await;
                    restore_inflight_turns(&http_for_tmux, &shared_for_tmux2, &provider_for_restore).await;

                    // Restore pending intervention queues saved during previous SIGTERM
                    let (restored_queues, restored_overrides) = load_pending_queues(
                        &provider_for_restore,
                        &shared_for_tmux2.token_hash,
                    );
                    for (thread_channel_id, alt_channel_id) in &restored_overrides {
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
                        let mut data = shared_for_tmux2.core.lock().await;
                        for (channel_id, items) in restored_queues {
                            let queue = data.intervention_queue.entry(channel_id).or_default();
                            let existing_ids: std::collections::HashSet<u64> =
                                queue.iter().map(|i| i.message_id.get()).collect();
                            for item in items {
                                if existing_ids.contains(&item.message_id.get()) {
                                    skipped += 1;
                                } else {
                                    queue.push(item);
                                    added += 1;
                                }
                            }
                        }
                        drop(data);
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts}] 📋 FLUSH: restored {added} pending queue item(s) from disk (skipped {skipped} duplicates)");
                    }

                    if let Some(ref db) = shared_for_tmux2.db {
                        let (checked, cleared) =
                            crate::server::routes::dispatches::validate_channel_thread_maps_on_startup(
                                db,
                                &token_for_kickoff,
                            )
                            .await;
                        if checked > 0 || cleared > 0 {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!(
                                "  [{ts}] 🧹 THREAD-MAP: validated {checked} mapping(s), cleared {cleared} stale binding(s)"
                            );
                        }
                    }

                    // Startup catch-up polling: recover messages lost during restart gap
                    catch_up_missed_messages(
                        &http_for_tmux,
                        &shared_for_tmux2,
                        &provider_for_restore,
                    ).await;

                    // #226: Collect channels that recovery already handled (spawned + ended watchers).
                    // restore_tmux_watchers must skip these to prevent duplicate watcher creation.
                    // The issue: recovery watcher starts → session ends quickly → watcher removes
                    // itself from DashMap → restore_tmux_watchers sees empty slot → creates second watcher.
                    #[cfg(unix)]
                    {
                        // Mark all channels that recovery touched as "recently handled"
                        // by inserting a recovery_handled marker in kv_meta.
                        // restore_tmux_watchers checks this and skips those channels.
                        if let Some(ref db) = shared_for_tmux2.db
                            && let Ok(conn) = db.lock()
                        {
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

                        restore_tmux_watchers(&http_for_tmux, &shared_for_tmux2).await;
                        cleanup_orphan_tmux_sessions(&shared_for_tmux2).await;

                        // Clean up recovery markers
                        if let Some(ref db) = shared_for_tmux2.db
                            && let Ok(conn) = db.lock()
                        {
                            conn.execute(
                                "DELETE FROM kv_meta WHERE key LIKE 'recovery_handled_channel:%'",
                                [],
                            )
                            .ok();
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
                    super::mark_reconcile_complete(&shared_for_restart_reports);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ✓ Reconcile complete — intake open");

                    // Kick off again to drain messages queued during reconcile window
                    kickoff_idle_queues(
                        &ctx_for_kickoff,
                        &shared_for_restart_reports,
                        &token_for_kickoff,
                        &provider_for_restore,
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

    let intents = serenity::GatewayIntents::GUILDS
        | serenity::GatewayIntents::GUILD_MESSAGES
        | serenity::GatewayIntents::DIRECT_MESSAGES
        | serenity::GatewayIntents::MESSAGE_CONTENT;

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

                // Persist pending intervention queues so they survive restart
                {
                    let data = shared_for_signal.core.lock().await;
                    let queue_count: usize =
                        data.intervention_queue.values().map(|q| q.len()).sum();
                    if queue_count > 0 {
                        save_pending_queues(
                            &provider_for_shutdown,
                            &shared_for_signal.token_hash,
                            &data.intervention_queue,
                            &shared_for_signal.dispatch_role_overrides,
                        );
                        let ts3 = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts3}] 📋 saved {queue_count} pending queue item(s) to disk");
                    }
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
                    let data = shared_for_signal.core.lock().await;
                    let queue_count: usize =
                        data.intervention_queue.values().map(|q| q.len()).sum();
                    if queue_count > 0 {
                        save_pending_queues(
                            &provider_for_shutdown,
                            &shared_for_signal.token_hash,
                            &data.intervention_queue,
                            &shared_for_signal.dispatch_role_overrides,
                        );
                        let ts4 = chrono::Local::now().format("%H:%M:%S");
                        println!("  [{ts4}] 📋 final save: {queue_count} pending queue item(s)");
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
                    && shared_for_signal
                        .shutdown_remaining
                        .fetch_sub(1, std::sync::atomic::Ordering::AcqRel)
                        == 1
                {
                    std::process::exit(0);
                }
            }
        }
    });

    if let Err(e) = client.start().await {
        eprintln!("  ✗ {} bot error: {e}", provider_for_error.display_name());
    }
}

/// Check if a user is authorized (owner or allowed user)
/// Returns true if authorized, false if rejected.
/// On first use, registers the user as owner.
pub(super) async fn check_auth(
    user_id: UserId,
    user_name: &str,
    shared: &Arc<SharedData>,
    token: &str,
) -> bool {
    let mut settings = shared.settings.write().await;
    match settings.owner_user_id {
        None => {
            // Imprint: register first user as owner
            settings.owner_user_id = Some(user_id.get());
            save_bot_settings(token, &settings);
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ★ Owner registered: {user_name} (id:{})",
                user_id.get()
            );
            true
        }
        Some(_) => {
            let uid = user_id.get();
            if user_is_authorized(&settings, uid) {
                true
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ✗ Rejected: {user_name} (id:{})", uid);
                false
            }
        }
    }
}

pub(super) fn user_is_authorized(settings: &DiscordBotSettings, user_id: u64) -> bool {
    settings.allow_all_users
        || settings.owner_user_id == Some(user_id)
        || settings.allowed_user_ids.contains(&user_id)
}

/// Check if a user is the owner (not just allowed)
pub(super) async fn check_owner(user_id: UserId, shared: &Arc<SharedData>) -> bool {
    let settings = shared.settings.read().await;
    settings.owner_user_id == Some(user_id.get())
}

/// Check for pending DM replies and consume them. The answer text is stored
/// in the consumed row's context (as `_answer`), and a notification is sent
/// to the source agent's Discord channel so its session can process the reply.
pub(super) async fn try_handle_pending_dm_reply(
    db: &crate::db::Db,
    msg: &serenity::Message,
) -> bool {
    if msg.author.bot || msg.guild_id.is_some() {
        return false;
    }
    let answer = msg.content.trim();
    if answer.is_empty() {
        return false;
    }
    let user_id_str = msg.author.id.get().to_string();
    let username = msg.author.name.clone();
    let db = db.clone();
    let answer_owned = answer.to_string();
    let result = tokio::task::spawn_blocking(move || {
        consume_pending_dm_reply(&db, &user_id_str, &answer_owned)
    })
    .await;
    match result {
        Ok(Some(info)) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ✉️ DM reply consumed: user={} agent={} id={}",
                msg.author.id.get(),
                info.source_agent,
                info.id
            );

            // Notify the source agent's Discord channel (inline, not fire-and-forget)
            if let Err(e) = notify_source_agent(
                &info.db,
                &info.source_agent,
                info.id,
                info.channel_id.as_deref(),
                &username,
                &info.answer,
            )
            .await
            {
                eprintln!("  [dm-reply] notify source agent failed: {e}");
                // Record failure in context so readConsumed can detect it
                let db3 = info.db.clone();
                let reply_id = info.id;
                let err_msg = e.to_string();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok(conn) = db3.separate_conn() {
                        let _ = conn.execute(
                            "UPDATE pending_dm_replies SET context = \
                             json_set(context, '$._notify_failed', json('true'), '$._notify_error', ?1) \
                             WHERE id = ?2",
                            rusqlite::params![err_msg, reply_id],
                        );
                    }
                })
                .await;
            }

            true
        }
        Ok(None) => false,
        Err(e) => {
            eprintln!("  [dm-reply] consume task error: {e}");
            false
        }
    }
}

/// Send a notification to the source agent's Discord channel about the DM reply.
/// Prefers the stored `channel_id` from the pending row (alt/thread channels);
/// falls back to `agents.discord_channel_id` only if none was stored.
async fn notify_source_agent(
    db: &crate::db::Db,
    source_agent: &str,
    reply_id: i64,
    stored_channel_id: Option<&str>,
    username: &str,
    answer: &str,
) -> Result<(), String> {
    let token =
        crate::credential::read_bot_token("announce").ok_or("no announce bot token configured")?;

    // Prefer the stored channel_id from the pending row (supports alt/thread channels)
    let channel_id: u64 = if let Some(ch) = stored_channel_id {
        resolve_channel_to_u64(ch)?
    } else {
        // Fall back to the agent's primary discord_channel_id
        let db = db.clone();
        let agent_name = source_agent.to_string();
        let ch_opt: Option<String> = tokio::task::spawn_blocking(move || {
            let conn = db.separate_conn().map_err(|e| format!("{e}"))?;
            crate::db::agents::resolve_agent_primary_channel_on_conn(&conn, &agent_name)
                .map_err(|e| format!("{e}"))
        })
        .await
        .map_err(|e| format!("join: {e}"))??;
        let raw = ch_opt.ok_or("agent has no discord_channel_id")?;
        resolve_channel_to_u64(&raw)?
    };

    let message = format!("DM_REPLY:{reply_id} from {username}: {answer}");
    send_message_to_channel(&token, channel_id, &message)
        .await
        .map_err(|e| format!("{e}"))?;
    Ok(())
}

/// Parse a channel identifier — numeric ID or name alias (e.g. "윤호네비서") → u64.
fn resolve_channel_to_u64(raw: &str) -> Result<u64, String> {
    raw.parse::<u64>().or_else(|_| {
        crate::server::routes::dispatches::resolve_channel_alias_pub(raw)
            .ok_or_else(|| format!("cannot resolve channel '{raw}'"))
    })
}

/// Retry DM reply notifications that previously failed (`_notify_failed` in context).
/// Called from the 5-min tick loop.
pub(crate) async fn retry_failed_dm_notifications(db: &crate::db::Db) {
    let db2 = db.clone();
    let entries: Vec<(i64, String, String, Option<String>)> =
        match tokio::task::spawn_blocking(move || {
            let conn = db2.separate_conn().map_err(|e| format!("{e}"))?;
            let mut stmt = conn
                .prepare(
                    "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
                     WHERE status = 'consumed' AND json_extract(context, '$._notify_failed') IS NOT NULL \
                     LIMIT 10",
                )
                .map_err(|e| format!("{e}"))?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })
                .map_err(|e| format!("{e}"))?
                .filter_map(|r| r.ok())
                .collect::<Vec<_>>();
            Ok::<_, String>(rows)
        })
        .await
        {
            Ok(Ok(v)) => v,
            _ => return,
        };

    if entries.is_empty() {
        return;
    }

    for (id, source_agent, context_str, channel_id) in entries {
        let ctx: serde_json::Value =
            serde_json::from_str(&context_str).unwrap_or(serde_json::json!({}));
        let answer = ctx
            .get("_answer")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if answer.is_empty() {
            continue;
        }

        match notify_source_agent(
            db,
            &source_agent,
            id,
            channel_id.as_deref(),
            "(retry)",
            &answer,
        )
        .await
        {
            Ok(()) => {
                // Clear _notify_failed on success
                let db3 = db.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok(conn) = db3.separate_conn() {
                        let _ = conn.execute(
                            "UPDATE pending_dm_replies SET context = \
                             json_remove(context, '$._notify_failed', '$._notify_error') \
                             WHERE id = ?1",
                            rusqlite::params![id],
                        );
                    }
                })
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!("  [{ts}] ✉️ DM reply retry OK: id={id} agent={source_agent}");
            }
            Err(e) => {
                eprintln!("  [dm-reply] retry still failing id={id}: {e}");
            }
        }
    }
}

#[allow(dead_code)]
struct ConsumedDmReply {
    id: i64,
    source_agent: String,
    answer: String,
    channel_id: Option<String>,
    db: crate::db::Db,
}

fn consume_pending_dm_reply(
    db: &crate::db::Db,
    user_id: &str,
    answer: &str,
) -> Option<ConsumedDmReply> {
    let conn = db.separate_conn().ok()?;
    // FIFO: consume oldest non-expired pending entry
    let row: Result<(i64, String, String, Option<String>), _> = conn.query_row(
        "SELECT id, source_agent, context, channel_id FROM pending_dm_replies \
         WHERE user_id = ?1 AND status = 'pending' \
         AND (expires_at IS NULL OR expires_at > datetime('now')) \
         ORDER BY created_at ASC LIMIT 1",
        rusqlite::params![user_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    );
    let (id, source_agent, context_str, channel_id) = row.ok()?;

    // Merge the answer into the context JSON
    let mut context: serde_json::Value =
        serde_json::from_str(&context_str).unwrap_or(serde_json::json!({}));
    context["_answer"] = serde_json::Value::String(answer.to_string());
    let updated_context = serde_json::to_string(&context).unwrap_or_default();

    // CAS: only mark consumed if still pending (guards against race)
    let updated = conn.execute(
        "UPDATE pending_dm_replies SET status = 'consumed', consumed_at = datetime('now'), \
         context = ?1 WHERE id = ?2 AND status = 'pending'",
        rusqlite::params![updated_context, id],
    );
    match updated {
        Ok(0) => return None, // already consumed by another path
        Err(_) => return None,
        _ => {}
    }

    Some(ConsumedDmReply {
        id,
        source_agent,
        answer: answer.to_string(),
        channel_id,
        db: db.clone(),
    })
}

/// Rate limit helper — ensures minimum 1s gap between API calls per channel
pub(super) async fn rate_limit_wait(shared: &Arc<SharedData>, channel_id: ChannelId) {
    let min_gap = tokio::time::Duration::from_millis(1000);
    let sleep_until = {
        let now = tokio::time::Instant::now();
        let default_ts = now - tokio::time::Duration::from_secs(10);
        let last_ts = shared
            .api_timestamps
            .get(&channel_id)
            .map(|r| *r.value())
            .unwrap_or(default_ts);
        let earliest_next = last_ts + min_gap;
        let target = if earliest_next > now {
            earliest_next
        } else {
            now
        };
        shared.api_timestamps.insert(channel_id, target);
        target
    };
    tokio::time::sleep_until(sleep_until).await;
}

/// Add a reaction to a message
pub(super) async fn add_reaction(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: char,
) {
    let reaction = serenity::ReactionType::Unicode(emoji.to_string());
    if let Err(e) = channel_id
        .create_reaction(&ctx.http, message_id, reaction)
        .await
    {
        let ts = chrono::Local::now().format("%H:%M:%S");
        eprintln!(
            "  [{ts}] ⚠ Failed to add reaction '{emoji}' to msg {message_id} in channel {channel_id}: {e}"
        );
    }
}

// ─── Event handler ───────────────────────────────────────────────────────────

/// Periodic GC: delete stale idle/disconnected thread sessions from DB via cleanup API.
async fn gc_stale_thread_sessions_via_api(api_port: u16) {
    let url = crate::config::local_api_url(api_port, "/api/dispatched-sessions/gc-threads");
    match reqwest::Client::new().delete(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            if let Ok(body) = resp.json::<serde_json::Value>().await {
                let gc = body.get("gc_threads").and_then(|v| v.as_u64()).unwrap_or(0);
                if gc > 0 {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] 🧹 GC: removed {gc} stale thread session(s) from DB");
                }
            }
        }
        Ok(resp) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!(
                "  [{ts}] ⚠ Thread session GC failed: HTTP {}",
                resp.status()
            );
        }
        Err(e) => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            eprintln!("  [{ts}] ⚠ Thread session GC error: {e}");
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

/// Periodically clean up idle sessions and their associated data.
/// Called from handle_event; uses a static Mutex to track the last cleanup time.
pub(super) async fn maybe_cleanup_sessions(shared: &Arc<SharedData>) {
    use std::sync::OnceLock;
    static LAST_CLEANUP: OnceLock<tokio::sync::Mutex<tokio::time::Instant>> = OnceLock::new();
    let last = LAST_CLEANUP.get_or_init(|| tokio::sync::Mutex::new(tokio::time::Instant::now()));
    let mut last_guard = last.lock().await;
    if last_guard.elapsed() < SESSION_CLEANUP_INTERVAL {
        return;
    }
    *last_guard = tokio::time::Instant::now();
    drop(last_guard);

    let expired: Vec<(ChannelId, Option<String>)> = {
        let data = shared.core.lock().await;
        let now = tokio::time::Instant::now();
        data.sessions
            .iter()
            .filter(|(_, s)| now.duration_since(s.last_active) > SESSION_MAX_IDLE)
            .map(|(ch, s)| (*ch, s.session_id.clone()))
            .collect()
    };
    if expired.is_empty() {
        return;
    }
    // Collect session_keys for audit before removing from memory
    let expired_keys: Vec<(ChannelId, String)> = {
        let hostname = crate::services::platform::hostname_short();
        let provider = shared.settings.read().await.provider.clone();
        let data = shared.core.lock().await;
        expired
            .iter()
            .filter_map(|(ch, _)| {
                data.sessions.get(ch).and_then(|s| {
                    s.channel_name.as_ref().map(|name| {
                        let tmux_name = provider.build_tmux_session_name(name);
                        (*ch, format!("{}:{}", hostname, tmux_name))
                    })
                })
            })
            .collect()
    };
    {
        let mut data = shared.core.lock().await;
        for (ch, _) in &expired {
            // Clean up worktree if session had one
            if let Some(session) = data.sessions.get(ch)
                && let Some(ref wt) = session.worktree
            {
                cleanup_git_worktree(wt);
            }
            data.sessions.remove(ch);
            if data.cancel_tokens.remove(ch).is_some() {
                shared
                    .global_active
                    .fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
            }
            data.active_request_owner.remove(ch);
            data.intervention_queue.remove(ch);
        }
    }
    for (ch, _) in &expired {
        shared.api_timestamps.remove(ch);
        shared.tmux_watchers.remove(ch);
    }
    // Record termination audit for cleaned-up sessions
    for (_, session_key) in &expired_keys {
        crate::services::termination_audit::record_termination(
            session_key,
            None,
            "cleanup",
            "idle_session_expiry",
            Some("in-memory session expired due to idle timeout"),
            None,
            None,
            None,
        );
    }
    println!("  [cleanup] Removed {} idle session(s)", expired.len());
}

// ─── Slash commands (extracted to commands/ module) ──────────────────────────

// Command functions removed — see commands/ submodule.
// Remaining in mod.rs: detect_worktree_conflict, create_git_worktree, cleanup_git_worktree,
// send_file_to_channel, send_message_to_channel, send_message_to_user, auto_restore_session,
// bootstrap_thread_session, resolve_channel_category, and other non-command functions.

// ─── Text message → Claude AI ───────────────────────────────────────────────

/// Handle regular text messages — send to the active provider.
/// Check if a path is a git repo and if another channel already uses it.
/// Returns the conflicting channel's name if found.
pub(super) fn detect_worktree_conflict(
    sessions: &HashMap<ChannelId, DiscordSession>,
    path: &str,
    my_channel: ChannelId,
) -> Option<String> {
    let norm = path.trim_end_matches('/');
    for (cid, session) in sessions {
        if *cid == my_channel {
            continue;
        }
        let other_path = if let Some(ref wt) = session.worktree {
            &wt.original_path
        } else {
            match &session.current_path {
                Some(p) => p.as_str(),
                None => continue,
            }
        };
        if other_path.trim_end_matches('/') == norm {
            return session
                .channel_name
                .clone()
                .or_else(|| Some(cid.get().to_string()));
        }
    }
    None
}

/// Create a git worktree for the given repo path.
/// Returns (worktree_path, branch_name) on success.
pub(super) fn create_git_worktree(
    repo_path: &str,
    channel_name: &str,
    provider: &str,
) -> Result<(String, String), String> {
    let git_check = std::process::Command::new("git")
        .args(["-C", repo_path, "rev-parse", "--is-inside-work-tree"])
        .output()
        .map_err(|e| format!("git check failed: {}", e))?;
    if !git_check.status.success() {
        return Err(format!("{} is not a git repository", repo_path));
    }

    let ts = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let safe_name = channel_name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let branch = format!("wt/{}-{}-{}", provider, safe_name, ts);

    let wt_base = worktrees_root().ok_or("Cannot determine worktree root")?;
    std::fs::create_dir_all(&wt_base)
        .map_err(|e| format!("Failed to create worktree base dir: {}", e))?;
    let wt_dir = wt_base.join(format!("{}-{}-{}", provider, safe_name, ts));
    let wt_path = wt_dir.display().to_string();

    let output = std::process::Command::new("git")
        .args(["-C", repo_path, "worktree", "add", &wt_path, "-b", &branch])
        .output()
        .map_err(|e| format!("git worktree add failed: {}", e))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git worktree add failed: {}", stderr));
    }

    let ts_log = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts_log}] 🌿 Created worktree: {} (branch: {})",
        wt_path, branch
    );
    Ok((wt_path, branch))
}

/// Clean up a git worktree after session ends.
fn cleanup_git_worktree(wt_info: &WorktreeInfo) {
    let ts = chrono::Local::now().format("%H:%M:%S");

    let status = std::process::Command::new("git")
        .args(["-C", &wt_info.worktree_path, "status", "--porcelain"])
        .output();
    let has_changes = match &status {
        Ok(out) => !out.stdout.is_empty(),
        Err(_) => false,
    };

    // Check if branch has new commits
    let diff = std::process::Command::new("git")
        .args([
            "-C",
            &wt_info.original_path,
            "log",
            "--oneline",
            &format!("HEAD..{}", wt_info.branch_name),
        ])
        .output();
    let has_commits = match &diff {
        Ok(out) => !out.stdout.is_empty(),
        Err(_) => false,
    };

    if has_changes || has_commits {
        println!(
            "  [{ts}] 🌿 Worktree {} has changes/commits — keeping for manual merge",
            wt_info.worktree_path
        );
        println!(
            "  [{ts}] 🌿 Branch: {} | Original: {}",
            wt_info.branch_name, wt_info.original_path
        );
    } else {
        let _ = std::process::Command::new("git")
            .args([
                "-C",
                &wt_info.original_path,
                "worktree",
                "remove",
                &wt_info.worktree_path,
            ])
            .output();
        let _ = std::process::Command::new("git")
            .args([
                "-C",
                &wt_info.original_path,
                "branch",
                "-d",
                &wt_info.branch_name,
            ])
            .output();
        println!(
            "  [{ts}] 🌿 Cleaned up worktree: {} (no changes)",
            wt_info.worktree_path
        );
    }
}

// ─── File upload handling ────────────────────────────────────────────────────

// ─── Sendfile (CLI) ──────────────────────────────────────────────────────────

/// Send a file to a Discord channel (called from CLI --discord-sendfile)
pub(crate) async fn send_file_to_channel(
    token: &str,
    channel_id: u64,
    file_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let path = Path::new(file_path);
    if !path.exists() {
        return Err(format!("File not found: {}", file_path).into());
    }

    let http = serenity::Http::new(token);

    let channel = ChannelId::new(channel_id);
    let attachment = CreateAttachment::path(path).await?;

    channel
        .send_message(
            &http,
            CreateMessage::new()
                .content(format!(
                    "📎 {}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ))
                .add_file(attachment),
        )
        .await?;

    Ok(())
}

/// Send a text message to a Discord channel (called from CLI --discord-sendmessage)
pub(crate) async fn send_message_to_channel(
    token: &str,
    channel_id: u64,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http = serenity::Http::new(token);
    let channel = ChannelId::new(channel_id);

    channel
        .send_message(&http, CreateMessage::new().content(message))
        .await?;

    Ok(())
}

/// Send a text message to a Discord user DM (called from CLI --discord-senddm)
pub(crate) async fn send_message_to_user(
    token: &str,
    user_id: u64,
    message: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let http = serenity::Http::new(token);
    let dm_channel = UserId::new(user_id).create_dm_channel(&http).await?;

    dm_channel
        .id
        .send_message(&http, CreateMessage::new().content(message))
        .await?;

    Ok(())
}

// ─── Session persistence ─────────────────────────────────────────────────────

/// Auto-restore session from bot_settings.json if not in memory
pub(super) async fn auto_restore_session(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    serenity_ctx: &serenity::prelude::Context,
) {
    // Resolve channel/category before taking the lock for mutation
    let (live_ch_name, cat_name) = resolve_channel_category(serenity_ctx, channel_id).await;
    let existing_channel_name = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
    };
    let restore_ch_name = choose_restore_channel_name(
        existing_channel_name.as_deref(),
        live_ch_name.as_deref(),
        resolve_thread_parent(&serenity_ctx.http, channel_id).await,
        channel_id,
    );

    // Read settings first to get last_sessions/last_remotes info
    // DB cwd takes priority over yaml last_sessions (preserves worktree paths)
    let (last_path, saved_remote, provider) = {
        let settings = shared.settings.read().await;
        let channel_key = channel_id.get().to_string();
        let yaml_path = settings.last_sessions.get(&channel_key).cloned();
        let saved_remote = settings.last_remotes.get(&channel_key).cloned();
        let provider = settings.provider.clone();

        // Use the effective tmux channel name here so restart recovery keeps
        // looking up the same session key for thread sessions that intentionally
        // use a synthetic "{parent}-t{thread_id}" channel name.
        let db_cwd: Option<String> = restore_ch_name.as_ref().and_then(|ch| {
            let tmux_name = provider.build_tmux_session_name(ch);
            let hostname = crate::services::platform::hostname_short();
            let session_key = format!("{}:{}", hostname, tmux_name);
            shared.db.as_ref().and_then(|db| {
                db.lock().ok().and_then(|conn| {
                    conn.query_row(
                        "SELECT cwd FROM sessions WHERE session_key = ?1",
                        [&session_key],
                        |row| row.get::<_, String>(0),
                    )
                    .ok()
                    .filter(|p| !p.is_empty() && session_path_is_usable(p, saved_remote.as_deref()))
                })
            })
        });
        let last_path = db_cwd.or(yaml_path);

        (last_path, saved_remote, provider)
    };

    let mut data = shared.core.lock().await;
    if let Some(session) = data.sessions.get_mut(&channel_id) {
        session.channel_id = Some(channel_id.get());
        session.last_active = tokio::time::Instant::now();
        session.channel_name = restore_ch_name.clone();
        session.category_name = cat_name.clone();
        if session.remote_profile_name.is_none() {
            session.remote_profile_name = saved_remote.clone();
        }
        if session.current_path.is_some() || last_path.is_none() {
            return;
        }
    }

    if let Some(last_path) = last_path
        && session_path_is_usable(&last_path, saved_remote.as_deref())
    {
        // Session ID is restored from DB (sessions.claude_session_id column)
        // which is already loaded into DiscordSession.session_id at startup.
        let session = data
            .sessions
            .entry(channel_id)
            .or_insert_with(|| DiscordSession {
                session_id: None,
                current_path: None,
                history: Vec::new(),
                pending_uploads: Vec::new(),
                cleared: false,
                channel_id: Some(channel_id.get()),
                channel_name: restore_ch_name.clone(),
                category_name: cat_name.clone(),
                remote_profile_name: saved_remote.clone(),

                last_active: tokio::time::Instant::now(),
                worktree: None,

                born_generation: runtime_store::load_generation(),
            });
        session.channel_id = Some(channel_id.get());
        session.last_active = tokio::time::Instant::now();
        session.channel_name = restore_ch_name.clone();
        session.category_name = cat_name.clone();
        if session.remote_profile_name.is_none() {
            session.remote_profile_name = saved_remote.clone();
        }
        session.current_path = Some(last_path.clone());
        drop(data);

        // Rescan skills with project path
        let new_skills = scan_skills(&provider, Some(&last_path));
        *shared.skills_cache.write().await = new_skills;
        let ts = chrono::Local::now().format("%H:%M:%S");
        let remote_info = saved_remote
            .as_ref()
            .map(|n| format!(" (remote: {})", n))
            .unwrap_or_default();
        println!("  [{ts}] ↻ Auto-restored session: {last_path}{remote_info}");
    }
}

/// Create a lightweight session for a thread, bootstrapped from the parent channel's path.
/// The session's `channel_name` uses `{parent_channel}-t{thread_id}` so the derived
/// tmux session name stays short and unique instead of using the full thread title.
pub(super) async fn bootstrap_thread_session(
    shared: &Arc<SharedData>,
    thread_channel_id: ChannelId,
    parent_path: &str,
    serenity_ctx: &serenity::prelude::Context,
) {
    let (_thread_title, cat_name) = resolve_channel_category(serenity_ctx, thread_channel_id).await;
    // Build a short, stable channel_name: "{parent_channel}-t{thread_id}"
    let parent_info = resolve_thread_parent(&serenity_ctx.http, thread_channel_id).await;
    let ch_name = if let Some((_parent_id, parent_name)) = parent_info {
        let parent = parent_name.unwrap_or_else(|| format!("{}", _parent_id));
        Some(synthetic_thread_channel_name(&parent, thread_channel_id))
    } else {
        // Not a thread (shouldn't happen here) — fall back to resolved name
        _thread_title
    };
    let mut data = shared.core.lock().await;
    if data.sessions.contains_key(&thread_channel_id) {
        return;
    }

    // Session ID comes from DB (sessions.claude_session_id), not from file.
    let session = data
        .sessions
        .entry(thread_channel_id)
        .or_insert_with(|| DiscordSession {
            session_id: None,
            current_path: None,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            channel_id: Some(thread_channel_id.get()),
            channel_name: ch_name,
            category_name: cat_name,
            remote_profile_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: runtime_store::load_generation(),
        });
    session.current_path = Some(parent_path.to_string());
    let ts = chrono::Local::now().format("%H:%M:%S");
    println!("  [{ts}] ↻ Bootstrapped thread session from parent path: {parent_path}");
}

/// Resolve the channel name and parent category name for a Discord channel.
pub(super) async fn resolve_channel_category(
    ctx: &serenity::prelude::Context,
    channel_id: serenity::model::id::ChannelId,
) -> (Option<String>, Option<String>) {
    let Ok(channel) = channel_id.to_channel(&ctx.http).await else {
        return (None, None);
    };
    let serenity::model::channel::Channel::Guild(gc) = channel else {
        return (None, None);
    };
    let ch_name = Some(gc.name.clone());
    let cat_name = if let Some(parent_id) = gc.parent_id {
        let cached_cat_name = ctx.cache.guild(gc.guild_id).and_then(|guild| {
            guild
                .channels
                .get(&parent_id)
                .map(|parent_ch| parent_ch.name.clone())
        });

        if let Some(cat_name) = cached_cat_name {
            Some(cat_name)
        } else if let Ok(parent_ch) = parent_id.to_channel(&ctx.http).await {
            match parent_ch {
                serenity::model::channel::Channel::Guild(cat) => Some(cat.name.clone()),
                _ => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ⚠ Category channel {parent_id} is not a Guild channel for #{}",
                        gc.name
                    );
                    None
                }
            }
        } else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ⚠ Failed to resolve category {parent_id} for #{}",
                gc.name
            );
            None
        }
    } else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] ⚠ No parent_id for #{}", gc.name);
        None
    };
    (ch_name, cat_name)
}

pub(super) async fn provider_handles_channel(
    ctx: &serenity::prelude::Context,
    provider: &ProviderKind,
    settings: &DiscordBotSettings,
    channel_id: serenity::model::id::ChannelId,
) -> bool {
    let is_dm = matches!(
        channel_id.to_channel(&ctx.http).await,
        Ok(serenity::model::channel::Channel::Private(_))
    );
    let (channel_name, _) = resolve_channel_category(ctx, channel_id).await;
    let (effective_channel_id, effective_channel_name) = if let Some((parent_id, parent_name)) =
        resolve_thread_parent(&ctx.http, channel_id).await
    {
        (parent_id, parent_name.or(channel_name))
    } else {
        (channel_id, channel_name)
    };
    validate_bot_channel_routing(
        settings,
        provider,
        effective_channel_id,
        effective_channel_name.as_deref(),
        is_dm,
    )
    .is_ok()
}

/// If `channel_id` is a Discord thread, return the parent channel ID and name.
/// For non-thread channels, returns `None`.
pub(super) async fn resolve_thread_parent(
    http: &Arc<serenity::Http>,
    channel_id: serenity::model::id::ChannelId,
) -> Option<(serenity::model::id::ChannelId, Option<String>)> {
    let channel = channel_id.to_channel(http).await.ok()?;
    let serenity::model::channel::Channel::Guild(gc) = channel else {
        return None;
    };
    use poise::serenity_prelude::model::channel::ChannelType;
    match gc.kind {
        ChannelType::PublicThread | ChannelType::PrivateThread => {
            let parent_id = gc.parent_id?;
            let parent_name = if let Ok(parent_ch) = parent_id.to_channel(http).await {
                match parent_ch {
                    serenity::model::channel::Channel::Guild(pg) => Some(pg.name.clone()),
                    _ => None,
                }
            } else {
                None
            };
            Some((parent_id, parent_name))
        }
        _ => None,
    }
}

/// Enrich role_map.json's byChannelName entries with channelId from byChannelId.
/// This enables reliable channel name → ID resolution without provider inference hacks.
fn enrich_role_map_with_channel_ids() {
    let Some(root) = crate::cli::agentdesk_runtime_root() else {
        return;
    };
    let path = root.join("config/role_map.json");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return;
    };
    let Ok(mut json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return;
    };

    let mut changed = false;

    // Build maps from byChannelId: channelId → (roleId, provider) and name→id lookup
    let by_id = json
        .get("byChannelId")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    // Pass 1: collect mappings (name → channelId) without mutating
    let mut mappings: Vec<(String, String)> = Vec::new();
    if let Some(by_name) = json.get("byChannelName").and_then(|v| v.as_object()) {
        // Collect already-assigned IDs to avoid duplicates
        let already_assigned: std::collections::HashSet<String> = by_name
            .iter()
            .filter_map(|(_, e)| {
                e.get("channelId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();

        for (name, entry) in by_name {
            if entry.get("channelId").is_some() {
                continue;
            }
            let role_id = entry.get("roleId").and_then(|v| v.as_str()).unwrap_or("");
            let entry_provider = entry.get("provider").and_then(|v| v.as_str());

            let candidates: Vec<(&String, &serde_json::Value)> = by_id
                .iter()
                .filter(|(_, e)| e.get("roleId").and_then(|v| v.as_str()) == Some(role_id))
                .collect();

            let ch_id = if candidates.len() == 1 {
                Some(candidates[0].0.clone())
            } else if candidates.len() > 1 {
                if let Some(p) = entry_provider {
                    // Explicit provider — exact match
                    candidates
                        .iter()
                        .find(|(_, e)| e.get("provider").and_then(|v| v.as_str()) == Some(p))
                        .map(|(id, _)| id.to_string())
                } else {
                    // No provider in byChannelName — match by expected provider type:
                    // Claude channels are the "primary" (cc suffix or no suffix)
                    // Codex channels are the "alt" (cdx suffix)
                    // This determines which byChannelId entry to pick.
                    let expected_provider = if name.ends_with("-cdx") {
                        "codex"
                    } else {
                        "claude"
                    };
                    candidates
                        .iter()
                        .find(|(_, e)| {
                            e.get("provider").and_then(|v| v.as_str()) == Some(expected_provider)
                        })
                        .map(|(id, _)| id.to_string())
                        .or_else(|| {
                            // Fallback: pick one not already assigned
                            candidates
                                .iter()
                                .find(|(id, _)| !already_assigned.contains(id.as_str()))
                                .map(|(id, _)| id.to_string())
                        })
                }
            } else {
                None
            };

            if let Some(id) = ch_id {
                mappings.push((name.clone(), id));
            }
        }
    }

    // Pass 2: apply mappings
    if let Some(by_name) = json
        .get_mut("byChannelName")
        .and_then(|v| v.as_object_mut())
    {
        for (name, ch_id) in &mappings {
            if let Some(entry) = by_name.get_mut(name)
                && let Some(obj) = entry.as_object_mut()
            {
                obj.insert("channelId".to_string(), serde_json::json!(ch_id));
                changed = true;
            }
        }
    }

    if changed && let Ok(pretty) = serde_json::to_string_pretty(&json) {
        let _ = runtime_store::atomic_write(&path, &pretty);
    }
}
