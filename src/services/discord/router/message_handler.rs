use super::super::*;
use crate::services::memory::{
    RecallRequest, RecallResponse, build_memory_backend, resolve_memory_role_id,
    resolve_memory_session_id,
};

#[derive(Debug, PartialEq, Eq)]
struct MemoryInjectionPlan<'a> {
    shared_knowledge_for_context: Option<&'a str>,
    shared_knowledge_for_system_prompt: Option<&'a str>,
    external_recall_for_context: Option<&'a str>,
    longterm_catalog_for_system_prompt: Option<&'a str>,
}

fn build_memory_injection_plan<'a>(
    provider: &ProviderKind,
    has_session_id: bool,
    dispatch_profile: DispatchProfile,
    memory_recall: &'a RecallResponse,
) -> MemoryInjectionPlan<'a> {
    let should_inject_shared_knowledge =
        dispatch_profile == DispatchProfile::Full && !has_session_id;
    let shared_knowledge_for_context =
        if should_inject_shared_knowledge && !matches!(provider, ProviderKind::Claude) {
            memory_recall.shared_knowledge.as_deref()
        } else {
            None
        };
    let shared_knowledge_for_system_prompt =
        if dispatch_profile == DispatchProfile::Full && matches!(provider, ProviderKind::Claude) {
            memory_recall.shared_knowledge.as_deref()
        } else {
            None
        };
    let external_recall_for_context = if dispatch_profile == DispatchProfile::Full {
        memory_recall.external_recall.as_deref()
    } else {
        None
    };
    let longterm_catalog_for_system_prompt = if dispatch_profile == DispatchProfile::Full {
        memory_recall.longterm_catalog.as_deref()
    } else {
        None
    };

    MemoryInjectionPlan {
        shared_knowledge_for_context,
        shared_knowledge_for_system_prompt,
        external_recall_for_context,
        longterm_catalog_for_system_prompt,
    }
}

pub(in crate::services::discord) async fn handle_text_message(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    request_owner: UserId,
    request_owner_name: &str,
    user_text: &str,
    shared: &Arc<SharedData>,
    token: &str,
    reply_to_user_message: bool,
    defer_watcher_resume: bool,
    wait_for_completion: bool,
    reply_context: Option<String>,
) -> Result<(), Error> {
    // Get session info, allowed tools, and pending uploads
    let (session_info, provider, allowed_tools, pending_uploads) = {
        let mut data = shared.core.lock().await;
        let info = data.sessions.get_mut(&channel_id).and_then(|session| {
            let current_path = session.validated_path(channel_id)?;
            Some((session.session_id.clone(), current_path))
        });
        let uploads = data
            .sessions
            .get_mut(&channel_id)
            .map(|s| {
                s.cleared = false;
                std::mem::take(&mut s.pending_uploads)
            })
            .unwrap_or_default();
        drop(data);
        let settings = shared.settings.read().await;
        (
            info,
            settings.provider.clone(),
            settings.allowed_tools.clone(),
            uploads,
        )
    };

    let (session_id, current_path) = match session_info {
        Some(info) => info,
        None => {
            // Try auto-start from role_map workspace
            let ch_name = {
                let data = shared.core.lock().await;
                data.sessions
                    .get(&channel_id)
                    .and_then(|s| s.channel_name.clone())
            };
            let mut workspace = settings::resolve_workspace(channel_id, ch_name.as_deref());
            // Fallback: if this is a thread, try resolving workspace from parent channel
            if workspace.is_none() {
                if let Some((parent_id, parent_name)) =
                    super::super::resolve_thread_parent(&ctx.http, channel_id).await
                {
                    // Use parent name from Discord API first, fall back to session map
                    let parent_ch_name = parent_name.or_else(|| {
                        let data = shared.core.try_lock().ok()?;
                        data.sessions
                            .get(&parent_id)
                            .and_then(|s| s.channel_name.clone())
                    });
                    workspace = settings::resolve_workspace(parent_id, parent_ch_name.as_deref());
                    if workspace.is_some() {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!(
                            "  [{ts}] 🧵 Thread auto-start: resolved workspace from parent channel {}",
                            parent_id
                        );
                    }
                }
            }
            if let Some(ws_path) = workspace {
                let ws = std::path::Path::new(&ws_path);
                if ws.is_dir() {
                    let canonical = ws
                        .canonicalize()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| ws_path.clone());
                    // Resolve channel name from Discord API before worktree
                    // creation so the path uses the real name, not "unknown".
                    let (ch_name_api, cat_name) = resolve_channel_category(ctx, channel_id).await;
                    let ch_name = match super::super::resolve_thread_parent(&ctx.http, channel_id)
                        .await
                    {
                        Some((_parent_id, parent_name)) => {
                            let parent = parent_name.unwrap_or_else(|| format!("{}", _parent_id));
                            Some(super::super::synthetic_thread_channel_name(
                                &parent, channel_id,
                            ))
                        }
                        None => ch_name_api,
                    };

                    // Check worktree: always use worktree for thread sessions,
                    // or when conflict detected with another session on same path.
                    let wt_info = {
                        let is_thread = shared.dispatch_thread_parents.contains_key(&channel_id);
                        let data = shared.core.lock().await;
                        let conflict =
                            detect_worktree_conflict(&data.sessions, &canonical, channel_id);
                        drop(data);
                        let needs_worktree = is_thread || conflict.is_some();
                        if needs_worktree {
                            let reason = if is_thread {
                                "thread session"
                            } else {
                                "conflict"
                            };
                            let ch = ch_name.as_deref().unwrap_or("unknown");
                            match create_git_worktree(&canonical, ch, provider.as_str()) {
                                Ok((wt_path, branch)) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    println!(
                                        "  [{ts}] 🌿 Auto-start worktree ({reason}): {ch} → {}",
                                        wt_path
                                    );
                                    Some(WorktreeInfo {
                                        original_path: canonical.clone(),
                                        worktree_path: wt_path,
                                        branch_name: branch,
                                    })
                                }
                                Err(_) => None,
                            }
                        } else {
                            None
                        }
                    };
                    let eff_path = wt_info
                        .as_ref()
                        .map(|wt| wt.worktree_path.clone())
                        .unwrap_or_else(|| canonical.clone());
                    {
                        let mut data = shared.core.lock().await;
                        let session =
                            data.sessions
                                .entry(channel_id)
                                .or_insert_with(|| DiscordSession {
                                    session_id: None,
                                    current_path: None,
                                    history: Vec::new(),
                                    pending_uploads: Vec::new(),
                                    cleared: false,
                                    channel_name: None,
                                    category_name: None,
                                    remote_profile_name: None,
                                    channel_id: Some(channel_id.get()),
                                    last_active: tokio::time::Instant::now(),
                                    worktree: None,

                                    born_generation: super::super::runtime_store::load_generation(),
                                });
                        session.current_path = Some(eff_path.clone());
                        session.channel_name = ch_name;
                        session.category_name = cat_name;
                        session.channel_id = Some(channel_id.get());
                        session.last_active = tokio::time::Instant::now();
                        session.worktree = wt_info;
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ▶ Auto-started session from workspace: {eff_path}");
                    let sid = {
                        let data = shared.core.lock().await;
                        data.sessions
                            .get(&channel_id)
                            .and_then(|s| s.session_id.clone())
                    };
                    (sid, eff_path)
                } else {
                    rate_limit_wait(shared, channel_id).await;
                    let _ = channel_id
                        .say(&ctx.http, "No active session. Use `/start <path>` first.")
                        .await;
                    return Ok(());
                }
            } else {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(&ctx.http, "No active session. Use `/start <path>` first.")
                    .await;
                return Ok(());
            }
        }
    };

    // Add hourglass reaction to user's message
    add_reaction(ctx, channel_id, user_msg_id, '⏳').await;

    // ── Dispatch thread auto-creation ──────────────────────────────
    // When a dispatch message arrives, create a Discord thread for
    // isolated context.  All subsequent agent output goes to the thread.
    // Skip if already inside a thread (threads cannot nest).
    // Thread reuse: if the card already has an active_thread_id, redirect
    // to the existing thread instead of creating a new one.
    let is_already_thread = super::super::resolve_thread_parent(&ctx.http, channel_id)
        .await
        .is_some();
    let dispatch_id_for_thread = super::super::adk_session::parse_dispatch_id(user_text);
    let mut dispatch_type_str: Option<String> = None;
    // #259: Fetch dispatch metadata once before thread creation so we can extract
    // worktree_path for both thread bootstrap and the subsequent session CWD override.
    let dispatch_info_cached = if let Some(ref did) = dispatch_id_for_thread {
        super::lookup_dispatch_info(shared.api_port, did).await
    } else {
        None
    };
    // #259: Prefer card-bound worktree over parent channel CWD for dispatch sessions.
    // All dispatch types now inject worktree_path into context via resolve_card_worktree().
    let dispatch_worktree_path = dispatch_info_cached
        .as_ref()
        .and_then(|info| {
            info.context
                .as_ref()
                .and_then(|ctx_str| serde_json::from_str::<serde_json::Value>(ctx_str).ok())
                .and_then(|v| v.get("worktree_path")?.as_str().map(String::from))
        })
        .filter(|p| std::path::Path::new(p).exists());
    if let (Some(wt), Some(did)) = (&dispatch_worktree_path, &dispatch_id_for_thread) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}] 🌿 Dispatch {did}: resolved worktree CWD: {wt}");
    }
    let dispatch_default_path = crate::services::platform::resolve_repo_dir()
        .filter(|p| std::path::Path::new(p).is_dir())
        .unwrap_or_else(|| current_path.clone());
    let dispatch_effective_path = dispatch_worktree_path
        .clone()
        .unwrap_or_else(|| dispatch_default_path.clone());
    if dispatch_worktree_path.is_none() && dispatch_id_for_thread.is_some() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] 🌱 Dispatch fallback CWD: using repo root instead of inherited session path: {}",
            dispatch_effective_path
        );
    }
    let channel_id = if let Some(ref did) = dispatch_id_for_thread {
        // Use cached dispatch metadata for thread reuse and cross-channel role override
        let dispatch_info = &dispatch_info_cached;
        dispatch_type_str = dispatch_info.as_ref().and_then(|i| i.dispatch_type.clone());
        let is_counter_model_dispatch =
            crate::server::routes::dispatches::use_counter_model_channel(
                dispatch_type_str.as_deref(),
            );
        let alt_channel_id = dispatch_info
            .as_ref()
            .and_then(|i| i.discord_channel_alt.as_deref())
            .and_then(|s| s.parse::<u64>().ok())
            .map(ChannelId::new);

        if is_already_thread {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 🧵 Dispatch {did} arrived in existing thread, skipping thread creation"
            );
            // For review dispatches in reused threads, set role override
            // so this turn uses the counter-model channel's role/model.
            if is_counter_model_dispatch {
                if let Some(alt_ch) = alt_channel_id {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] 🔄 Review dispatch in reused thread: overriding role to alt channel {}",
                        alt_ch
                    );
                    shared.dispatch_role_overrides.insert(channel_id, alt_ch);
                }
            }
            channel_id
        } else {
            // Check if card already has an active thread via internal API
            let existing_thread = dispatch_info
                .as_ref()
                .and_then(|i| i.active_thread_id.clone());
            let reuse_tid = existing_thread.as_ref().and_then(|t| {
                let id = t.parse::<u64>().unwrap_or(0);
                if id != 0 {
                    Some(ChannelId::new(id))
                } else {
                    None
                }
            });

            let reused = if let Some(tid) = reuse_tid {
                if super::verify_thread_accessible(ctx, tid).await {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] 🧵 Reusing existing thread {} for dispatch {}",
                        tid, did
                    );
                    super::super::bootstrap_thread_session(
                        shared,
                        tid,
                        &dispatch_effective_path,
                        ctx,
                    )
                    .await;
                    shared.dispatch_thread_parents.insert(channel_id, tid);
                    // For review dispatches reusing an implementation thread,
                    // override role/model to use the counter-model channel.
                    if is_counter_model_dispatch {
                        if let Some(alt_ch) = alt_channel_id {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            println!(
                                "  [{ts}] 🔄 Review dispatch reusing thread: overriding role to alt channel {}",
                                alt_ch
                            );
                            shared.dispatch_role_overrides.insert(tid, alt_ch);
                        }
                    }
                    Some(tid)
                } else {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] 🧵 Thread {} is locked/inaccessible, creating new for {}",
                        tid, did
                    );
                    None
                }
            } else {
                None
            };

            if let Some(tid) = reused {
                tid
            } else {
                // No existing usable thread — create new
                let thread_title = user_text
                    .find(" - ")
                    .map(|idx| &user_text[idx + 3..])
                    .unwrap_or("dispatch")
                    .chars()
                    .take(90)
                    .collect::<String>();

                match channel_id
                    .create_thread(
                        &ctx.http,
                        poise::serenity_prelude::builder::CreateThread::new(thread_title)
                            .kind(poise::serenity_prelude::ChannelType::PublicThread)
                            .auto_archive_duration(
                                poise::serenity_prelude::AutoArchiveDuration::OneDay,
                            ),
                    )
                    .await
                {
                    Ok(thread) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        println!(
                            "  [{ts}] 🧵 Created dispatch thread {} for dispatch {}",
                            thread.id, did
                        );
                        super::super::bootstrap_thread_session(
                            shared,
                            thread.id,
                            &dispatch_effective_path,
                            ctx,
                        )
                        .await;
                        shared.dispatch_thread_parents.insert(channel_id, thread.id);
                        super::link_dispatch_thread(
                            shared.api_port,
                            did,
                            thread.id.get(),
                            channel_id.get(),
                        )
                        .await;
                        thread.id
                    }
                    Err(e) => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        eprintln!("  [{ts}] ⚠ Failed to create dispatch thread: {e}");
                        channel_id // fallback to main channel
                    }
                }
            }
        }
    } else {
        channel_id
    };

    // #259: Override current_path with the pre-computed dispatch worktree path.
    // Also update the in-memory session so the worktree sticks for subsequent turns.
    let current_path = if dispatch_worktree_path.is_some() {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.current_path = Some(dispatch_effective_path.clone());
        }
        dispatch_effective_path.clone()
    } else {
        current_path
    };

    // Send placeholder message
    rate_limit_wait(shared, channel_id).await;
    let placeholder = channel_id
        .send_message(&ctx.http, {
            let builder = CreateMessage::new().content("...");
            if reply_to_user_message && dispatch_id_for_thread.is_none() {
                builder.reference_message((channel_id, user_msg_id))
            } else {
                builder
            }
        })
        .await?;
    let placeholder_msg_id = placeholder.id;

    // Sanitize input
    let sanitized_input = ai_screen::sanitize_user_input(user_text);

    let role_binding = {
        // For cross-channel dispatch reuse (e.g. review in implementation thread),
        // resolve role from the override channel instead of the thread's parent.
        if let Some(override_ch) = shared.dispatch_role_overrides.get(&channel_id) {
            let alt_ch = *override_ch;
            resolve_role_binding(alt_ch, None)
        } else {
            let data = shared.core.lock().await;
            let ch_name = data
                .sessions
                .get(&channel_id)
                .and_then(|s| s.channel_name.as_deref());
            resolve_role_binding(channel_id, ch_name)
        }
    };

    // For cross-channel dispatch reuse, override the provider so the turn
    // executes via the counter-model CLI (e.g. Codex reviews Claude's work).
    let provider = if shared.dispatch_role_overrides.contains_key(&channel_id) {
        role_binding
            .as_ref()
            .and_then(|rb| rb.provider.clone())
            .unwrap_or(provider)
    } else {
        provider
    };

    // Derive dispatch prompt profile before memory recall so ReviewLite can
    // skip heavy memory work consistently across local/mem0 backends.
    let dispatch_profile = DispatchProfile::from_dispatch_type(
        dispatch_id_for_thread
            .as_ref()
            .and_then(|_| dispatch_type_str.as_deref()),
    );

    reset_provider_session_if_pending(shared, channel_id, &provider).await;
    let prompt_prep_started = std::time::Instant::now();

    // Resolve channel/tmux session name from current session state. We need the
    // persisted provider session_id before recall so Mem0 can scope search by run_id.
    let (channel_name, tmux_session_name) = {
        let data = shared.core.lock().await;
        let channel_name = data
            .sessions
            .get(&channel_id)
            .and_then(|s| s.channel_name.clone());
        let tmux_session_name = channel_name
            .as_ref()
            .map(|name| provider.build_tmux_session_name(name));
        (channel_name, tmux_session_name)
    };
    let adk_session_key = build_adk_session_key(shared, channel_id, &provider).await;
    let mut session_id = session_id;
    if session_id.is_none() {
        if let Some(ref key) = adk_session_key {
            let restored = super::super::adk_session::fetch_provider_session_id(
                key,
                &provider,
                shared.api_port,
            )
            .await;
            if restored.is_some() {
                let ts = chrono::Local::now().format("%H:%M:%S");
                println!(
                    "  [{ts}] ↻ Restored provider session_id from DB for {}",
                    key
                );
                let mut data = shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.session_id = restored.clone();
                }
            }
            session_id = restored;
        }
    }

    let memory_settings = settings::memory_settings_for_binding(role_binding.as_ref());
    let memory_backend = build_memory_backend(&memory_settings);
    let memory_recall = memory_backend
        .recall(RecallRequest {
            provider: provider.clone(),
            role_id: resolve_memory_role_id(role_binding.as_ref()),
            channel_id: channel_id.get(),
            session_id: resolve_memory_session_id(session_id.as_deref(), channel_id.get()),
            dispatch_profile,
            user_text: user_text.to_string(),
        })
        .await;
    for warning in &memory_recall.warnings {
        let ts = chrono::Local::now().format("%H:%M:%S");
        eprintln!(
            "  [{ts}] [memory] recall warning for channel {}: {}",
            channel_id.get(),
            warning
        );
    }

    // Prepend pending file uploads
    let mut context_chunks = Vec::new();
    let memory_injection_plan = build_memory_injection_plan(
        &provider,
        session_id.is_some(),
        dispatch_profile,
        &memory_recall,
    );
    if !pending_uploads.is_empty() {
        context_chunks.push(pending_uploads.join("\n"));
    }
    if let Some(ref reply_ctx) = reply_context {
        context_chunks.push(reply_ctx.clone());
    }
    // Re-inject formatting + compaction reminder for interactive follow-up
    // turns. System prompt is only sent at session creation; after context
    // compaction these rules can be lost.
    if session_id.is_some() {
        context_chunks.push(super::super::prompt_builder::build_followup_turn_system_reminder());
    }
    if let Some(knowledge) = memory_injection_plan.shared_knowledge_for_context {
        context_chunks.push(knowledge.to_string());
    }
    if let Some(external_recall) = memory_injection_plan.external_recall_for_context {
        context_chunks.push(external_recall.to_string());
    }
    context_chunks.push(sanitized_input);
    let context_prompt = context_chunks.join("\n\n");

    // Build disabled tools notice
    let default_tools: std::collections::HashSet<&str> =
        DEFAULT_ALLOWED_TOOLS.iter().copied().collect();
    let allowed_set: std::collections::HashSet<&str> =
        allowed_tools.iter().map(|s| s.as_str()).collect();
    let disabled: Vec<&&str> = default_tools
        .iter()
        .filter(|t| !allowed_set.contains(**t))
        .collect();
    let disabled_notice = if disabled.is_empty() {
        String::new()
    } else {
        let names: Vec<&str> = disabled.iter().map(|t| **t).collect();
        format!(
            "\n\nDISABLED TOOLS: The following tools have been disabled by the user: {}.\n\
             You MUST NOT attempt to use these tools. \
             If a user's request requires a disabled tool, do NOT proceed with the task. \
             Instead, clearly inform the user which tool is needed and that it is currently disabled. \
             Suggest they re-enable it with: /allowed +ToolName",
            names.join(", ")
        )
    };

    // Build skills notice for system prompt
    let skills_notice = {
        let skills = shared.skills_cache.read().await;
        format_skills_notice(&provider, &skills)
    };

    // Build Discord context info
    let discord_context = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id);
        let ch_name = session.and_then(|s| s.channel_name.as_deref());
        let cat_name = session.and_then(|s| s.category_name.as_deref());
        match ch_name {
            Some(name) => {
                let cat_part = cat_name
                    .map(|c| format!(" (category: {})", c))
                    .unwrap_or_default();
                format!(
                    "Discord context: channel #{} (ID: {}){}, user: {} (ID: {})",
                    name,
                    channel_id.get(),
                    cat_part,
                    request_owner_name,
                    request_owner.get()
                )
            }
            None => format!(
                "Discord context: DM, user: {} (ID: {})",
                request_owner_name,
                request_owner.get()
            ),
        }
    };

    // Claude keeps SAK in the system prompt for prefix-cache stability.
    // Non-Claude providers receive SAK in the user context instead.
    let sak_for_system = memory_injection_plan.shared_knowledge_for_system_prompt;
    let longterm_catalog_for_prompt = memory_injection_plan.longterm_catalog_for_system_prompt;

    let system_prompt_owned = build_system_prompt(
        &discord_context,
        &current_path,
        channel_id,
        token,
        &disabled_notice,
        &skills_notice,
        role_binding.as_ref(),
        reply_to_user_message,
        dispatch_profile,
        dispatch_type_str.as_deref(),
        sak_for_system,
        longterm_catalog_for_prompt,
    );
    if sak_for_system.is_some() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!(
            "  [{ts}] 📦 SAK in system prompt ({} chars) for channel {}",
            sak_for_system.unwrap().len(),
            channel_id.get()
        );
    }
    let prompt_prep_duration_ms = prompt_prep_started.elapsed().as_millis();
    let memory_backend_label = match memory_settings.backend {
        settings::MemoryBackendKind::Local => "local",
        settings::MemoryBackendKind::Mem0 => "mem0",
    };
    let provider_label = match &provider {
        ProviderKind::Claude => "claude",
        ProviderKind::Codex => "codex",
        ProviderKind::Gemini => "gemini",
        ProviderKind::Qwen => "qwen",
        ProviderKind::Unsupported(_) => "unsupported",
    };
    let dispatch_profile_label = match dispatch_profile {
        DispatchProfile::Full => "full",
        DispatchProfile::ReviewLite => "review_lite",
    };
    let ts = chrono::Local::now().format("%H:%M:%S");
    println!(
        "  [{ts}] [prompt-prep] channel={} provider={} dispatch={} memory_backend={} memory_profile={} reused_session={} duration_ms={}",
        channel_id.get(),
        provider_label,
        dispatch_profile_label,
        memory_backend_label,
        memory_settings.mem0.profile,
        session_id.is_some(),
        prompt_prep_duration_ms
    );

    // Create cancel token — with second check to close the TOCTOU race window.
    // Multiple messages can pass the initial cancel_tokens check (line 169) concurrently
    // because the async gap between check and insert allows interleaving.
    // If another message won the race, queue ourselves and clean up.
    let cancel_token = Arc::new(CancelToken::new());
    {
        let mut data = shared.core.lock().await;
        if data.cancel_tokens.contains_key(&channel_id) {
            // Race lost — another message already started a turn.
            // Queue this message as an intervention instead.
            let queue = data.intervention_queue.entry(channel_id).or_default();
            super::super::enqueue_intervention(
                queue,
                super::super::Intervention {
                    author_id: request_owner,
                    message_id: user_msg_id,
                    text: user_text.to_string(),
                    mode: super::super::InterventionMode::Soft,
                    created_at: std::time::Instant::now(),
                },
            );
            // Write-through: persist this channel's queue to disk
            if let Some(q) = data.intervention_queue.get(&channel_id) {
                super::super::save_channel_queue(
                    &provider,
                    &shared.token_hash,
                    channel_id,
                    q,
                    shared
                        .dispatch_role_overrides
                        .get(&channel_id)
                        .map(|r| r.value().get()),
                );
            }
            drop(data);
            // Clean up: remove placeholder and reaction created before this check
            let _ = channel_id
                .delete_message(&ctx.http, placeholder_msg_id)
                .await;
            super::super::formatting::remove_reaction_raw(&ctx.http, channel_id, user_msg_id, '⏳')
                .await;
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] 🔀 RACE: message queued (another turn won), channel {}",
                channel_id
            );
            return Ok(());
        }
        shared
            .global_active
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        data.cancel_tokens.insert(channel_id, cancel_token.clone());
        data.active_request_owner.insert(channel_id, request_owner);
    }
    shared
        .turn_start_times
        .insert(channel_id, std::time::Instant::now());

    // Spawn turn watchdog — cancels the turn if it exceeds the deadline.
    // The deadline is stored in cancel_token.watchdog_deadline_ms and can be
    // extended via POST /api/turns/{channel_id}/extend-timeout (up to 3h cap).
    {
        let watchdog_token = cancel_token.clone();
        let watchdog_shared = shared.clone();
        let watchdog_http = ctx.http.clone();
        let timeout = super::super::turn_watchdog_timeout();

        // Set initial deadline and max ceiling (initial + 3h)
        let now_ms = chrono::Utc::now().timestamp_millis();
        let deadline_ms = now_ms + timeout.as_millis() as i64;
        let max_deadline_ms = now_ms + 3 * 3600 * 1000; // 3 hours absolute cap
        watchdog_token
            .watchdog_deadline_ms
            .store(deadline_ms, std::sync::atomic::Ordering::Relaxed);
        watchdog_token
            .watchdog_max_deadline_ms
            .store(max_deadline_ms, std::sync::atomic::Ordering::Relaxed);

        let watchdog_channel_id_num = channel_id.get();
        let watchdog_provider = provider.clone();
        tokio::spawn(async move {
            const CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

            loop {
                tokio::time::sleep(CHECK_INTERVAL).await;

                // Exit early if the turn already completed/cancelled
                if watchdog_token
                    .cancelled
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    super::super::clear_watchdog_deadline_override(watchdog_channel_id_num);
                    return;
                }

                // Check for API-based deadline extension
                if let Some(new_deadline) =
                    super::super::take_watchdog_deadline_override(watchdog_channel_id_num)
                {
                    let max_dl = watchdog_token
                        .watchdog_max_deadline_ms
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let clamped = std::cmp::min(new_deadline, max_dl);
                    watchdog_token
                        .watchdog_deadline_ms
                        .store(clamped, std::sync::atomic::Ordering::Relaxed);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    let remaining_min =
                        (clamped - chrono::Utc::now().timestamp_millis()) / 1000 / 60;
                    println!(
                        "  [{ts}] ⏰ WATCHDOG: deadline extended for channel {} — {remaining_min}m remaining",
                        channel_id
                    );
                }

                // Auto-extend based on inflight updated_at: if inflight was updated recently
                // (within last 5 min), push deadline forward by the default timeout
                {
                    let current_dl = watchdog_token
                        .watchdog_deadline_ms
                        .load(std::sync::atomic::Ordering::Relaxed);
                    let now_ms_check = chrono::Utc::now().timestamp_millis();
                    // Only auto-extend when close to deadline (within 2 minutes)
                    if now_ms_check > current_dl - 120_000 {
                        if let Some(inflight) = super::super::inflight::load_inflight_state(
                            &watchdog_provider,
                            watchdog_channel_id_num,
                        ) {
                            if let Ok(updated) = chrono::NaiveDateTime::parse_from_str(
                                &inflight.updated_at,
                                "%Y-%m-%d %H:%M:%S",
                            ) {
                                let updated_ms = updated.and_utc().timestamp_millis();
                                let age_ms = now_ms_check - updated_ms;
                                // If inflight was updated within the last 5 minutes, auto-extend
                                if age_ms < 300_000 {
                                    let max_dl = watchdog_token
                                        .watchdog_max_deadline_ms
                                        .load(std::sync::atomic::Ordering::Relaxed);
                                    let new_dl = std::cmp::min(
                                        now_ms_check + timeout.as_millis() as i64,
                                        max_dl,
                                    );
                                    if new_dl > current_dl {
                                        watchdog_token
                                            .watchdog_deadline_ms
                                            .store(new_dl, std::sync::atomic::Ordering::Relaxed);
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        let remaining_min = (new_dl - now_ms_check) / 1000 / 60;
                                        println!(
                                            "  [{ts}] ⏰ WATCHDOG: auto-extended for channel {} (inflight active) — {remaining_min}m remaining",
                                            channel_id
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                let current_deadline = watchdog_token
                    .watchdog_deadline_ms
                    .load(std::sync::atomic::Ordering::Relaxed);
                let now = chrono::Utc::now().timestamp_millis();

                if now < current_deadline {
                    continue; // Not yet — deadline may have been extended
                }

                // Deadline reached — fire watchdog
                // Verify this watchdog's token is still the CURRENT active token for this channel.
                let is_current_token = {
                    let data = watchdog_shared.core.lock().await;
                    data.cancel_tokens
                        .get(&channel_id)
                        .map_or(false, |current| {
                            std::sync::Arc::ptr_eq(&watchdog_token, current)
                        })
                };
                if is_current_token {
                    let elapsed_mins =
                        (now - (current_deadline - timeout.as_millis() as i64)) / 1000 / 60;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ⏰ WATCHDOG: turn timeout (~{elapsed_mins}m) for channel {}, cancelling",
                        channel_id
                    );
                    super::super::turn_bridge::cancel_active_token(
                        &watchdog_token,
                        true,
                        "watchdog timeout",
                    );

                    // Notify Discord
                    let has_queued = {
                        let mut data = watchdog_shared.core.lock().await;
                        data.intervention_queue
                            .get_mut(&channel_id)
                            .map_or(false, |q| super::super::has_soft_intervention(q))
                    };
                    let msg = if has_queued {
                        format!(
                            "⚠️ 턴이 {elapsed_mins}분 타임아웃으로 자동 중단되었습니다. 대기 중인 메시지로 다음 턴을 시작합니다.",
                        )
                    } else {
                        format!("⚠️ 턴이 {elapsed_mins}분 타임아웃으로 자동 중단되었습니다.",)
                    };
                    let _ = channel_id.say(&watchdog_http, msg).await;
                }
                return; // Watchdog done regardless
            }
        });
    }

    // Resolve remote profile for this channel
    let remote_profile = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.remote_profile_name.as_ref())
            .and_then(|name| {
                let settings = crate::config::Settings::load();
                settings
                    .remote_profiles
                    .iter()
                    .find(|p| p.name == *name)
                    .cloned()
            })
    };

    let adk_session_name = channel_name.clone();
    let adk_session_info = derive_adk_session_info(
        Some(user_text),
        channel_name.as_deref(),
        Some(&current_path),
    );
    // #222: DB-based dispatch lookup takes priority over text parsing.
    // In unified threads, user_text may contain a stale DISPATCH: prefix
    // from a previous dispatch in the same thread. DB lookup uses the
    // thread→card→dispatch link which is always current.
    let dispatch_id = super::super::adk_session::lookup_pending_dispatch_for_thread(
        shared.api_port,
        channel_id.get(),
    )
    .await
    .or_else(|| super::super::adk_session::parse_dispatch_id(user_text));
    post_adk_session_status(
        adk_session_key.as_deref(),
        adk_session_name.as_deref(),
        Some(provider.as_str()),
        "working",
        &provider,
        Some(&adk_session_info),
        None,
        Some(&current_path),
        dispatch_id.as_deref(),
        shared.api_port,
    )
    .await;

    let (inflight_tmux_name, inflight_output_path, inflight_input_fifo, inflight_offset) = {
        #[cfg(unix)]
        {
            if remote_profile.is_none()
                && provider.uses_managed_tmux_backend()
                && claude::is_tmux_available()
            {
                if let Some(ref tmux_name) = tmux_session_name {
                    let (output_path, input_fifo_path) = tmux_runtime_paths(tmux_name);
                    let session_exists =
                        crate::services::tmux_diagnostics::tmux_session_has_live_pane(tmux_name);
                    let last_offset = std::fs::metadata(&output_path)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    (
                        Some(tmux_name.clone()),
                        Some(output_path),
                        Some(input_fifo_path),
                        if session_exists { last_offset } else { 0 },
                    )
                } else {
                    (None, None, None, 0)
                }
            } else {
                (None, None, None, 0)
            }
        }
        #[cfg(not(unix))]
        {
            (None, None, None, 0u64)
        }
    };

    let mut inflight_state = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        channel_name.clone(),
        request_owner.get(),
        user_msg_id.get(),
        placeholder_msg_id.get(),
        user_text.to_string(),
        session_id.clone(),
        inflight_tmux_name,
        inflight_output_path,
        inflight_input_fifo.clone(),
        inflight_offset,
    );
    // Persist identifiers for long-turn diagnostics (#130)
    inflight_state.session_key = adk_session_key.clone();
    inflight_state.dispatch_id = dispatch_id.clone();
    if let Err(e) = save_inflight_state(&inflight_state) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        println!("  [{ts}]   ⚠ inflight state save failed: {e}");
    }

    // Create channel for streaming
    let (tx, rx) = mpsc::channel();
    let (completion_tx, completion_rx) = if wait_for_completion {
        let (tx, rx) = tokio::sync::oneshot::channel();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let session_id_clone = session_id.clone();
    let current_path_clone = current_path.clone();
    let cancel_token_clone = cancel_token.clone();

    // Pause tmux watcher if one exists (so it doesn't read our turn's output)
    if let Some(watcher) = shared.tmux_watchers.get(&channel_id) {
        watcher
            .pause_epoch
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        watcher
            .paused
            .store(true, std::sync::atomic::Ordering::Relaxed);
    }

    // Auto-sync worktree before sending message to session
    {
        let script = super::super::runtime_store::agentdesk_root()
            .unwrap_or_default()
            .join("scripts/worktree-autosync.sh");
        if script.exists() {
            let ws = current_path.clone();
            let ts = chrono::Local::now().format("%H:%M:%S");
            match std::process::Command::new(&script)
                .arg(&ws)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .output()
            {
                Ok(out) => {
                    let stdout = String::from_utf8_lossy(&out.stdout);
                    let msg = stdout.trim();
                    match out.status.code() {
                        Some(0) => println!("  [{ts}] 🔄 worktree-autosync [{ws}]: {msg}"),
                        Some(1) => println!("  [{ts}] ⏭ worktree-autosync [{ws}]: skipped — {msg}"),
                        _ => eprintln!("  [{ts}] ⚠ worktree-autosync [{ws}]: error — {msg}"),
                    }
                }
                Err(e) => eprintln!("  [{ts}] ⚠ worktree-autosync: failed to run — {e}"),
            }
        }
    }

    let model_for_turn =
        super::super::commands::resolve_model_for_turn(shared, channel_id, &provider).await;

    // Fetch context compact percent from ADK settings (provider-specific)
    let ctx_thresholds = super::super::adk_session::fetch_context_thresholds(shared.api_port).await;
    let compact_percent = ctx_thresholds.compact_pct_for(&provider);
    // Use model-specific context window (reads Codex models cache), falling
    // back to the provider default if the model isn't found.
    let model_context_window = provider.resolve_context_window(model_for_turn.as_deref());

    // Pre-compute provider-specific compact config
    let compact_percent_for_claude = Some(ctx_thresholds.compact_pct_for(&provider));
    let compact_token_limit_for_codex = {
        let cli_config = provider.compact_cli_config(compact_percent, model_context_window);
        cli_config
            .first()
            .map(|(_, v)| v.parse::<u64>().unwrap_or(0))
    };

    // Run the provider in a blocking thread
    let provider_for_blocking = provider.clone();
    tokio::task::spawn_blocking(move || {
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(
                || match &provider_for_blocking {
                    ProviderKind::Claude => claude::execute_command_streaming(
                        &context_prompt,
                        session_id_clone.as_deref(),
                        &current_path_clone,
                        tx.clone(),
                        Some(&system_prompt_owned),
                        Some(&allowed_tools),
                        Some(cancel_token_clone),
                        remote_profile.as_ref(),
                        tmux_session_name.as_deref(),
                        Some(channel_id.get()),
                        Some(provider_for_blocking.clone()),
                        model_for_turn.as_deref(),
                        compact_percent_for_claude,
                    ),
                    ProviderKind::Codex => codex::execute_command_streaming(
                        &context_prompt,
                        session_id_clone.as_deref(),
                        &current_path_clone,
                        tx.clone(),
                        Some(&system_prompt_owned),
                        Some(&allowed_tools),
                        Some(cancel_token_clone),
                        remote_profile.as_ref(),
                        tmux_session_name.as_deref(),
                        Some(channel_id.get()),
                        Some(provider_for_blocking.clone()),
                        model_for_turn.as_deref(),
                        compact_token_limit_for_codex,
                    ),
                    ProviderKind::Gemini => gemini::execute_command_streaming(
                        &context_prompt,
                        session_id_clone.as_deref(),
                        &current_path_clone,
                        tx.clone(),
                        Some(&system_prompt_owned),
                        Some(&allowed_tools),
                        Some(cancel_token_clone),
                        remote_profile.as_ref(),
                        tmux_session_name.as_deref(),
                        Some(channel_id.get()),
                        Some(provider_for_blocking.clone()),
                        model_for_turn.as_deref(),
                        None, // Gemini: compact not supported
                    ),
                    ProviderKind::Qwen => qwen::execute_command_streaming(
                        &context_prompt,
                        session_id_clone.as_deref(),
                        &current_path_clone,
                        tx.clone(),
                        Some(&system_prompt_owned),
                        Some(&allowed_tools),
                        Some(cancel_token_clone),
                        remote_profile.as_ref(),
                        tmux_session_name.as_deref(),
                        Some(channel_id.get()),
                        Some(provider_for_blocking.clone()),
                        model_for_turn.as_deref(),
                        None, // Qwen: compact not supported
                    ),
                    ProviderKind::Unsupported(name) => {
                        let _ = tx.send(StreamMessage::Error {
                            message: format!("Provider '{}' is not installed", name),
                            stdout: String::new(),
                            stderr: String::new(),
                            exit_code: None,
                        });
                        Ok(())
                    }
                },
            ));

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                eprintln!("  [streaming] Error: {}", e);
                let _ = tx.send(StreamMessage::Error {
                    message: e,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
            Err(panic_info) => {
                let msg = if let Some(s) = panic_info.downcast_ref::<String>() {
                    s.clone()
                } else if let Some(s) = panic_info.downcast_ref::<&str>() {
                    s.to_string()
                } else {
                    "unknown panic".to_string()
                };
                eprintln!("  [streaming] PANIC: {}", msg);
                let _ = tx.send(StreamMessage::Error {
                    message: format!("Internal error (panic): {}", msg),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
        }
    });

    spawn_turn_bridge(
        ctx.http.clone(),
        shared.clone(),
        cancel_token.clone(),
        rx,
        TurnBridgeContext {
            provider,
            channel_id,
            user_msg_id,
            user_text_owned: user_text.to_string(),
            request_owner_name: request_owner_name.to_string(),
            request_owner: Some(request_owner),
            serenity_ctx: Some(ctx.clone()),
            token: Some(token.to_string()),
            role_binding: role_binding.clone(),
            adk_session_key,
            adk_session_name,
            adk_session_info: Some(adk_session_info),
            adk_cwd: Some(current_path.clone()),
            dispatch_id,
            current_msg_id: placeholder_msg_id,
            response_sent_offset: 0,
            full_response: String::new(),
            tmux_last_offset: Some(inflight_offset),
            new_session_id: session_id.clone(),
            defer_watcher_resume,
            completion_tx,
            inflight_state,
        },
    );

    if let Some(rx) = completion_rx {
        rx.await
            .map_err(|_| "queued turn completion wait failed".to_string())?;
    }

    Ok(())
}

/// Handle file uploads from Discord messages
pub(super) async fn handle_file_upload(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let channel_id = msg.channel_id;

    // Always use the runtime uploads directory (works without session)
    let Some(save_dir) = channel_upload_dir(channel_id) else {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(&ctx.http, "Cannot resolve upload directory.")
            .await;
        return Ok(());
    };

    if let Err(e) = fs::create_dir_all(&save_dir) {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(
                &ctx.http,
                format!("Failed to prepare upload directory: {}", e),
            )
            .await;
        return Ok(());
    }

    for attachment in &msg.attachments {
        let file_name = &attachment.filename;

        // Download file from Discord CDN
        let buf = match reqwest::get(&attachment.url).await {
            Ok(resp) => match resp.bytes().await {
                Ok(bytes) => bytes,
                Err(e) => {
                    rate_limit_wait(shared, channel_id).await;
                    let _ = channel_id
                        .say(&ctx.http, format!("Download failed: {}", e))
                        .await;
                    continue;
                }
            },
            Err(e) => {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(&ctx.http, format!("Download failed: {}", e))
                    .await;
                continue;
            }
        };

        // Save to session path (sanitize filename)
        let safe_name = Path::new(file_name)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("uploaded_file"));
        let ts = chrono::Utc::now().timestamp_millis();
        let stamped_name = format!("{}_{}", ts, safe_name.to_string_lossy());
        let dest = save_dir.join(stamped_name);
        let file_size = buf.len();

        match fs::write(&dest, &buf) {
            Ok(_) => {
                let msg_text = format!("Saved: {}\n({} bytes)", dest.display(), file_size);
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id.say(&ctx.http, &msg_text).await;
            }
            Err(e) => {
                rate_limit_wait(shared, channel_id).await;
                let _ = channel_id
                    .say(&ctx.http, format!("Failed to save file: {}", e))
                    .await;
                continue;
            }
        }

        // Record upload in session
        let upload_record = format!(
            "[File uploaded] {} → {} ({} bytes)",
            file_name,
            dest.display(),
            file_size
        );
        {
            let mut data = shared.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                session.history.push(HistoryItem {
                    item_type: HistoryType::User,
                    content: upload_record.clone(),
                });
                session.pending_uploads.push(upload_record);
            }
        }
    }

    Ok(())
}

/// Handle shell commands from raw text messages (! prefix)
pub(super) async fn handle_shell_command_raw(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    text: &str,
    shared: &Arc<SharedData>,
) -> Result<(), Error> {
    let cmd_str = text.strip_prefix('!').unwrap_or("").trim();
    if cmd_str.is_empty() {
        rate_limit_wait(shared, channel_id).await;
        let _ = channel_id
            .say(&ctx.http, "Usage: `!<command>`\nExample: `!ls -la`")
            .await;
        return Ok(());
    }

    let working_dir = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|s| s.current_path.clone())
            .unwrap_or_else(|| {
                dirs::home_dir()
                    .map(|h| h.display().to_string())
                    .unwrap_or_else(|| "/".to_string())
            })
    };

    let cmd_owned = cmd_str.to_string();
    let working_dir_clone = working_dir.clone();

    let result = tokio::task::spawn_blocking(move || {
        let child = crate::services::platform::shell::shell_command_builder(&cmd_owned)
            .current_dir(&working_dir_clone)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn();
        match child {
            Ok(child) => child.wait_with_output(),
            Err(e) => Err(e),
        }
    })
    .await;

    let response = match result {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let exit_code = output.status.code().unwrap_or(-1);
            let mut parts = Vec::new();
            if !stdout.is_empty() {
                parts.push(format!("```\n{}\n```", stdout.trim_end()));
            }
            if !stderr.is_empty() {
                parts.push(format!("stderr:\n```\n{}\n```", stderr.trim_end()));
            }
            if parts.is_empty() {
                parts.push(format!("(exit code: {})", exit_code));
            } else if exit_code != 0 {
                parts.push(format!("(exit code: {})", exit_code));
            }
            parts.join("\n")
        }
        Ok(Err(e)) => format!("Failed to execute: {}", e),
        Err(e) => format!("Task error: {}", e),
    };

    send_long_message_raw(&ctx.http, channel_id, &response, shared).await?;
    Ok(())
}

/// Handle text-based commands (!start, !meeting, !stop, !clear, etc.).
/// Returns true if the command was handled, false otherwise.
pub(super) async fn handle_text_command(
    ctx: &serenity::Context,
    msg: &serenity::Message,
    data: &Data,
    channel_id: serenity::ChannelId,
    text: &str,
) -> Result<bool, Error> {
    let parts: Vec<&str> = text.splitn(3, char::is_whitespace).collect();
    let cmd = parts[0];
    let arg1 = parts.get(1).unwrap_or(&"");
    let arg2 = parts.get(2).unwrap_or(&"");

    match cmd {
        "!start" => {
            let path_str = if arg1.is_empty() { "." } else { arg1 };

            // Resolve path
            let effective_path = if path_str == "." || path_str.is_empty() {
                // Use workspace root or current directory
                let Some(workspace_dir) = runtime_store::workspace_root() else {
                    let _ = msg
                        .reply(&ctx.http, "Error: cannot determine workspace root.")
                        .await;
                    return Ok(true);
                };
                // Create a random workspace for this channel
                use rand::Rng;
                let random_name: String = rand::thread_rng()
                    .sample_iter(&rand::distributions::Alphanumeric)
                    .take(8)
                    .map(char::from)
                    .collect();
                let ch_name = resolve_channel_category(ctx, channel_id)
                    .await
                    .0
                    .unwrap_or_else(|| format!("ch-{}", channel_id));
                let dir = workspace_dir.join(format!("{}-{}", ch_name, random_name));
                std::fs::create_dir_all(&dir).ok();
                dir.to_string_lossy().to_string()
            } else if path_str.starts_with('~') {
                dirs::home_dir()
                    .map(|h| path_str.replacen('~', &h.to_string_lossy(), 1))
                    .unwrap_or_else(|| path_str.to_string())
            } else {
                path_str.to_string()
            };

            // Validate path exists
            if !std::path::Path::new(&effective_path).exists() {
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!("Error: path `{}` does not exist.", effective_path),
                    )
                    .await;
                return Ok(true);
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ◀ [{}] !start path={}",
                msg.author.name, effective_path
            );

            // Create session
            let (ch_name, cat_name) = resolve_channel_category(ctx, channel_id).await;
            {
                let mut d = data.shared.core.lock().await;
                let session = d
                    .sessions
                    .entry(channel_id)
                    .or_insert_with(|| DiscordSession {
                        session_id: None,
                        current_path: None,
                        history: Vec::new(),
                        pending_uploads: Vec::new(),
                        cleared: false,
                        channel_name: None,
                        category_name: None,
                        remote_profile_name: None,
                        channel_id: Some(channel_id.get()),
                        last_active: tokio::time::Instant::now(),
                        worktree: None,

                        born_generation: runtime_store::load_generation(),
                    });
                session.current_path = Some(effective_path.clone());
                session.channel_name = ch_name;
                session.category_name = cat_name;
                session.last_active = tokio::time::Instant::now();
            }

            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ▶ Session started: {}", effective_path);
            let _ = msg
                .reply(
                    &ctx.http,
                    format!("Session started at `{}`.", effective_path),
                )
                .await;
            return Ok(true);
        }

        "!meeting" => {
            let action = if arg1.is_empty() { "start" } else { arg1 };
            let agenda = if arg2.is_empty() { arg1 } else { arg2 };

            match action {
                "start" => {
                    let agenda_text = if agenda.is_empty() || *agenda == "start" {
                        let _ = msg
                            .reply(
                                &ctx.http,
                                "사용법: `!meeting start <안건>` 또는 `!meeting <안건>`",
                            )
                            .await;
                        return Ok(true);
                    } else {
                        agenda
                    };

                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!(
                        "  [{ts}] ◀ [{}] !meeting start {}",
                        msg.author.name, agenda_text
                    );

                    let http = ctx.http.clone();
                    let shared = data.shared.clone();
                    let provider = data.provider.clone();
                    let agenda_owned = agenda_text.to_string();

                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!(
                                "📋 회의를 시작할게. 진행 모델: {} / 교차검증: {}",
                                provider.display_name(),
                                provider.counterpart().display_name()
                            ),
                        )
                        .await;

                    tokio::spawn(async move {
                        match meeting::start_meeting(
                            &*http,
                            channel_id,
                            &agenda_owned,
                            provider,
                            &shared,
                        )
                        .await
                        {
                            Ok(Some(id)) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                println!("  [{ts}] ✅ Meeting completed: {id}");
                            }
                            Ok(None) => {}
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                println!("  [{ts}] ❌ Meeting error: {e}");
                            }
                        }
                    });
                    return Ok(true);
                }
                "stop" => {
                    let _ = meeting::cancel_meeting(&ctx.http, channel_id, &data.shared).await;
                    return Ok(true);
                }
                "status" => {
                    let _ = meeting::meeting_status(&ctx.http, channel_id, &data.shared).await;
                    return Ok(true);
                }
                _ => {
                    // Treat unknown action as agenda text
                    let full_agenda = text.trim_start_matches("!meeting").trim();
                    if full_agenda.is_empty() {
                        let _ = msg.reply(&ctx.http, "사용법: `!meeting <안건>`").await;
                        return Ok(true);
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    println!("  [{ts}] ◀ [{}] !meeting {}", msg.author.name, full_agenda);

                    let http = ctx.http.clone();
                    let shared = data.shared.clone();
                    let provider = data.provider.clone();
                    let agenda_owned = full_agenda.to_string();

                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!(
                                "📋 회의를 시작할게. 진행 모델: {} / 교차검증: {}",
                                provider.display_name(),
                                provider.counterpart().display_name()
                            ),
                        )
                        .await;

                    tokio::spawn(async move {
                        match meeting::start_meeting(
                            &*http,
                            channel_id,
                            &agenda_owned,
                            provider,
                            &shared,
                        )
                        .await
                        {
                            Ok(Some(id)) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                println!("  [{ts}] ✅ Meeting completed: {id}");
                            }
                            Ok(None) => {}
                            Err(e) => {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                println!("  [{ts}] ❌ Meeting error: {e}");
                            }
                        }
                    });
                    return Ok(true);
                }
            }
        }

        "!stop" => {
            let mut d = data.shared.core.lock().await;
            if let Some(token) = d.cancel_tokens.remove(&channel_id) {
                token.cancel_with_tmux_cleanup();
                drop(d);
                let _ = msg.reply(&ctx.http, "Turn cancelled.").await;
            } else {
                drop(d);
                let _ = msg.reply(&ctx.http, "No active turn to stop.").await;
            }
            return Ok(true);
        }

        "!clear" => {
            super::super::commands::clear_channel_session_state(
                &ctx.http,
                &data.shared,
                &data.provider,
                channel_id,
                "!clear",
            )
            .await;
            let _ = msg.reply(&ctx.http, "Session cleared.").await;
            return Ok(true);
        }

        // ── Simple diagnostic / info commands ──
        "!pwd" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !pwd", msg.author.name);

            auto_restore_session(&data.shared, channel_id, ctx).await;

            let (current_path, remote_name) = {
                let d = data.shared.core.lock().await;
                let session = d.sessions.get(&channel_id);
                (
                    session.and_then(|s| s.current_path.clone()),
                    session.and_then(|s| s.remote_profile_name.clone()),
                )
            };
            let reply = match current_path {
                Some(path) => {
                    let remote_info = remote_name
                        .map(|n| format!(" (remote: **{}**)", n))
                        .unwrap_or_else(|| " (local)".to_string());
                    format!("`{}`{}", path, remote_info)
                }
                None => "No active session. Use `!start <path>` first.".to_string(),
            };
            let _ = msg.reply(&ctx.http, &reply).await;
            return Ok(true);
        }

        "!health" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !health", msg.author.name);

            let text =
                commands::build_health_report(&data.shared, &data.provider, channel_id).await;
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!status" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !status", msg.author.name);

            let text =
                commands::build_status_report(&data.shared, &data.provider, channel_id).await;
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!inflight" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !inflight", msg.author.name);

            let text =
                commands::build_inflight_report(&data.shared, &data.provider, channel_id).await;
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!queue" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !queue", msg.author.name);

            let show_all = *arg1 == "all";
            let text =
                commands::build_queue_report(&data.shared, &data.provider, channel_id, show_all)
                    .await;
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!metrics" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !metrics", msg.author.name);

            let metrics_data = if arg1.is_empty() {
                metrics::load_today()
            } else {
                metrics::load_date(arg1)
            };
            let label = if arg1.is_empty() { "today" } else { arg1 };
            let text = metrics::build_metrics_report(&metrics_data, label);
            send_long_message_raw(&ctx.http, channel_id, &text, &data.shared).await?;
            return Ok(true);
        }

        "!debug" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !debug", msg.author.name);

            let new_state = claude::toggle_debug();
            let status = if new_state { "ON" } else { "OFF" };
            let _ = msg
                .reply(&ctx.http, format!("Debug logging: **{}**", status))
                .await;
            println!("  [{ts}] ▶ Debug logging toggled to {status}");
            return Ok(true);
        }

        "!help" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !help", msg.author.name);

            let provider_name = data.provider.display_name();
            let help = format!(
                "\
**AgentDesk Discord Bot**
Manage server files & chat with {p}.
Each channel gets its own independent {p} session.

**Session**
`!start <path>` — Start session at directory
`!pwd` — Show current working directory
`!health` — Show runtime health summary
`!status` — Show this channel session status
`!inflight` — Show saved inflight turn state
`!clear` — Clear AI conversation history
`!stop` — Stop current AI request

**File Transfer**
`!down <file>` — Download file from server
Send a file/photo — Upload to session directory

**Shell**
`!shell <command>` — Run shell command directly

**AI Chat**
Any other message is sent to {p}.

**Tool Management**
`!allowedtools` — Show currently allowed tools
`!allowed +name` — Add tool (e.g. `!allowed +Bash`)
`!allowed -name` — Remove tool

**Skills**
`!cc <skill>` — Run a provider skill

**Settings**
`/model` — Open the interactive model picker
`!debug` — Toggle debug logging
`!metrics [date]` — Show turn metrics
`!queue [all]` — Show pending queue

**User Management** (owner only)
`!allowall on|off|status` — Allow everyone or restrict to authorized users
`!adduser <user_id>` — Allow a user to use the bot
`!removeuser <user_id>` — Remove a user's access
`!help` — Show this help",
                p = provider_name
            );
            send_long_message_raw(&ctx.http, channel_id, &help, &data.shared).await?;
            return Ok(true);
        }

        "!allowedtools" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !allowedtools", msg.author.name);

            let tools = {
                let settings = data.shared.settings.read().await;
                settings.allowed_tools.clone()
            };

            let mut reply = String::from("**Allowed Tools**\n\n");
            for tool in &tools {
                let (desc, destructive) = super::super::formatting::tool_info(tool);
                let badge = super::super::formatting::risk_badge(destructive);
                if badge.is_empty() {
                    reply.push_str(&format!("`{}` — {}\n", tool, desc));
                } else {
                    reply.push_str(&format!("`{}` {} — {}\n", tool, badge, desc));
                }
            }
            reply.push_str(&format!(
                "\n{} = destructive\nTotal: {}",
                super::super::formatting::risk_badge(true),
                tools.len()
            ));
            send_long_message_raw(&ctx.http, channel_id, &reply, &data.shared).await?;
            return Ok(true);
        }

        // ── Commands with arguments ──
        "!model" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !model {} {}", msg.author.name, arg1, arg2);
            let _ = msg
                .reply(
                    &ctx.http,
                    "Model picker text commands are deprecated. Use `/model`.",
                )
                .await;
            return Ok(true);
        }

        "!allowed" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !allowed {}", msg.author.name, arg1);

            let arg = arg1.trim();
            let (op, raw_name) = if let Some(name) = arg.strip_prefix('+') {
                ('+', name.trim())
            } else if let Some(name) = arg.strip_prefix('-') {
                ('-', name.trim())
            } else {
                let _ = msg.reply(&ctx.http, "Use `+toolname` to add or `-toolname` to remove.\nExample: `!allowed +Bash`").await;
                return Ok(true);
            };

            if raw_name.is_empty() {
                let _ = msg.reply(&ctx.http, "Tool name cannot be empty.").await;
                return Ok(true);
            }

            let Some(tool_name) =
                super::super::formatting::canonical_tool_name(raw_name).map(str::to_string)
            else {
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!(
                            "Unknown tool `{}`. Use `!allowedtools` to see valid tool names.",
                            raw_name
                        ),
                    )
                    .await;
                return Ok(true);
            };

            let response_msg = {
                let mut settings = data.shared.settings.write().await;
                match op {
                    '+' => {
                        if settings.allowed_tools.iter().any(|t| t == &tool_name) {
                            format!("`{}` is already in the list.", tool_name)
                        } else {
                            settings.allowed_tools.push(tool_name.clone());
                            save_bot_settings(&data.token, &settings);
                            format!("Added `{}`", tool_name)
                        }
                    }
                    '-' => {
                        let before_len = settings.allowed_tools.len();
                        settings.allowed_tools.retain(|t| t != &tool_name);
                        if settings.allowed_tools.len() < before_len {
                            save_bot_settings(&data.token, &settings);
                            format!("Removed `{}`", tool_name)
                        } else {
                            format!("`{}` is not in the list.", tool_name)
                        }
                    }
                    _ => unreachable!(),
                }
            };
            let _ = msg.reply(&ctx.http, &response_msg).await;
            return Ok(true);
        }

        "!adduser" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !adduser {}", msg.author.name, arg1);

            if !check_owner(msg.author.id, &data.shared).await {
                let _ = msg.reply(&ctx.http, "Only the owner can add users.").await;
                return Ok(true);
            }

            let raw_id = arg1
                .trim()
                .trim_start_matches("<@")
                .trim_end_matches('>')
                .trim_start_matches('!');
            let target_id: u64 = match raw_id.parse() {
                Ok(id) => id,
                Err(_) => {
                    let _ = msg
                        .reply(&ctx.http, "Usage: `!adduser <user_id>` or `!adduser @user`")
                        .await;
                    return Ok(true);
                }
            };

            {
                let mut settings = data.shared.settings.write().await;
                if settings.allowed_user_ids.contains(&target_id) {
                    let _ = msg
                        .reply(&ctx.http, format!("`{}` is already authorized.", target_id))
                        .await;
                    return Ok(true);
                }
                settings.allowed_user_ids.push(target_id);
                save_bot_settings(&data.token, &settings);
            }

            let _ = msg
                .reply(
                    &ctx.http,
                    format!("Added `{}` as authorized user.", target_id),
                )
                .await;
            println!("  [{ts}] ▶ Added user: {target_id}");
            return Ok(true);
        }

        "!allowall" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !allowall {}", msg.author.name, arg1);

            if !check_owner(msg.author.id, &data.shared).await {
                let _ = msg
                    .reply(&ctx.http, "Only the owner can change public access.")
                    .await;
                return Ok(true);
            }

            let action = arg1.trim().to_ascii_lowercase();
            if action.is_empty() || action == "status" {
                let enabled = {
                    let settings = data.shared.settings.read().await;
                    settings.allow_all_users
                };
                let message = if enabled {
                    "Public access is enabled. Any Discord user can talk to this bot in allowed channels."
                } else {
                    "Public access is disabled. Only the owner and authorized users can talk to this bot."
                };
                let _ = msg.reply(&ctx.http, message).await;
                return Ok(true);
            }

            let enabled = match action.as_str() {
                "on" | "true" | "enable" | "enabled" => true,
                "off" | "false" | "disable" | "disabled" => false,
                _ => {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            "Usage: `!allowall on`, `!allowall off`, or `!allowall status`",
                        )
                        .await;
                    return Ok(true);
                }
            };

            let response = {
                let mut settings = data.shared.settings.write().await;
                settings.allow_all_users = enabled;
                save_bot_settings(&data.token, &settings);
                if enabled {
                    "Public access enabled. Any Discord user can talk to this bot in allowed channels."
                } else {
                    "Public access disabled. Only the owner and authorized users can talk to this bot."
                }
            };

            let _ = msg.reply(&ctx.http, response).await;
            println!("  [{ts}] ▶ {response}");
            return Ok(true);
        }

        "!removeuser" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!("  [{ts}] ◀ [{}] !removeuser {}", msg.author.name, arg1);

            if !check_owner(msg.author.id, &data.shared).await {
                let _ = msg
                    .reply(&ctx.http, "Only the owner can remove users.")
                    .await;
                return Ok(true);
            }

            let raw_id = arg1
                .trim()
                .trim_start_matches("<@")
                .trim_end_matches('>')
                .trim_start_matches('!');
            let target_id: u64 = match raw_id.parse() {
                Ok(id) => id,
                Err(_) => {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            "Usage: `!removeuser <user_id>` or `!removeuser @user`",
                        )
                        .await;
                    return Ok(true);
                }
            };

            {
                let mut settings = data.shared.settings.write().await;
                let before_len = settings.allowed_user_ids.len();
                settings.allowed_user_ids.retain(|&id| id != target_id);
                if settings.allowed_user_ids.len() == before_len {
                    let _ = msg
                        .reply(
                            &ctx.http,
                            format!("`{}` is not in the authorized list.", target_id),
                        )
                        .await;
                    return Ok(true);
                }
                save_bot_settings(&data.token, &settings);
            }

            let _ = msg
                .reply(
                    &ctx.http,
                    format!("Removed `{}` from authorized users.", target_id),
                )
                .await;
            println!("  [{ts}] ▶ Removed user: {target_id}");
            return Ok(true);
        }

        "!down" => {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let file_arg = text.strip_prefix("!down").unwrap_or("").trim();
            println!("  [{ts}] ◀ [{}] !down {}", msg.author.name, file_arg);

            if file_arg.is_empty() {
                let _ = msg
                    .reply(
                        &ctx.http,
                        "Usage: `!down <filepath>`\nExample: `!down /home/user/file.txt`",
                    )
                    .await;
                return Ok(true);
            }

            // Resolve relative path
            let resolved_path = if std::path::Path::new(file_arg).is_absolute() {
                file_arg.to_string()
            } else {
                let current_path = {
                    let d = data.shared.core.lock().await;
                    d.sessions
                        .get(&channel_id)
                        .and_then(|s| s.current_path.clone())
                };
                match current_path {
                    Some(base) => format!("{}/{}", base.trim_end_matches('/'), file_arg),
                    None => {
                        let _ = msg
                            .reply(
                                &ctx.http,
                                "No active session. Use absolute path or `!start <path>` first.",
                            )
                            .await;
                        return Ok(true);
                    }
                }
            };

            let path = std::path::Path::new(&resolved_path);
            if !path.exists() {
                let _ = msg
                    .reply(&ctx.http, format!("File not found: {}", resolved_path))
                    .await;
                return Ok(true);
            }
            if !path.is_file() {
                let _ = msg
                    .reply(&ctx.http, format!("Not a file: {}", resolved_path))
                    .await;
                return Ok(true);
            }

            let attachment = CreateAttachment::path(path).await?;
            rate_limit_wait(&data.shared, channel_id).await;
            let _ = channel_id
                .send_message(&ctx.http, CreateMessage::new().add_file(attachment))
                .await;
            return Ok(true);
        }

        "!shell" => {
            let cmd_str = text.strip_prefix("!shell").unwrap_or("").trim();
            let ts = chrono::Local::now().format("%H:%M:%S");
            let preview = truncate_str(cmd_str, 60);
            println!("  [{ts}] ◀ [{}] !shell {}", msg.author.name, preview);

            if cmd_str.is_empty() {
                let _ = msg
                    .reply(
                        &ctx.http,
                        "Usage: `!shell <command>`\nExample: `!shell ls -la`",
                    )
                    .await;
                return Ok(true);
            }

            let working_dir = {
                let d = data.shared.core.lock().await;
                d.sessions
                    .get(&channel_id)
                    .and_then(|s| s.current_path.clone())
                    .unwrap_or_else(|| {
                        dirs::home_dir()
                            .map(|h| h.display().to_string())
                            .unwrap_or_else(|| "/".to_string())
                    })
            };

            let cmd_owned = cmd_str.to_string();
            let working_dir_clone = working_dir.clone();

            let result = tokio::task::spawn_blocking(move || {
                let child = crate::services::platform::shell::shell_command_builder(&cmd_owned)
                    .current_dir(&working_dir_clone)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .spawn();
                match child {
                    Ok(child) => child.wait_with_output(),
                    Err(e) => Err(e),
                }
            })
            .await;

            let response = match result {
                Ok(Ok(output)) => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    let exit_code = output.status.code().unwrap_or(-1);
                    let mut parts = Vec::new();
                    if !stdout.is_empty() {
                        parts.push(format!("```\n{}\n```", stdout.trim_end()));
                    }
                    if !stderr.is_empty() {
                        parts.push(format!("stderr:\n```\n{}\n```", stderr.trim_end()));
                    }
                    if parts.is_empty() {
                        parts.push(format!("(exit code: {})", exit_code));
                    } else if exit_code != 0 {
                        parts.push(format!("(exit code: {})", exit_code));
                    }
                    parts.join("\n")
                }
                Ok(Err(e)) => format!("Failed to execute: {}", e),
                Err(e) => format!("Task error: {}", e),
            };

            send_long_message_raw(&ctx.http, channel_id, &response, &data.shared).await?;
            return Ok(true);
        }

        "!cc" => {
            let skill = arg1.to_string();
            let args_str = text
                .strip_prefix("!cc")
                .unwrap_or("")
                .trim()
                .strip_prefix(&skill)
                .unwrap_or("")
                .trim();
            let ts = chrono::Local::now().format("%H:%M:%S");
            println!(
                "  [{ts}] ◀ [{}] !cc {} {}",
                msg.author.name, skill, args_str
            );

            if skill.is_empty() {
                let _ = msg.reply(&ctx.http, "Usage: `!cc <skill> [args]`").await;
                return Ok(true);
            }

            // Handle built-in shortcuts
            match skill.as_str() {
                "clear" => {
                    let _ = msg.reply(&ctx.http, "Use `!clear` instead.").await;
                    return Ok(true);
                }
                "stop" => {
                    let mut d = data.shared.core.lock().await;
                    if let Some(token) = d.cancel_tokens.remove(&channel_id) {
                        token.cancel_with_tmux_cleanup();
                        drop(d);
                        let _ = msg.reply(&ctx.http, "Stopping...").await;
                    } else {
                        drop(d);
                        let _ = msg.reply(&ctx.http, "No active request to stop.").await;
                    }
                    return Ok(true);
                }
                "pwd" => {
                    // Delegate to !pwd
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!pwd")).await;
                }
                "health" => {
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!health"))
                        .await;
                }
                "status" => {
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!status"))
                        .await;
                }
                "inflight" => {
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!inflight"))
                        .await;
                }
                "help" => {
                    return Box::pin(handle_text_command(ctx, msg, data, channel_id, "!help"))
                        .await;
                }
                _ => {}
            }

            // Auto-restore session
            auto_restore_session(&data.shared, channel_id, ctx).await;

            // Verify skill exists
            let skill_exists = {
                let skills = data.shared.skills_cache.read().await;
                skills.iter().any(|(name, _)| name == &skill)
            };

            if !skill_exists {
                let _ = msg
                    .reply(
                        &ctx.http,
                        format!(
                            "Unknown skill: `{}`. Use `!cc` to see available skills.",
                            skill
                        ),
                    )
                    .await;
                return Ok(true);
            }

            // Check session exists
            let has_session = {
                let d = data.shared.core.lock().await;
                d.sessions
                    .get(&channel_id)
                    .and_then(|s| s.current_path.as_ref())
                    .is_some()
            };

            if !has_session {
                let _ = msg
                    .reply(&ctx.http, "No active session. Use `!start <path>` first.")
                    .await;
                return Ok(true);
            }

            // Block if AI is in progress
            {
                let d = data.shared.core.lock().await;
                if d.cancel_tokens.contains_key(&channel_id) {
                    drop(d);
                    let _ = msg
                        .reply(&ctx.http, "AI request in progress. Use `!stop` to cancel.")
                        .await;
                    return Ok(true);
                }
            }

            // Build the prompt
            let skill_prompt = match super::super::commands::build_provider_skill_prompt(
                &data.provider,
                &skill,
                args_str,
            ) {
                Ok(prompt) => prompt,
                Err(message) => {
                    let _ = msg.reply(&ctx.http, message).await;
                    return Ok(true);
                }
            };

            // Send confirmation and hand off to AI
            rate_limit_wait(&data.shared, channel_id).await;
            let confirm = channel_id
                .send_message(
                    &ctx.http,
                    CreateMessage::new().content(format!("Running skill: `/{skill}`")),
                )
                .await?;

            handle_text_message(
                ctx,
                channel_id,
                confirm.id,
                msg.author.id,
                &msg.author.name,
                &skill_prompt,
                &data.shared,
                &data.token,
                false,
                false,
                false,
                None,
            )
            .await?;
            return Ok(true);
        }

        _ => {}
    }

    Ok(false)
}

/// Private helper: reset provider session if a model change is pending.
async fn reset_provider_session_if_pending(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
    provider: &ProviderKind,
) {
    if shared
        .model_session_reset_pending
        .remove(&channel_id)
        .is_none()
    {
        return;
    }

    let channel_name = {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.session_id = None;
            session.channel_name.clone()
        } else {
            None
        }
    };

    if provider.uses_managed_tmux_backend() {
        if let Some(name) = channel_name.as_deref() {
            crate::services::claude::terminate_local_session(
                &provider.build_tmux_session_name(name),
            );
        }
    }

    if let Some(session_key) = build_adk_session_key(shared, channel_id, provider).await {
        super::super::adk_session::clear_provider_session_id(&session_key, shared.api_port).await;
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::DiscordSession;
    use super::*;
    use crate::services::memory::RecallResponse;

    fn make_session(
        current_path: Option<String>,
        remote_profile_name: Option<String>,
    ) -> DiscordSession {
        DiscordSession {
            session_id: None,
            current_path,
            history: Vec::new(),
            pending_uploads: Vec::new(),
            cleared: false,
            remote_profile_name,
            channel_id: None,
            channel_name: None,
            category_name: None,
            last_active: tokio::time::Instant::now(),
            worktree: None,
            born_generation: 0,
        }
    }

    fn sample_recall() -> RecallResponse {
        RecallResponse {
            shared_knowledge: Some("[Shared Knowledge]".to_string()),
            longterm_catalog: Some("- notes.md".to_string()),
            external_recall: Some("[External Recall]".to_string()),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn memory_injection_plan_routes_shared_knowledge_by_provider() {
        let recall = sample_recall();

        let claude = build_memory_injection_plan(
            &ProviderKind::Claude,
            false,
            DispatchProfile::Full,
            &recall,
        );
        assert_eq!(claude.shared_knowledge_for_context, None);
        assert_eq!(
            claude.shared_knowledge_for_system_prompt,
            Some("[Shared Knowledge]")
        );
        assert_eq!(
            claude.external_recall_for_context,
            Some("[External Recall]")
        );
        assert_eq!(
            claude.longterm_catalog_for_system_prompt,
            Some("- notes.md")
        );

        let codex = build_memory_injection_plan(
            &ProviderKind::Codex,
            false,
            DispatchProfile::Full,
            &recall,
        );
        assert_eq!(
            codex.shared_knowledge_for_context,
            Some("[Shared Knowledge]")
        );
        assert_eq!(codex.shared_knowledge_for_system_prompt, None);
        assert_eq!(codex.external_recall_for_context, Some("[External Recall]"));
        assert_eq!(codex.longterm_catalog_for_system_prompt, Some("- notes.md"));

        let qwen =
            build_memory_injection_plan(&ProviderKind::Qwen, false, DispatchProfile::Full, &recall);
        assert_eq!(
            qwen.shared_knowledge_for_context,
            Some("[Shared Knowledge]")
        );
        assert_eq!(qwen.shared_knowledge_for_system_prompt, None);
        assert_eq!(qwen.external_recall_for_context, Some("[External Recall]"));
        assert_eq!(qwen.longterm_catalog_for_system_prompt, Some("- notes.md"));
    }

    #[test]
    fn memory_injection_plan_keeps_review_lite_minimal() {
        let recall = sample_recall();
        let plan = build_memory_injection_plan(
            &ProviderKind::Codex,
            false,
            DispatchProfile::ReviewLite,
            &recall,
        );

        assert_eq!(plan.shared_knowledge_for_context, None);
        assert_eq!(plan.shared_knowledge_for_system_prompt, None);
        assert_eq!(plan.external_recall_for_context, None);
        assert_eq!(plan.longterm_catalog_for_system_prompt, None);
    }

    #[test]
    fn memory_injection_plan_skips_shared_knowledge_when_session_exists() {
        let recall = sample_recall();
        let plan =
            build_memory_injection_plan(&ProviderKind::Codex, true, DispatchProfile::Full, &recall);

        assert_eq!(plan.shared_knowledge_for_context, None);
        assert_eq!(plan.shared_knowledge_for_system_prompt, None);
        assert_eq!(plan.external_recall_for_context, Some("[External Recall]"));
        assert_eq!(plan.longterm_catalog_for_system_prompt, Some("- notes.md"));
    }

    #[test]
    fn memory_injection_plan_keeps_shared_knowledge_for_claude_resumed_sessions() {
        let recall = sample_recall();
        let plan = build_memory_injection_plan(
            &ProviderKind::Claude,
            true,
            DispatchProfile::Full,
            &recall,
        );

        assert_eq!(plan.shared_knowledge_for_context, None);
        assert_eq!(
            plan.shared_knowledge_for_system_prompt,
            Some("[Shared Knowledge]")
        );
        assert_eq!(plan.external_recall_for_context, Some("[External Recall]"));
        assert_eq!(plan.longterm_catalog_for_system_prompt, Some("- notes.md"));
    }

    #[test]
    fn session_path_is_usable_for_existing_local_path() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = make_session(Some(dir.path().to_str().unwrap().to_string()), None);
        assert!(session.validated_path("test-channel").is_some());
    }

    #[test]
    fn session_path_is_not_usable_for_missing_local_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().to_str().unwrap().to_string();
        drop(dir);
        let mut session = make_session(Some(missing_path), None);
        assert!(session.validated_path("test-channel").is_none());
        assert!(session.current_path.is_none());
        assert!(session.worktree.is_none());
    }

    #[test]
    fn session_path_is_stale_for_remote_session_with_missing_local_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().to_str().unwrap().to_string();
        drop(dir);
        let mut session = make_session(Some(missing_path), Some("mac-mini".to_string()));
        assert!(session.validated_path("test-channel").is_some());
        assert!(session.current_path.is_some());
    }
}
