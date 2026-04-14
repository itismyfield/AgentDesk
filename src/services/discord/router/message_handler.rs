use super::super::gateway::{DiscordGateway, LiveDiscordTurnContext};
use super::super::*;
use super::control_intent::{
    build_control_intent_system_reminder, detect_natural_language_control_intent,
};
use crate::services::memory::{
    RecallRequest, RecallResponse, build_memory_backend, resolve_memory_role_id,
    resolve_memory_session_id,
};
use crate::services::provider::{CancelToken, cancel_requested};
use poise::serenity_prelude::CreateMessage;
use std::sync::Arc;

#[derive(Debug, PartialEq, Eq)]
struct MemoryInjectionPlan<'a> {
    shared_knowledge_for_context: Option<&'a str>,
    shared_knowledge_for_system_prompt: Option<&'a str>,
    external_recall_for_context: Option<&'a str>,
    longterm_catalog_for_system_prompt: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionResetReason {
    IdleExpired,
    AssistantTurnCap,
}

fn build_memory_injection_plan<'a>(
    provider: &ProviderKind,
    has_session_id: bool,
    dispatch_profile: DispatchProfile,
    memory_recall: &'a RecallResponse,
) -> MemoryInjectionPlan<'a> {
    let should_inject_shared_knowledge =
        !has_session_id && dispatch_profile == DispatchProfile::Full;
    let shared_knowledge_for_context =
        if should_inject_shared_knowledge && !matches!(provider, ProviderKind::Claude) {
            memory_recall.shared_knowledge.as_deref()
        } else {
            None
        };
    let shared_knowledge_for_system_prompt =
        if should_inject_shared_knowledge && matches!(provider, ProviderKind::Claude) {
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

fn should_skip_memento_recall(
    memory_settings: &settings::ResolvedMemorySettings,
    memento_context_loaded: bool,
) -> bool {
    memory_settings.backend == settings::MemoryBackendKind::Memento && memento_context_loaded
}

fn should_add_turn_pending_reaction(dispatch_id: Option<&str>) -> bool {
    dispatch_id.is_none()
}

fn session_reset_reason_for_turn(
    session: &DiscordSession,
    now: tokio::time::Instant,
) -> Option<SessionResetReason> {
    if now.duration_since(session.last_active) > super::super::SESSION_MAX_IDLE {
        Some(SessionResetReason::IdleExpired)
    } else if session.assistant_turn_count() >= super::super::SESSION_MAX_ASSISTANT_TURNS {
        Some(SessionResetReason::AssistantTurnCap)
    } else {
        None
    }
}

fn format_session_retry_context(raw_context: &str) -> Option<String> {
    let raw_context = raw_context.trim();
    if raw_context.is_empty() {
        None
    } else {
        Some(format!(
            "[이전 대화 복원 — 새 세션 시작으로 최근 대화를 컨텍스트에 포함합니다]\n\n{raw_context}"
        ))
    }
}

fn merge_reply_contexts(primary: Option<String>, secondary: Option<String>) -> Option<String> {
    match (primary, secondary) {
        (Some(primary), Some(secondary)) => Some(format!("{secondary}\n\n{primary}")),
        (Some(primary), None) => Some(primary),
        (None, Some(secondary)) => Some(secondary),
        (None, None) => None,
    }
}

fn take_session_retry_context(channel_id: ChannelId) -> Option<String> {
    let key = super::super::session_retry_context_key(channel_id);
    super::super::internal_api::take_kv_value(&key)
        .ok()
        .flatten()
        .and_then(|raw| format_session_retry_context(&raw))
}
async fn send_restore_notification(
    shared: &Arc<SharedData>,
    fallback_http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    restored_session_id: Option<&str>,
) {
    let sid_full = restored_session_id.unwrap_or("?");
    let sid_short: String = sid_full.chars().take(8).collect();
    let restore_msg = format!(
        "📋 세션 복원: {} (session: {})",
        provider.as_str(),
        sid_short
    );

    if let Some(registry) = shared.health_registry() {
        match super::super::health::resolve_bot_http(registry.as_ref(), "notify").await {
            Ok(notify_http) => match channel_id.say(&*notify_http, &restore_msg).await {
                Ok(_) => return,
                Err(err) => {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ Restore notify send failed in channel {}: {} — falling back to provider bot",
                        channel_id,
                        err
                    );
                }
            },
            Err((status, body)) => {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ Restore notify bot unavailable in channel {}: {} {} — falling back to provider bot",
                    channel_id,
                    status,
                    body
                );
            }
        }
    } else {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ Restore notify bot unavailable in channel {}: health registry dropped — falling back to provider bot",
            channel_id
        );
    }

    if let Err(err) = channel_id.say(fallback_http, &restore_msg).await {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
            "  [{ts}] ⚠ Restore fallback send failed in channel {}: {}",
            channel_id,
            err
        );
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct DispatchContextHints {
    worktree_path: Option<String>,
    force_new_session: bool,
}

fn parse_dispatch_context_hints(
    dispatch_context: Option<&str>,
    dispatch_type: Option<&str>,
) -> DispatchContextHints {
    let parsed =
        dispatch_context.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
    let default_force_new_session =
        crate::dispatch::dispatch_type_force_new_session_default(dispatch_type).unwrap_or(false);
    DispatchContextHints {
        worktree_path: parsed
            .as_ref()
            .and_then(|v| v.get("worktree_path"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .filter(|p| std::path::Path::new(p).exists()),
        force_new_session: parsed
            .as_ref()
            .and_then(|v| v.get("force_new_session"))
            .and_then(|v| v.as_bool())
            .unwrap_or(default_force_new_session),
    }
}

fn load_session_runtime_state(
    sessions: &mut std::collections::HashMap<ChannelId, DiscordSession>,
    channel_id: ChannelId,
) -> Option<(Option<String>, bool, String)> {
    sessions.get_mut(&channel_id).and_then(|session| {
        let current_path = session.validated_path(channel_id)?;
        Some((
            session.session_id.clone(),
            session.memento_context_loaded,
            current_path,
        ))
    })
}

fn session_runtime_state_after_redirect(
    sessions: &mut std::collections::HashMap<ChannelId, DiscordSession>,
    original_channel_id: ChannelId,
    effective_channel_id: ChannelId,
    original_state: (Option<String>, bool, String),
) -> (Option<String>, bool, String) {
    if effective_channel_id == original_channel_id {
        return original_state;
    }

    load_session_runtime_state(sessions, effective_channel_id).unwrap_or(original_state)
}

fn build_race_requeued_intervention(
    request_owner: UserId,
    user_msg_id: MessageId,
    user_text: &str,
    reply_context: Option<String>,
    has_reply_boundary: bool,
    merge_consecutive: bool,
) -> Intervention {
    Intervention {
        author_id: request_owner,
        message_id: user_msg_id,
        source_message_ids: vec![user_msg_id],
        text: user_text.to_string(),
        mode: super::super::InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context,
        has_reply_boundary,
        merge_consecutive,
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
    merge_consecutive: bool,
    reply_context: Option<String>,
    has_reply_boundary: bool,
) -> Result<(), Error> {
    let original_channel_id = channel_id;
    let mut session_reset_reason = None;
    let mut reset_session_id_to_clear = None;
    // Get session info, allowed tools, and pending uploads
    let (session_info, provider, allowed_tools, pending_uploads) = {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id)
            && let Some(reason) =
                session_reset_reason_for_turn(session, tokio::time::Instant::now())
        {
            if let Some(retry_context) =
                session.recent_history_context(super::super::SESSION_RECOVERY_CONTEXT_MESSAGES)
            {
                let _ = super::super::internal_api::set_kv_value(
                    &super::super::session_retry_context_key(channel_id),
                    &retry_context,
                );
            }
            session_reset_reason = Some(reason);
            reset_session_id_to_clear = session.session_id.clone();
            session.clear_provider_session();
            session.history.clear();
        }
        let info = load_session_runtime_state(&mut data.sessions, channel_id);
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
    let is_dm_channel = matches!(
        channel_id.to_channel(&ctx.http).await.ok(),
        Some(serenity::Channel::Private(_))
    );
    let dm_default_agent = if is_dm_channel {
        super::super::agentdesk_config::resolve_dm_default_agent(&provider)
    } else {
        None
    };

    let (session_id, memento_context_loaded, current_path) = match session_info {
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
                        tracing::info!(
                            "  [{ts}] 🧵 Thread auto-start: resolved workspace from parent channel {}",
                            parent_id
                        );
                    }
                }
            }
            if workspace.is_none()
                && let Some(default_agent) = dm_default_agent.as_ref()
            {
                workspace = Some(default_agent.workspace.clone());
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 💬 DM auto-start: using default agent '{}' workspace",
                    default_agent.role_binding.role_id
                );
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
                    // Use both dispatch_thread_parents (for reused threads) AND Discord API
                    // thread detection (for first-turn in newly created threads where
                    // dispatch_thread_parents hasn't been populated yet).
                    let wt_info = {
                        let is_thread = shared.dispatch_thread_parents.contains_key(&channel_id)
                            || super::super::resolve_thread_parent(&ctx.http, channel_id)
                                .await
                                .is_some();
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
                                    tracing::info!(
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
                                    memento_context_loaded: false,
                                    memento_reflected: false,
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
                                    assistant_turns: 0,
                                });
                        session.current_path = Some(eff_path.clone());
                        session.channel_name = ch_name;
                        session.category_name = cat_name;
                        session.channel_id = Some(channel_id.get());
                        session.last_active = tokio::time::Instant::now();
                        session.worktree = wt_info;
                    }
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!("  [{ts}] ▶ Auto-started session from workspace: {eff_path}");
                    let session_state = {
                        let data = shared.core.lock().await;
                        data.sessions
                            .get(&channel_id)
                            .map(|s| (s.session_id.clone(), s.memento_context_loaded))
                            .unwrap_or((None, false))
                    };
                    (session_state.0, session_state.1, eff_path)
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

    let dispatch_id_for_thread = super::super::adk_session::parse_dispatch_id(user_text);
    if should_add_turn_pending_reaction(dispatch_id_for_thread.as_deref()) {
        add_reaction(ctx, channel_id, user_msg_id, '⏳').await;
    }

    // ── Dispatch thread auto-creation ──────────────────────────────
    // When a dispatch message arrives, create a Discord thread for
    // isolated context.  All subsequent agent output goes to the thread.
    // Skip if already inside a thread (threads cannot nest).
    // Thread reuse: if the card already has an active_thread_id, redirect
    // to the existing thread instead of creating a new one.
    let is_already_thread = super::super::resolve_thread_parent(&ctx.http, channel_id)
        .await
        .is_some();
    // #259: Fetch dispatch metadata once before thread creation so we can extract
    // worktree_path for both thread bootstrap and the subsequent session CWD override.
    let dispatch_info_cached = if let Some(ref did) = dispatch_id_for_thread {
        super::lookup_dispatch_info(shared.api_port, did).await
    } else {
        None
    };
    // #259: Prefer card-bound worktree over parent channel CWD for dispatch sessions.
    // All dispatch types now inject worktree_path into context via resolve_card_worktree().
    let mut dispatch_type_str = dispatch_info_cached
        .as_ref()
        .and_then(|info| info.dispatch_type.clone());
    let dispatch_context_hints = parse_dispatch_context_hints(
        dispatch_info_cached
            .as_ref()
            .and_then(|info| info.context.as_deref()),
        dispatch_type_str.as_deref(),
    );
    let dispatch_worktree_path = dispatch_context_hints.worktree_path.clone();
    let dispatch_force_new_session = dispatch_context_hints.force_new_session;
    if let (Some(wt), Some(did)) = (&dispatch_worktree_path, &dispatch_id_for_thread) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}] 🌿 Dispatch {did}: resolved worktree CWD: {wt}");
    }
    let dispatch_default_path = crate::services::platform::resolve_repo_dir()
        .filter(|p| std::path::Path::new(p).is_dir())
        .unwrap_or_else(|| current_path.clone());
    let dispatch_effective_path = dispatch_worktree_path
        .clone()
        .unwrap_or_else(|| dispatch_default_path.clone());
    if dispatch_worktree_path.is_none() && dispatch_id_for_thread.is_some() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🌱 Dispatch fallback CWD: using repo root instead of inherited session path: {}",
            dispatch_effective_path
        );
    }
    let channel_id = if let Some(ref did) = dispatch_id_for_thread {
        // Use cached dispatch metadata for thread reuse and cross-channel role override
        let dispatch_info = &dispatch_info_cached;
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
            // Ensure thread is accessible (unarchive if needed) before proceeding
            if !super::verify_thread_accessible(ctx, channel_id).await {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ Dispatch {did} thread {channel_id} is not accessible (archived/locked), skipping"
                );
                return Ok(());
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧵 Dispatch {did} arrived in existing thread, skipping thread creation"
            );
            // For review dispatches in reused threads, set role override
            // so this turn uses the counter-model channel's role/model.
            if is_counter_model_dispatch {
                if let Some(alt_ch) = alt_channel_id {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
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
                    tracing::info!(
                        "  [{ts}] 🧵 Reusing existing thread {} for dispatch {}",
                        tid,
                        did
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
                            tracing::info!(
                                "  [{ts}] 🔄 Review dispatch reusing thread: overriding role to alt channel {}",
                                alt_ch
                            );
                            shared.dispatch_role_overrides.insert(tid, alt_ch);
                        }
                    }
                    Some(tid)
                } else {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] 🧵 Thread {} is locked/inaccessible, creating new for {}",
                        tid,
                        did
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
                        tracing::info!(
                            "  [{ts}] 🧵 Created dispatch thread {} for dispatch {}",
                            thread.id,
                            did
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
                        tracing::warn!("  [{ts}] ⚠ Failed to create dispatch thread: {e}");
                        channel_id // fallback to main channel
                    }
                }
            }
        }
    } else {
        channel_id
    };
    let active_dispatch_id_for_prompt =
        super::super::adk_session::lookup_pending_dispatch_for_thread(
            shared.api_port,
            channel_id.get(),
        )
        .await
        .or_else(|| dispatch_id_for_thread.clone());
    let active_dispatch_info = match active_dispatch_id_for_prompt.as_deref() {
        Some(did) if dispatch_id_for_thread.as_deref() == Some(did) => dispatch_info_cached.clone(),
        Some(did) => super::lookup_dispatch_info(shared.api_port, did).await,
        None => None,
    };
    if let Some(active_dispatch_type) = active_dispatch_info
        .as_ref()
        .and_then(|info| info.dispatch_type.clone())
    {
        dispatch_type_str = Some(active_dispatch_type);
    }

    let (mut session_id, mut memento_context_loaded, current_path) = {
        let mut data = shared.core.lock().await;
        session_runtime_state_after_redirect(
            &mut data.sessions,
            original_channel_id,
            channel_id,
            (session_id, memento_context_loaded, current_path),
        )
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
    if dispatch_force_new_session {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.clear_provider_session();
        }
        drop(data);
        session_id = None;
        memento_context_loaded = false;
        if let Some(ref did) = dispatch_id_for_thread {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ♻️ Dispatch {did}: force_new_session=true, skipping provider session reuse"
            );
        }
    }

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
    }
    .or_else(|| {
        dm_default_agent
            .as_ref()
            .map(|resolved| resolved.role_binding.clone())
    });

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
        active_dispatch_id_for_prompt
            .as_ref()
            .and_then(|_| dispatch_type_str.as_deref()),
    );

    super::super::commands::reset_provider_session_if_pending(
        &ctx.http, shared, &provider, channel_id,
    )
    .await;
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
    if session_reset_reason.is_some() {
        if let Some(ref key) = adk_session_key {
            super::super::adk_session::clear_provider_session_id(key, shared.api_port).await;
        }
        if let Some(ref session_id_to_clear) = reset_session_id_to_clear {
            let _ = super::super::internal_api::clear_stale_session_id(session_id_to_clear).await;
        }
    }
    if session_id.is_none() {
        if dispatch_force_new_session {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for forced fresh dispatch turn"
            );
        } else if let Some(reason) = session_reset_reason {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let reason = match reason {
                SessionResetReason::IdleExpired => "idle timeout",
                SessionResetReason::AssistantTurnCap => "assistant turn cap",
            };
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for channel {} due to {}",
                channel_id.get(),
                reason
            );
        } else if let Some(ref key) = adk_session_key {
            let restored = super::super::adk_session::fetch_provider_session_id(
                key,
                &provider,
                shared.api_port,
            )
            .await;
            if restored.is_some() {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ Restored provider session_id from DB for {}",
                    key
                );
                let mut data = shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.restore_provider_session(restored.clone());
                }
                memento_context_loaded = true;
                // Notify: session restored — send before placeholder so it appears first
                send_restore_notification(
                    shared,
                    &ctx.http,
                    channel_id,
                    &provider,
                    restored.as_deref(),
                )
                .await;
            }
            session_id = restored;
        }
    }
    let reply_context = merge_reply_contexts(reply_context, take_session_retry_context(channel_id));

    // Send placeholder message (after restore notification so restore appears first)
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

    // Create cancel token — with second check to close the TOCTOU race window.
    // Multiple messages can pass the initial cancel_tokens check (line 169) concurrently
    // because the async gap between check and insert allows interleaving.
    // If another message won the race, queue ourselves and clean up.
    let cancel_token = Arc::new(CancelToken::new());
    let started = super::super::mailbox_try_start_turn(
        shared,
        channel_id,
        cancel_token.clone(),
        request_owner,
        user_msg_id,
    )
    .await;
    if !started {
        let bot_owner_provider = super::super::resolve_discord_bot_provider(token);
        let _ = super::super::mailbox_enqueue_intervention(
            shared,
            &bot_owner_provider,
            channel_id,
            build_race_requeued_intervention(
                request_owner,
                user_msg_id,
                user_text,
                reply_context.clone(),
                has_reply_boundary,
                merge_consecutive,
            ),
        )
        .await;
        // Clean up: remove placeholder and reaction created before this check
        let _ = channel_id
            .delete_message(&ctx.http, placeholder_msg_id)
            .await;
        super::super::formatting::remove_reaction_raw(&ctx.http, channel_id, user_msg_id, '⏳')
            .await;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 🔀 RACE: message queued (another turn won), channel {}",
            channel_id
        );
        return Ok(());
    }
    shared
        .global_active
        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    shared
        .turn_start_times
        .insert(channel_id, std::time::Instant::now());

    let (memory_settings, memory_backend) = build_memory_backend(role_binding.as_ref());
    let memory_recall = if should_skip_memento_recall(&memory_settings, memento_context_loaded) {
        RecallResponse::default()
    } else {
        memory_backend
            .recall(RecallRequest {
                provider: provider.clone(),
                role_id: resolve_memory_role_id(role_binding.as_ref()),
                channel_id: channel_id.get(),
                session_id: resolve_memory_session_id(session_id.as_deref(), channel_id.get()),
                dispatch_profile,
                user_text: user_text.to_string(),
            })
            .await
    };
    if memory_settings.backend == settings::MemoryBackendKind::Memento && !memento_context_loaded {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.note_memento_context_loaded();
        }
    }
    for warning in &memory_recall.warnings {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::warn!(
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
    if let Some(knowledge) = memory_injection_plan.shared_knowledge_for_context {
        context_chunks.push(knowledge.to_string());
    }
    if let Some(external_recall) = memory_injection_plan.external_recall_for_context {
        context_chunks.push(external_recall.to_string());
    }
    if let Some(control_intent) = detect_natural_language_control_intent(user_text) {
        context_chunks.push(build_control_intent_system_reminder(&control_intent));
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
    let narrate_progress = settings::load_narrate_progress(shared.db.as_ref());
    let current_task_context = active_dispatch_info.as_ref().map(|info| {
        super::super::prompt_builder::CurrentTaskContext {
            card_title: info.card_title.as_deref(),
            github_issue_url: info.github_issue_url.as_deref(),
            issue_body: info.issue_body.as_deref(),
            deferred_dod: info.deferred_dod.as_ref(),
        }
    });

    let system_prompt_owned = build_system_prompt(
        &discord_context,
        &current_path,
        channel_id,
        token,
        &disabled_notice,
        narrate_progress,
        role_binding.as_ref(),
        reply_to_user_message,
        dispatch_profile,
        dispatch_type_str.as_deref(),
        current_task_context.as_ref(),
        sak_for_system,
        longterm_catalog_for_prompt,
        Some(&memory_settings),
    );
    if sak_for_system.is_some() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 📦 SAK in system prompt ({} chars) for channel {}",
            sak_for_system.unwrap().len(),
            channel_id.get()
        );
    }
    let prompt_prep_duration_ms = prompt_prep_started.elapsed().as_millis();
    let memory_backend_label = memory_settings.backend.as_str();
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
    tracing::info!(
        "  [{ts}] [prompt-prep] channel={} provider={} dispatch={} memory_backend={} memory_profile={} reused_session={} duration_ms={}",
        channel_id.get(),
        provider_label,
        dispatch_profile_label,
        memory_backend_label,
        memory_settings.mem0.profile,
        session_id.is_some(),
        prompt_prep_duration_ms
    );
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
                    super::super::clear_watchdog_deadline_override(watchdog_channel_id_num).await;
                    return;
                }

                // Check for API-based deadline extension
                if let Some(new_deadline) =
                    super::super::take_watchdog_deadline_override(watchdog_channel_id_num).await
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
                    tracing::info!(
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
                                        tracing::info!(
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
                let is_current_token =
                    super::super::mailbox_cancel_token(&watchdog_shared, channel_id)
                        .await
                        .is_some_and(|current| std::sync::Arc::ptr_eq(&watchdog_token, &current));
                if is_current_token {
                    let elapsed_mins =
                        (now - (current_deadline - timeout.as_millis() as i64)) / 1000 / 60;
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::info!(
                        "  [{ts}] ⏰ WATCHDOG: turn timeout (~{elapsed_mins}m) for channel {}, cancelling",
                        channel_id
                    );
                    // #441: cancel_active_token → token.cancelled triggers turn_bridge loop exit
                    // → mailbox_finish_turn canonical cleanup
                    super::super::turn_bridge::cancel_active_token(
                        &watchdog_token,
                        true,
                        "watchdog timeout",
                    );

                    // Notify Discord
                    let has_queued = super::super::mailbox_has_pending_soft_queue(
                        &watchdog_shared,
                        &watchdog_provider,
                        channel_id,
                    )
                    .await
                    .has_pending;
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
    let adk_thread_channel_id = adk_session_name
        .as_deref()
        .and_then(super::super::adk_session::parse_thread_channel_id_from_name)
        .or_else(|| {
            shared
                .dispatch_thread_parents
                .contains_key(&channel_id)
                .then_some(channel_id.get())
        });
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
        adk_thread_channel_id,
        role_binding
            .as_ref()
            .map(|binding| binding.role_id.as_str()),
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

    let (logical_channel_id, thread_id, thread_title) = if let Some((parent_id, _parent_name)) =
        super::super::resolve_thread_parent(&ctx.http, channel_id).await
    {
        let (live_thread_title, _) = super::super::resolve_channel_category(ctx, channel_id).await;
        (parent_id.get(), Some(channel_id.get()), live_thread_title)
    } else {
        (channel_id.get(), None, None)
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
    inflight_state.logical_channel_id = Some(logical_channel_id);
    inflight_state.thread_id = thread_id;
    inflight_state.thread_title = thread_title;
    // Persist identifiers for long-turn diagnostics (#130)
    inflight_state.session_key = adk_session_key.clone();
    inflight_state.dispatch_id = dispatch_id.clone();
    if let Err(e) = save_inflight_state(&inflight_state) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}]   ⚠ inflight state save failed: {e}");
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
                        Some(0) => tracing::info!("  [{ts}] 🔄 worktree-autosync [{ws}]: {msg}"),
                        Some(1) => {
                            tracing::info!("  [{ts}] ⏭ worktree-autosync [{ws}]: skipped — {msg}")
                        }
                        _ => tracing::warn!("  [{ts}] ⚠ worktree-autosync [{ws}]: error — {msg}"),
                    }
                }
                Err(e) => tracing::warn!("  [{ts}] ⚠ worktree-autosync: failed to run — {e}"),
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
                tracing::warn!("  [streaming] Error: {}", e);
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
                tracing::warn!("  [streaming] PANIC: {}", msg);
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
        shared.clone(),
        cancel_token.clone(),
        rx,
        TurnBridgeContext {
            provider: provider.clone(),
            gateway: Arc::new(DiscordGateway::new(
                ctx.http.clone(),
                shared.clone(),
                provider.clone(),
                Some(LiveDiscordTurnContext {
                    ctx: ctx.clone(),
                    token: token.to_string(),
                    request_owner,
                }),
            )),
            channel_id,
            user_msg_id,
            user_text_owned: user_text.to_string(),
            request_owner_name: request_owner_name.to_string(),
            role_binding: role_binding.clone(),
            adk_session_key,
            adk_session_name,
            adk_session_info: Some(adk_session_info),
            adk_cwd: Some(current_path.clone()),
            dispatch_id,
            memory_recall_usage: memory_recall.token_usage,
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

pub(super) enum TextStopLookup {
    NoActiveTurn,
    AlreadyStopping,
    Stop(Arc<CancelToken>),
}

#[cfg_attr(not(test), allow(dead_code))]
pub(super) fn lookup_text_stop_token(
    cancel_tokens: &std::collections::HashMap<serenity::ChannelId, Arc<CancelToken>>,
    channel_id: serenity::ChannelId,
) -> TextStopLookup {
    match cancel_tokens.get(&channel_id).cloned() {
        Some(token) if cancel_requested(Some(token.as_ref())) => TextStopLookup::AlreadyStopping,
        Some(token) => TextStopLookup::Stop(token),
        None => TextStopLookup::NoActiveTurn,
    }
}

#[allow(dead_code)]
pub(super) async fn lookup_text_stop_token_mailbox(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> TextStopLookup {
    match super::super::mailbox_cancel_token(shared, channel_id).await {
        Some(token) if cancel_requested(Some(token.as_ref())) => TextStopLookup::AlreadyStopping,
        Some(token) => TextStopLookup::Stop(token),
        None => TextStopLookup::NoActiveTurn,
    }
}

pub(super) async fn cancel_text_stop_token_mailbox(
    shared: &Arc<SharedData>,
    channel_id: serenity::ChannelId,
) -> TextStopLookup {
    let result = super::super::mailbox_cancel_active_turn(shared, channel_id).await;
    match result.token {
        Some(_) if result.already_stopping => TextStopLookup::AlreadyStopping,
        Some(token) => TextStopLookup::Stop(token),
        None => TextStopLookup::NoActiveTurn,
    }
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
    super::super::commands::handle_text_command(ctx, msg, data, channel_id, text).await
}
#[cfg(test)]
mod tests {
    use super::super::super::DiscordSession;
    use super::*;
    use crate::services::memory::RecallResponse;
    use crate::ui::ai_screen::{HistoryItem, HistoryType};
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::time::Duration;

    fn sample_recall() -> RecallResponse {
        RecallResponse {
            shared_knowledge: Some("[Shared Knowledge]".to_string()),
            longterm_catalog: Some("- notes.md".to_string()),
            external_recall: Some("[External Recall]".to_string()),
            warnings: Vec::new(),
            token_usage: crate::services::memory::TokenUsage::default(),
        }
    }

    fn make_session(
        current_path: Option<String>,
        remote_profile_name: Option<String>,
    ) -> DiscordSession {
        DiscordSession {
            session_id: None,
            memento_context_loaded: false,
            memento_reflected: false,
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
            assistant_turns: 0,
        }
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
        // Verify current_path and worktree were cleared
        assert!(session.current_path.is_none());
        assert!(session.worktree.is_none());
    }

    #[test]
    fn session_path_is_stale_for_remote_session_with_missing_local_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing_path = dir.path().to_str().unwrap().to_string();
        drop(dir);
        let mut session = make_session(Some(missing_path), Some("mac-mini".to_string()));
        // Remote sessions keep their CWD even when it is non-local to this machine.
        assert!(session.validated_path("test-channel").is_some());
        assert!(session.current_path.is_some());
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
    fn memento_recall_skip_only_triggers_for_loaded_memento_sessions() {
        let memento = settings::ResolvedMemorySettings {
            backend: settings::MemoryBackendKind::Memento,
            ..settings::ResolvedMemorySettings::default()
        };
        let file = settings::ResolvedMemorySettings::default();

        assert!(should_skip_memento_recall(&memento, true));
        assert!(!should_skip_memento_recall(&memento, false));
        assert!(!should_skip_memento_recall(&file, true));
    }

    #[test]
    fn dispatch_turns_skip_generic_pending_reaction() {
        let dispatch_id = crate::services::discord::adk_session::parse_dispatch_id(
            "DISPATCH:550e8400-e29b-41d4-a716-446655440000 - Fix login bug",
        );

        assert!(!should_add_turn_pending_reaction(dispatch_id.as_deref()));
    }

    #[test]
    fn regular_turns_keep_generic_pending_reaction() {
        assert!(should_add_turn_pending_reaction(None));
    }

    #[test]
    fn clear_resets_memento_skip_so_next_turn_can_reload_context() {
        let memento = settings::ResolvedMemorySettings {
            backend: settings::MemoryBackendKind::Memento,
            ..settings::ResolvedMemorySettings::default()
        };
        let mut session = make_session(Some("/tmp/project".to_string()), None);

        session.restore_provider_session(Some("session-1".to_string()));
        assert!(should_skip_memento_recall(
            &memento,
            session.memento_context_loaded
        ));

        session.clear_provider_session();
        assert!(!should_skip_memento_recall(
            &memento,
            session.memento_context_loaded
        ));
    }

    #[test]
    fn session_reset_reason_triggers_after_idle_timeout() {
        let mut session = make_session(Some("/tmp/project".to_string()), None);
        let now = tokio::time::Instant::now();
        session.last_active = now - Duration::from_secs(60 * 60) - Duration::from_secs(1);

        assert_eq!(
            session_reset_reason_for_turn(&session, now),
            Some(SessionResetReason::IdleExpired)
        );
    }

    #[test]
    fn session_reset_reason_triggers_after_assistant_turn_cap() {
        let mut session = make_session(Some("/tmp/project".to_string()), None);
        session.history = (0..100)
            .map(|idx| HistoryItem {
                item_type: HistoryType::Assistant,
                content: format!("assistant-{idx}"),
            })
            .collect();

        assert_eq!(
            session_reset_reason_for_turn(&session, tokio::time::Instant::now()),
            Some(SessionResetReason::AssistantTurnCap)
        );
    }

    #[test]
    fn merge_reply_contexts_prefers_retry_context_first() {
        assert_eq!(
            merge_reply_contexts(
                Some("reply context".to_string()),
                Some("retry context".to_string())
            )
            .as_deref(),
            Some("retry context\n\nreply context")
        );
    }

    #[test]
    fn parse_dispatch_context_hints_extracts_force_new_session_and_worktree() {
        let temp = tempfile::tempdir().unwrap();
        let raw = serde_json::json!({
            "worktree_path": temp.path(),
            "force_new_session": true
        })
        .to_string();

        let hints = parse_dispatch_context_hints(Some(&raw), Some("review-decision"));

        assert_eq!(hints.worktree_path.as_deref(), temp.path().to_str());
        assert!(hints.force_new_session);
    }

    #[test]
    fn parse_dispatch_context_hints_ignores_missing_path_but_keeps_reset_flag() {
        let hints = parse_dispatch_context_hints(
            Some(r#"{"worktree_path":"/definitely/missing","force_new_session":true}"#),
            Some("review-decision"),
        );

        assert!(hints.worktree_path.is_none());
        assert!(hints.force_new_session);
    }

    #[test]
    fn parse_dispatch_context_hints_defaults_fresh_session_for_work_dispatches() {
        let implementation = parse_dispatch_context_hints(None, Some("implementation"));
        let review = parse_dispatch_context_hints(None, Some("review"));
        let rework = parse_dispatch_context_hints(None, Some("rework"));

        assert!(implementation.force_new_session);
        assert!(review.force_new_session);
        assert!(rework.force_new_session);
    }

    #[test]
    fn parse_dispatch_context_hints_defaults_warm_resume_for_review_decision() {
        let hints = parse_dispatch_context_hints(None, Some("review-decision"));
        assert!(!hints.force_new_session);
    }

    #[test]
    fn parse_dispatch_context_hints_respects_explicit_override_over_dispatch_type_default() {
        let hints =
            parse_dispatch_context_hints(Some(r#"{"force_new_session":false}"#), Some("rework"));
        assert!(!hints.force_new_session);
    }

    #[test]
    fn session_runtime_state_after_redirect_prefers_reused_thread_state() {
        let parent_dir = tempfile::tempdir().unwrap();
        let thread_dir = tempfile::tempdir().unwrap();
        let parent_channel_id = ChannelId::new(100);
        let thread_channel_id = ChannelId::new(200);

        let mut sessions = std::collections::HashMap::new();
        let mut parent = make_session(Some(parent_dir.path().to_str().unwrap().to_string()), None);
        parent.restore_provider_session(Some("parent-session".to_string()));
        sessions.insert(parent_channel_id, parent);

        let thread = make_session(Some(thread_dir.path().to_str().unwrap().to_string()), None);
        sessions.insert(thread_channel_id, thread);

        let resolved = session_runtime_state_after_redirect(
            &mut sessions,
            parent_channel_id,
            thread_channel_id,
            (
                Some("parent-session".to_string()),
                true,
                parent_dir.path().to_str().unwrap().to_string(),
            ),
        );

        assert_eq!(resolved.0, None);
        assert!(!resolved.1);
        assert_eq!(resolved.2, thread_dir.path().to_str().unwrap());
    }

    #[test]
    fn session_runtime_state_after_redirect_keeps_original_state_when_channel_unchanged() {
        let channel_id = ChannelId::new(100);
        let dir = tempfile::tempdir().unwrap();
        let original = (
            Some("session-1".to_string()),
            true,
            dir.path().to_str().unwrap().to_string(),
        );

        let resolved = session_runtime_state_after_redirect(
            &mut std::collections::HashMap::new(),
            channel_id,
            channel_id,
            original.clone(),
        );

        assert_eq!(resolved, original);
    }

    #[test]
    fn race_requeue_preserves_reply_boundary_without_reply_context() {
        let queued = build_race_requeued_intervention(
            UserId::new(7),
            MessageId::new(8),
            "hello",
            None,
            true,
            true,
        );

        assert!(queued.has_reply_boundary);
        assert!(queued.reply_context.is_none());
        assert!(queued.merge_consecutive);
    }

    #[test]
    fn race_requeue_preserves_non_mergeable_turns() {
        let queued = build_race_requeued_intervention(
            UserId::new(7),
            MessageId::new(8),
            "hello",
            None,
            false,
            false,
        );

        assert!(!queued.has_reply_boundary);
        assert!(!queued.merge_consecutive);
    }
}
