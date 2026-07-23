use super::super::super::queue_marker;
use super::super::super::turn_view_reconciler::{
    note_intake_turn_cleared_current as tv_clear_current,
    note_intake_turn_started_current_with_attempt as tv_start_current_with_attempt,
};
use super::voice_announcement_route::route_voice_transcript_announcement_once;
use super::*;

mod adk_thread;
mod claim_bootstrap;
pub(crate) mod inflight_create_log;
pub(super) mod race_loss;
mod stale_dispatch_guard;
mod steering_hook;
mod turn_watchdog;
mod voice_intake;
mod worker_entry;

pub(crate) use worker_entry::{IntakeRequest, execute_intake_turn_core};

/// Bundle of Discord-runtime dependencies that `handle_text_message`
/// reads from outside its per-message parameters. Phase 2-pre.2 of
/// intake-node-routing (docs/design/intake-node-routing.md): the body
/// reads only `http` and (optionally) `cache`, both of which are REST-
/// or cache-backed primitives. Worker-side callers without a live shard
/// pass `cache: None` and `ctx_for_chained_dispatch: None`; leader-side
/// callers pass `Some(&ctx.cache)` and `Some(ctx)` to preserve the
/// in-process category cache and the chained-dispatch path.
///
/// `ctx_for_chained_dispatch` is the only remaining `&serenity::Context`
/// dependency: `DiscordGateway::new` accepts an optional
/// `LiveDiscordTurnContext { ctx, .. }` that wires the queued-turn
/// hand-off back through the gateway's live shard. Workers cannot
/// participate in that flow (they have no shard) so they pass `None`
/// and the gateway is constructed with `live_turn = None`.
#[derive(Clone, Copy)]
pub(in crate::services::discord) struct IntakeDeps<'a> {
    pub http: &'a Arc<serenity::http::Http>,
    pub cache: Option<&'a Arc<serenity::cache::Cache>>,
    pub ctx_for_chained_dispatch: Option<&'a serenity::Context>,
    pub shared: &'a Arc<SharedData>,
    pub token: &'a str,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_text_message(
    deps: &IntakeDeps<'_>,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    request_owner: UserId,
    request_owner_name: &str,
    user_text: &str,
    reply_to_user_message: bool,
    defer_watcher_resume: bool,
    wait_for_completion: bool,
    merge_consecutive: bool,
    reply_context: Option<String>,
    has_reply_boundary: bool,
    dm_hint: Option<bool>,
    turn_kind: TurnKind,
    preserve_on_cancel: bool,
    queued_drain: bool,
    preloaded_uploads: Vec<String>,
    gate_resolved_voice_announcement: Option<crate::voice::prompt::VoiceTranscriptAnnouncement>,
) -> Result<(), Error> {
    let IntakeDeps {
        http,
        cache,
        ctx_for_chained_dispatch,
        shared,
        token,
    } = *deps;
    let original_channel_id = channel_id;
    let stored_voice_announcement =
        crate::voice::announce_meta::global_store().take_with_acceptance(user_msg_id);
    let has_stored_voice_announcement = stored_voice_announcement.is_some();
    let has_legacy_voice_announcement =
        crate::voice::prompt::is_voice_transcript_announcement_candidate(user_text);
    let is_readable_voice_announcement =
        crate::voice::prompt::is_readable_voice_transcript_announcement(user_text);
    let voice_announcement_ref = if is_readable_voice_announcement {
        crate::voice::prompt::parse_voice_transcript_announcement_ref(user_text)
    } else {
        None
    };
    let announce_bot_id = if has_stored_voice_announcement
        || has_legacy_voice_announcement
        || is_readable_voice_announcement
    {
        super::super::super::resolve_announce_bot_user_id(shared).await
    } else {
        None
    };
    let (voice_announcement, voice_announcement_already_accepted) =
        voice_intake::resolve_intake_voice_announcement(
            shared,
            channel_id,
            user_msg_id,
            request_owner,
            announce_bot_id,
            is_readable_voice_announcement,
            &voice_announcement_ref,
            gate_resolved_voice_announcement,
            stored_voice_announcement,
        )
        .await;
    // #3464: scope unauthorized voice announcements to the owning agent — a
    // non-owning agent must not fall through to generic handling and answer
    // (multi-agent reply storm). Decision + one-shot warn live in
    // `voice_announcement_scope` (this file is LoC-frozen).
    if super::voice_announcement_scope::drop_unauthorized_voice_announcement(
        has_stored_voice_announcement,
        has_legacy_voice_announcement,
        is_readable_voice_announcement,
        voice_announcement.is_some(),
        announce_bot_id,
        request_owner,
        channel_id,
        user_msg_id,
    ) {
        return Ok(());
    }
    let is_voice_announcement = voice_announcement.is_some();
    let voice_prompt_text = voice_announcement.as_ref().map(|announcement| {
        let mut context = format!("voice_utterance_id: {}", announcement.utterance_id);
        if let Some(started_at) = announcement.started_at.as_deref() {
            context.push_str(&format!("\nvoice_started_at: {started_at}"));
        }
        if let Some(completed_at) = announcement.completed_at.as_deref() {
            context.push_str(&format!("\nvoice_completed_at: {completed_at}"));
        }
        if let Some(samples_written) = announcement.samples_written {
            context.push_str(&format!("\nvoice_samples_written: {samples_written}"));
        }
        crate::voice::prompt::voice_bridge_prompt(
            &announcement.transcript,
            &announcement.language,
            announcement.verbose_progress,
            Some(&context),
        )
    });
    // #2266: keep the original Discord author (the announce bot, for a voice-transcript announcement) so the race-loss enqueue path can
    // attribute the queued `Intervention` to the announce bot. When the
    // queued turn later re-enters `handle_text_message` via the
    // dispatch/kickoff hooks, the same `announce_bot_id == Some(request_owner)`
    // check (line ~2274) will pass and the reinserted voice payload (or
    // the embedded copy lifted into `stored_voice_announcement`) will be
    // honored instead of treated as spoofed. The post-rebind
    // `request_owner` below is the voice user id, used only for the rest
    // of the active-turn flow.
    let original_request_owner = request_owner;
    let voice_request_owner_name;
    let request_owner = voice_announcement
        .as_ref()
        .and_then(|announcement| announcement.user_id.parse::<u64>().ok())
        .map(UserId::new)
        .unwrap_or(request_owner);
    let request_owner_name = if let Some(announcement) = voice_announcement.as_ref() {
        voice_request_owner_name = format!("voice-user-{}", announcement.user_id);
        voice_request_owner_name.as_str()
    } else {
        request_owner_name
    };
    let user_text = voice_announcement
        .as_ref()
        .map(|announcement| announcement.transcript.as_str())
        .unwrap_or(user_text);
    let voice_route_outcome = route_voice_transcript_announcement_once(
        shared.pg_pool.as_ref(),
        channel_id,
        user_msg_id,
        voice_announcement_already_accepted,
        voice_announcement.as_ref(),
        |announcement| {
            let voice_barge_in = shared.voice_barge_in.clone();
            let shared = Arc::clone(shared);
            async move {
                voice_barge_in
                    .try_handle_voice_transcript_announcement(&shared, channel_id, &announcement)
                    .await
            }
        },
    )
    .await;
    if voice_route_outcome.bypasses_normal_turn() {
        return Ok(());
    }
    if !is_voice_announcement
        && shared
            .voice_barge_in
            .try_handle_voice_channel_text_reply(http, channel_id, user_text)
            .await
    {
        return Ok(());
    }
    let is_dm_channel = matches!(
        channel_id.to_channel(http).await.ok(),
        Some(serenity::Channel::Private(_))
    );
    let is_dm_channel = super::super::super::resolve_is_dm_channel(dm_hint, is_dm_channel);
    shared.record_channel_speaker(channel_id, request_owner, request_owner_name, is_dm_channel);
    let (settings_provider, allowed_tools) = {
        let settings = shared.settings.read().await;
        (settings.provider.clone(), settings.allowed_tools.clone())
    };
    let dm_default_agent = if is_dm_channel {
        super::super::super::agentdesk_config::resolve_dm_default_agent(&settings_provider)
    } else {
        None
    };
    let (early_stale_session_id, early_channel_name) = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .map(|session| (session.session_id.clone(), session.channel_name.clone()))
            .unwrap_or_default()
    };
    let early_thread_parent = super::super::super::resolve_thread_parent(http, channel_id).await;
    let early_resolved_channel_name = if early_channel_name.is_none() {
        let (channel_name, _) = resolve_channel_category(http, cache, channel_id).await;
        channel_name
    } else {
        None
    };
    let early_role_binding = resolve_thread_role_binding(
        channel_id,
        early_channel_name
            .as_deref()
            .or(early_resolved_channel_name.as_deref()),
        early_thread_parent.as_ref(),
    )
    .role_binding
    .or_else(|| {
        dm_default_agent
            .as_ref()
            .map(|resolved| resolved.role_binding.clone())
    });
    let early_provider = early_role_binding
        .as_ref()
        .and_then(|binding| binding.provider.clone())
        .unwrap_or_else(|| settings_provider.clone());
    let early_fast_mode_channel_id =
        effective_fast_mode_channel_id(channel_id, early_thread_parent.clone());
    if let GoalCommandKind::Lifecycle(command) = classify_codex_goal_command_for_provider(
        &early_provider,
        user_text,
        super::super::super::commands::channel_codex_goals_setting(
            shared,
            early_fast_mode_channel_id,
        )
        .await,
    ) {
        consume_codex_goal_lifecycle_command(
            http,
            shared,
            &early_provider,
            channel_id,
            command,
            early_stale_session_id,
        )
        .await;
        return Ok(());
    }
    // Get session info, allowed tools, and pending uploads
    let (session_info, mut pending_uploads, session_was_cleared) = {
        let mut data = shared.core.lock().await;
        let info = load_session_runtime_state(&mut data.sessions, channel_id);
        let (uploads, was_cleared) = data
            .sessions
            .get_mut(&channel_id)
            .map(|s| {
                let was_cleared = s.cleared;
                s.cleared = false;
                (std::mem::take(&mut s.pending_uploads), was_cleared)
            })
            .unwrap_or_default();
        drop(data);
        (info, uploads, was_cleared)
    };
    pending_uploads.extend(preloaded_uploads);
    let provider = settings_provider;
    let dispatch_id_for_thread = super::super::super::adk_session::parse_dispatch_id(user_text);
    let dispatch_info_cached = if let Some(ref did) = dispatch_id_for_thread {
        super::super::lookup_dispatch_info(shared.api_port, did).await
    } else {
        None
    };
    let pre_session_dispatch_type = dispatch_info_cached
        .as_ref()
        .and_then(|info| info.dispatch_type.as_deref());

    let (session_id, memento_context_loaded, current_path, auto_start_provider_isolated) =
        match session_info {
            Some(info) => (info.0, info.1, info.2, false),
            None => {
                // Try auto-start from role_map workspace
                let ch_name = {
                    let data = shared.core.lock().await;
                    data.sessions
                        .get(&channel_id)
                        .and_then(|s| s.channel_name.clone())
                };
                let mut workspace = settings::resolve_workspace(channel_id, ch_name.as_deref());
                if workspace.is_none()
                    && let Some((parent_id, parent_name)) = early_thread_parent.as_ref()
                {
                    let parent_ch_name = parent_name.clone().or_else(|| {
                        let data = shared.core.try_lock().ok()?;
                        data.sessions
                            .get(parent_id)
                            .and_then(|session| session.channel_name.clone())
                    });
                    workspace = resolve_thread_workspace(
                        channel_id,
                        ch_name.as_deref(),
                        Some(&(*parent_id, parent_ch_name)),
                    );
                    if workspace.is_some() {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 🧵 Thread auto-start: inherited workspace from parent channel {}",
                            parent_id
                        );
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
                        let (ch_name_api, cat_name) =
                            resolve_channel_category(http, cache, channel_id).await;
                        let ch_name = match super::super::super::resolve_thread_parent(
                            http, channel_id,
                        )
                        .await
                        {
                            Some((_parent_id, parent_name)) => {
                                let parent =
                                    parent_name.unwrap_or_else(|| format!("{}", _parent_id));
                                Some(super::super::super::synthetic_thread_channel_name(
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
                        let (wt_info, provider_isolation_applied) = {
                            let is_thread =
                                shared.dispatch.thread_parents.contains_key(&channel_id)
                                    || super::super::super::resolve_thread_parent(http, channel_id)
                                        .await
                                        .is_some();
                            let data = shared.core.lock().await;
                            let conflict =
                                detect_worktree_conflict(&data.sessions, &canonical, channel_id);
                            drop(data);
                            let provider_isolation_policy =
                                super::super::super::agentdesk_config::resolve_worktree_isolation_policy(
                                    channel_id,
                                    ch_name.as_deref(),
                                );
                            let provider_isolation_required =
                                provider_isolation_policy.as_ref().is_some_and(|policy| {
                                    should_force_provider_worktree_isolation(
                                        policy.non_main_provider_channel,
                                        policy.isolate_override,
                                        pre_session_dispatch_type,
                                    )
                                });
                            let needs_worktree =
                                is_thread || conflict.is_some() || provider_isolation_required;
                            let wt_info = if needs_worktree {
                                let reason = if is_thread {
                                    "thread session"
                                } else if provider_isolation_required {
                                    "provider isolation"
                                } else {
                                    "conflict"
                                };
                                let ch = ch_name.as_deref().unwrap_or("unknown");
                                // #3207 (part 2): reuse this channel's EXISTING managed
                                // worktree when one is persisted, instead of rotating a new
                                // timestamped worktree every cold start. A rotated worktree
                                // moves the cwd's claude project dir, so the prior session's
                                // transcript is gone and `--resume` is structurally impossible
                                // → fresh session + lost conversation. Reusing the worktree
                                // keeps the sid's jsonl discoverable so the launch genuinely
                                // resumes. Same persisted mapping + safety filters as the
                                // #3011 thread-bootstrap reuse.
                                let reused = resolve_reusable_worktree(
                                    shared.pg_pool.as_ref(),
                                    &shared.token_hash,
                                    &provider,
                                    ch,
                                    channel_id.get(),
                                    &canonical,
                                );
                                if let Some(wt) = reused {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        "  [{ts}] ↻ Auto-start worktree reused ({reason}): {ch} → {} (branch: {})",
                                        wt.worktree_path,
                                        wt.branch_name
                                    );
                                    Some(wt)
                                } else {
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
                                }
                            } else {
                                None
                            };
                            let provider_isolation_applied =
                                provider_isolation_required && wt_info.is_some();
                            (wt_info, provider_isolation_applied)
                        };
                        let eff_path = wt_info
                            .as_ref()
                            .map(|wt| wt.worktree_path.clone())
                            .unwrap_or_else(|| canonical.clone());
                        {
                            let mut data = shared.core.lock().await;
                            let session = data.sessions.entry(channel_id).or_insert_with(|| {
                                DiscordSession {
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

                                    born_generation:
                                        super::super::super::runtime_store::process_generation(),
                                }
                            });
                            session.current_path = Some(eff_path.clone());
                            session.channel_name = ch_name;
                            session.category_name = cat_name;
                            session.channel_id = Some(channel_id.get());
                            session.last_active = tokio::time::Instant::now();
                            session.worktree = wt_info;
                            if provider_isolation_applied {
                                session.clear_provider_session();
                                session.memento_context_loaded = false;
                            }
                        }
                        if provider_isolation_applied
                            && let Some(key) =
                                build_adk_session_key(shared, channel_id, &provider, None).await
                        {
                            super::super::super::adk_session::clear_provider_session_id(
                                &key,
                                shared.api_port,
                            )
                            .await;
                        }
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ▶ Auto-started session from workspace: {eff_path}"
                        );
                        let session_state = {
                            let data = shared.core.lock().await;
                            data.sessions
                                .get(&channel_id)
                                .map(|s| (s.session_id.clone(), s.memento_context_loaded))
                                .unwrap_or((None, false))
                        };
                        (
                            session_state.0,
                            session_state.1,
                            eff_path,
                            provider_isolation_applied,
                        )
                    } else {
                        rate_limit_wait(shared, channel_id).await;
                        let _ = channel_id
                            .say(http, "No active session. Use `/start <path>` first.")
                            .await;
                        return Ok(());
                    }
                } else {
                    rate_limit_wait(shared, channel_id).await;
                    let _ = channel_id
                        .say(http, "No active session. Use `/start <path>` first.")
                        .await;
                    return Ok(());
                }
            }
        };
    let turn_start_attempt = if should_add_turn_pending_reaction(dispatch_id_for_thread.as_deref())
        && !super::super::super::voice_barge_in::is_synthetic_voice_message_id(user_msg_id)
    {
        tv_start_current_with_attempt(shared, http, channel_id, user_msg_id, "intake_start")
            .await
            .attempt()
    } else {
        None
    };

    // ── Dispatch thread auto-creation ──────────────────────────────
    // When a dispatch message arrives, create a Discord thread for
    // isolated context.  All subsequent agent output goes to the thread.
    // Skip if already inside a thread (threads cannot nest).
    // Thread reuse: if the card already has an active_thread_id, redirect
    // to the existing thread instead of creating a new one.
    let is_already_thread = super::super::super::resolve_thread_parent(http, channel_id)
        .await
        .is_some();
    // #259: Fetch dispatch metadata once before thread creation so we can extract
    // worktree_path for both thread bootstrap and the subsequent session CWD override.
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
    let dispatch_stale_worktree_path = dispatch_context_hints.stale_worktree_path.clone();
    let dispatch_target_repo = dispatch_context_hints.target_repo.clone();
    let dispatch_reset_provider_state = dispatch_context_hints.reset_provider_state;
    let dispatch_recreate_tmux = dispatch_context_hints.recreate_tmux;
    let dispatch_retry_resume_session_id = dispatch_context_hints.retry_resume_session_id.clone();
    if let (Some(wt), Some(did)) = (&dispatch_worktree_path, &dispatch_id_for_thread) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}] 🌿 Dispatch {did}: resolved worktree CWD: {wt}");
    }
    // #762: when the dispatch pins an external target_repo but emits no
    // worktree_path (e.g. refresh fell back without a usable path), resolve
    // the repo's configured directory first instead of dropping straight into
    // the default AgentDesk repo. Otherwise external-repo reviews silently
    // execute in the wrong repo.
    let dispatch_target_repo_path =
        resolve_dispatch_target_repo_dir(dispatch_target_repo.as_deref());
    let dispatch_default_path = dispatch_target_repo_path
        .clone()
        .or_else(|| {
            crate::services::platform::resolve_repo_dir()
                .filter(|p| std::path::Path::new(p).is_dir())
        })
        .unwrap_or_else(|| current_path.clone());
    let mut dispatch_effective_path = dispatch_worktree_path
        .clone()
        .unwrap_or_else(|| dispatch_default_path.clone());
    if dispatch_worktree_path.is_none() && dispatch_id_for_thread.is_some() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        if let (Some(stale_path), Some(did)) = (
            dispatch_stale_worktree_path.as_deref(),
            dispatch_id_for_thread.as_deref(),
        ) {
            tracing::warn!(
                "  [{ts}] ⚠ Dispatch {did}: context worktree_path no longer exists: {} — falling back to {}",
                stale_path,
                dispatch_effective_path
            );
        } else if let (Some(did), Some(tr), Some(tr_path)) = (
            dispatch_id_for_thread.as_deref(),
            dispatch_target_repo.as_deref(),
            dispatch_target_repo_path.as_deref(),
        ) {
            tracing::info!(
                "  [{ts}] 🌱 Dispatch {did}: no worktree_path; honoring target_repo '{}' at {}",
                tr,
                tr_path
            );
        } else {
            tracing::info!(
                "  [{ts}] 🌱 Dispatch fallback CWD: using repo root instead of inherited session path: {}",
                dispatch_effective_path
            );
        }
    }
    let dispatch_uses_thread_routing =
        crate::dispatch::dispatch_type_uses_thread_routing(dispatch_type_str.as_deref());
    let mut bootstrapped_fresh_thread_session = false;
    let channel_id = if let Some(ref did) = dispatch_id_for_thread {
        if !dispatch_uses_thread_routing {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 📢 Dispatch {did} uses primary-channel routing, skipping thread creation"
            );
            channel_id
        } else {
            // Use cached dispatch metadata for thread reuse and cross-channel role override
            let dispatch_info = &dispatch_info_cached;
            let is_counter_model_dispatch =
                crate::services::dispatches::outbox_route::use_counter_model_channel(
                    dispatch_type_str.as_deref(),
                );
            let alt_channel_id = dispatch_info
                .as_ref()
                .and_then(|i| i.discord_channel_alt.as_deref())
                .and_then(|s| s.parse::<u64>().ok())
                .map(ChannelId::new);

            if is_already_thread {
                // Ensure thread is accessible (unarchive if needed) before proceeding
                if !super::super::verify_thread_accessible(http, channel_id).await {
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
                        shared.dispatch.role_overrides.insert(channel_id, alt_ch);
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
                    if super::super::verify_thread_accessible(http, tid).await {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] 🧵 Reusing existing thread {} for dispatch {}",
                            tid,
                            did
                        );
                        bootstrapped_fresh_thread_session =
                            super::super::super::bootstrap_thread_session(
                                shared,
                                tid,
                                &dispatch_effective_path,
                                http,
                                cache,
                            )
                            .await;
                        shared.dispatch.thread_parents.insert(channel_id, tid);
                        // For review dispatches reusing an implementation thread,
                        // override role/model to use the counter-model channel.
                        if is_counter_model_dispatch {
                            if let Some(alt_ch) = alt_channel_id {
                                let ts = chrono::Local::now().format("%H:%M:%S");
                                tracing::info!(
                                    "  [{ts}] 🔄 Review dispatch reusing thread: overriding role to alt channel {}",
                                    alt_ch
                                );
                                shared.dispatch.role_overrides.insert(tid, alt_ch);
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
                            http,
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
                            bootstrapped_fresh_thread_session =
                                super::super::super::bootstrap_thread_session(
                                    shared,
                                    thread.id,
                                    &dispatch_effective_path,
                                    http,
                                    cache,
                                )
                                .await;
                            shared.dispatch.thread_parents.insert(channel_id, thread.id);
                            super::super::link_dispatch_thread(
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
        }
    } else {
        channel_id
    };
    let final_thread_parent = super::super::super::resolve_thread_parent(http, channel_id).await;
    let mut authoritative = dispatch_worktree_path.is_some() || dispatch_target_repo_path.is_some();
    if dispatch_should_recover_session_worktree(
        dispatch_id_for_thread.is_some(),
        dispatch_type_str.as_deref(),
        dispatch_worktree_path.is_some(),
    ) {
        let session_worktree_path = {
            let data = shared.core.lock().await;
            data.sessions
                .get(&channel_id)
                .and_then(|session| session.worktree.as_ref())
                .map(|worktree| worktree.worktree_path.clone())
                .filter(|path| std::path::Path::new(path).is_dir())
        };
        if let Some(worktree_path) = session_worktree_path {
            authoritative = true;
            if dispatch_effective_path != worktree_path {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🌿 Dispatch recovered thread worktree CWD: {} → {}",
                    dispatch_effective_path,
                    worktree_path
                );
                dispatch_effective_path = worktree_path;
            }
        }
    }
    let active_dispatch_id_for_prompt =
        super::super::super::adk_session::lookup_pending_dispatch_for_thread(
            shared.api_port,
            channel_id.get(),
        )
        .await
        .or_else(|| dispatch_id_for_thread.clone());
    let active_dispatch_info = match active_dispatch_id_for_prompt.as_deref() {
        Some(did) if dispatch_id_for_thread.as_deref() == Some(did) => dispatch_info_cached.clone(),
        Some(did) => super::super::lookup_dispatch_info(shared.api_port, did).await,
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
    let mut session_strategy_reason = if session_id.is_some() {
        "runtime_cached_provider_session"
    } else if bootstrapped_fresh_thread_session {
        "thread_session_bootstrapped"
    } else if auto_start_provider_isolated {
        "provider_channel_worktree_isolated"
    } else {
        "no_runtime_provider_session"
    };

    // #259/#762: Keep runtime/session CWD aligned with the selected dispatch path.
    // This also corrects reused threads whose cached path differs from target_repo.
    // Inherited workspace applies only without an authoritative dispatch CWD;
    // the existing update block persists that selection for later turns.
    let final_workspace = apply_final_thread_workspace(
        shared,
        channel_id,
        final_thread_parent.as_ref(),
        (&mut dispatch_effective_path, authoritative),
    )
    .await;
    let mut current_path = if dispatch_session_path_should_update(
        dispatch_id_for_thread.is_some(),
        dispatch_type_str.as_deref(),
        dispatch_worktree_path.is_some(),
        bootstrapped_fresh_thread_session && !final_workspace,
        &current_path,
        &dispatch_effective_path,
    ) {
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            if session.current_path.as_deref() != Some(dispatch_effective_path.as_str()) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔄 Dispatch session CWD update: {:?} → {}",
                    session.current_path,
                    dispatch_effective_path
                );
                session.current_path = Some(dispatch_effective_path.clone());
            }
        }
        dispatch_effective_path.clone()
    } else {
        current_path
    };
    if let Some(active_info) = active_dispatch_info.as_ref() {
        let active_hints = parse_dispatch_context_hints(
            active_info.context.as_deref(),
            dispatch_type_str.as_deref(),
        );
        if let Some(active_worktree_path) = active_hints.worktree_path.as_deref()
            && current_path != active_worktree_path
        {
            let original_path =
                resolve_dispatch_target_repo_dir(active_hints.target_repo.as_deref())
                    .unwrap_or_else(|| dispatch_default_path.clone());
            let mut data = shared.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔄 Active dispatch CWD update: {:?} → {}",
                    session.current_path,
                    active_worktree_path
                );
                session.current_path = Some(active_worktree_path.to_string());
                if crate::dispatch::dispatch_type_requires_fresh_worktree(
                    dispatch_type_str.as_deref(),
                ) {
                    session.worktree = Some(WorktreeInfo {
                        original_path,
                        worktree_path: active_worktree_path.to_string(),
                        branch_name: active_hints.worktree_branch.unwrap_or_default(),
                    });
                }
            }
            current_path = active_worktree_path.to_string();
        }
    }
    // Sanitize input
    let sanitized_input =
        ai_screen::sanitize_user_input(voice_prompt_text.as_deref().unwrap_or(user_text));

    let mut resolved_role_binding = {
        // For cross-channel dispatch reuse (e.g. review in implementation thread),
        // resolve role from the override channel instead of the thread's parent.
        if let Some(override_ch) = shared.dispatch.role_overrides.get(&channel_id) {
            let alt_ch = *override_ch;
            ResolvedThreadRoleBinding::direct(resolve_role_binding(alt_ch, None))
        } else {
            let data = shared.core.lock().await;
            let ch_name = data
                .sessions
                .get(&channel_id)
                .and_then(|s| s.channel_name.as_deref());
            resolve_thread_role_binding(channel_id, ch_name, final_thread_parent.as_ref())
        }
    };
    if resolved_role_binding.role_binding.is_none() {
        resolved_role_binding.role_binding = dm_default_agent
            .as_ref()
            .map(|resolved| resolved.role_binding.clone());
    }
    let memory_scope_channel_id = resolved_role_binding.memory_channel_id(channel_id);
    let memory_channel_id = memory_scope_channel_id.get();
    let memory_channel_name = resolved_role_binding.memory_channel_name(None);
    let role_binding = resolved_role_binding.role_binding;

    // For cross-channel dispatch reuse, override the provider so the turn
    // executes via the counter-model CLI (e.g. Codex reviews Claude's work).
    let provider = if shared.dispatch.role_overrides.contains_key(&channel_id) {
        role_binding
            .as_ref()
            .and_then(|rb| rb.provider.clone())
            .unwrap_or(provider)
    } else {
        provider
    };

    {
        let channel_name_for_isolation = {
            let data = shared.core.lock().await;
            data.sessions
                .get(&channel_id)
                .and_then(|session| session.channel_name.clone())
        };
        let isolation_outcome = ensure_provider_worktree_isolation(
            shared,
            channel_id,
            &mut current_path,
            &provider,
            channel_name_for_isolation.as_deref(),
            dispatch_type_str.as_deref(),
        )
        .await;
        reset_provider_session_after_worktree_isolation(
            shared,
            channel_id,
            &provider,
            isolation_outcome,
            &mut session_id,
            &mut memento_context_loaded,
            &mut session_strategy_reason,
        )
        .await;
    }

    if matches!(provider, ProviderKind::Codex)
        && !dispatch_reset_provider_state
        && !dispatch_recreate_tmux
        && let Some(resume_session_id) = dispatch_retry_resume_session_id.as_deref()
    {
        if session_id.as_deref() != Some(resume_session_id) {
            let mut data = shared.core.lock().await;
            if let Some(session) = data.sessions.get_mut(&channel_id) {
                session.restore_provider_session(Some(resume_session_id.to_string()));
                memento_context_loaded = session.memento_context_loaded;
            } else {
                memento_context_loaded = false;
            }
            session_id = Some(resume_session_id.to_string());
        }
        session_strategy_reason = "dispatch_context_retry_resume";
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ Dispatch retry: using context-supplied Codex resume session for channel {}",
            channel_id.get()
        );
    }

    // Derive dispatch prompt profile before memory recall so ReviewLite can
    // skip heavy memory work consistently across supported backends.
    let dispatch_profile = {
        let dispatch_type = active_dispatch_id_for_prompt
            .as_ref()
            .and_then(|_| dispatch_type_str.as_deref());
        let data = shared.core.lock().await;
        let channel_name = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.as_deref());
        DispatchProfile::for_turn(
            dispatch_type,
            settings::resolve_dispatch_profile(channel_id, channel_name),
        )
    };

    if dispatch_reset_provider_state || dispatch_recreate_tmux {
        super::super::super::commands::reset_channel_provider_state(
            http,
            shared,
            &provider,
            channel_id,
            if dispatch_recreate_tmux {
                "dispatch hard reset"
            } else {
                "dispatch provider reset"
            },
            dispatch_reset_provider_state,
            false,
            dispatch_recreate_tmux,
        )
        .await;
        session_id = None;
        memento_context_loaded = false;
        session_strategy_reason =
            dispatch_reset_lifecycle_code(dispatch_reset_provider_state, dispatch_recreate_tmux);
        if let Some(ref did) = dispatch_id_for_thread {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ♻️ Dispatch {did}: reset_provider_state={}, recreate_tmux={}, skipping provider session reuse",
                dispatch_reset_provider_state,
                dispatch_recreate_tmux
            );
        }
    }

    let fast_mode_channel_id =
        effective_fast_mode_channel_id(channel_id, final_thread_parent.clone());
    super::super::super::commands::reset_provider_session_if_pending(
        http,
        shared,
        &provider,
        channel_id,
        fast_mode_channel_id,
    )
    .await;
    let prompt_prep_started = std::time::Instant::now();

    let state = (
        current_path,
        session_id,
        memento_context_loaded,
        session_strategy_reason,
    );
    let runtime = resolve_channel_runtime_for_launch(shared, &provider, channel_id, state).await;
    current_path = runtime.0;
    session_id = runtime.1;
    memento_context_loaded = runtime.2;
    session_strategy_reason = runtime.3;
    let (channel_name, tmux_session_name) = (runtime.4, runtime.5);
    let adk_session_key = build_adk_session_key(shared, channel_id, &provider, None).await;
    let turn_goal_kind = if !dispatch_reset_provider_state && !dispatch_recreate_tmux {
        classify_codex_goal_command_for_provider(
            &provider,
            user_text,
            super::super::super::commands::channel_codex_goals_setting(
                shared,
                fast_mode_channel_id,
            )
            .await,
        )
    } else {
        GoalCommandKind::NotGoal
    };
    if let GoalCommandKind::Lifecycle(command) = turn_goal_kind {
        if should_add_turn_pending_reaction(dispatch_id_for_thread.as_deref())
            && !super::super::super::voice_barge_in::is_synthetic_voice_message_id(user_msg_id)
        {
            tv_clear_current(shared, http, channel_id, user_msg_id, "intake_goal").await;
        }
        consume_codex_goal_lifecycle_command(
            http,
            shared,
            &provider,
            channel_id,
            command,
            session_id.clone(),
        )
        .await;
        return Ok(());
    }
    let force_fresh_provider_session = matches!(turn_goal_kind, GoalCommandKind::FreshStart);
    if force_fresh_provider_session {
        record_fresh_session_context_boundary(shared, channel_id).await?;
        clear_codex_goal_start_provider_session(
            shared,
            channel_id,
            adk_session_key.as_deref(),
            &mut session_id,
            &mut memento_context_loaded,
            &mut session_strategy_reason,
        )
        .await;
    }
    let sanitized_input = if force_fresh_provider_session {
        rewrite_fresh_goal_prompt(&sanitized_input)
    } else {
        sanitized_input
    };
    if session_id.is_none() {
        if force_fresh_provider_session {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for channel {} due to /goal fresh session request",
                channel_id.get()
            );
        } else if session_was_cleared {
            session_strategy_reason = "session_cleared_by_user";
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for channel {} due to prior /clear",
                channel_id.get()
            );
        } else if dispatch_reset_provider_state || dispatch_recreate_tmux {
            session_strategy_reason = dispatch_reset_lifecycle_code(
                dispatch_reset_provider_state,
                dispatch_recreate_tmux,
            );
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for dispatch reset_provider_state={} recreate_tmux={}",
                dispatch_reset_provider_state,
                dispatch_recreate_tmux
            );
        } else if let Some(ref key) = adk_session_key {
            let restored = super::super::super::adk_session::fetch_provider_session_id(
                key,
                &provider,
                shared.api_port,
            )
            .await;
            if let Some(restored_session_id) = restored {
                session_strategy_reason = "db_provider_session_restored";
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ Restored provider session_id from DB for {}",
                    key
                );
                let mut data = shared.core.lock().await;
                if let Some(session) = data.sessions.get_mut(&channel_id) {
                    session.restore_provider_session(Some(restored_session_id.clone()));
                    memento_context_loaded = session.memento_context_loaded;
                }
                session_id = Some(restored_session_id);
            } else if let Some((recovered, recovered_memento_context_loaded)) =
                restore_live_tui_provider_session_from_binding(
                    shared,
                    channel_id,
                    &provider,
                    tmux_session_name.as_deref(),
                    Some(key),
                )
                .await
            {
                session_strategy_reason = "live_tui_runtime_binding_restored";
                memento_context_loaded = recovered_memento_context_loaded;
                session_id = Some(recovered);
            } else {
                session_strategy_reason = "no_cached_provider_session";
                session_id = None;
            }
        } else {
            session_strategy_reason = "session_key_unavailable";
        }
    }
    let turn_id = format!("discord:{}:{}", channel_id.get(), user_msg_id.get());

    // #1332: probe turn liveness BEFORE posting any placeholder so a queued
    // message renders the dedicated `📬 메시지 대기 중` card instead of the
    // misleading `🔄 백그라운드 처리 중` Active card. The previous order
    // (send_intake_placeholder → mailbox_try_start_turn) made every queued
    // message look like processing had already begun.
    //
    // Create cancel token — with second check to close the TOCTOU race window.
    // Multiple messages can pass the initial cancel_tokens check (line 169) concurrently
    // because the async gap between check and insert allows interleaving.
    // If another message won the race, queue ourselves and clean up.
    let cancel_token = Arc::new(CancelToken::new());
    let started = try_start_turn_with_stale_busy_heal(
        shared,
        channel_id,
        cancel_token.clone(),
        request_owner,
        user_msg_id,
        intake_claim_context(
            adk_session_key.as_deref(),
            &provider,
            tmux_session_name.as_deref(),
            &mut current_path,
            &mut session_id,
            &mut memento_context_loaded,
            &mut session_strategy_reason,
        ),
    )
    .await;

    // #3813 Phase 1a: intake latency span anchor (turn claimed; observation-only
    // — see latency_spans.rs). Never `.log()`'d on the early returns below.
    let mut intake_latency = super::latency_spans::IntakeLatencySpans::turn_claimed();

    if stale_dispatch_guard::abort_terminal_dispatch_at_turn_start(
        http,
        shared,
        &provider,
        channel_id,
        user_msg_id,
        user_text,
        started,
        preserve_on_cancel,
    )
    .await
    {
        return Ok(());
    }

    claim_bootstrap::bootstrap_claimed_turn(
        http,
        shared,
        started,
        channel_id,
        user_msg_id,
        &provider,
        adk_session_key.as_deref(),
    )
    .await;

    // #1332 dispatch hand-off: if this turn was previously enqueued and is now
    // being dispatched, reuse the Queued placeholder card so the user sees a
    // single message transition `📬 → 🔄` instead of two distinct placeholders.
    //
    // codex review P2 (round-after-#1332): merged interventions accumulate
    // multiple `source_message_ids`; each lost a separate race and registered
    // its own queued placeholder. Drain mappings for ALL of them — the head
    // (intervention.message_id) becomes the live Active card, and any
    // additional source ids' Discord cards must be tidied up so the user does
    // not see duplicate `📬` cards left behind for the merged tail.
    let queued_placeholder_handoff = if started {
        // Use the write-through helper so the on-disk snapshot stays in sync
        // with the in-memory map (codex review round-3 P2). Round-5 P2: the
        // helper now takes the per-channel async persistence mutex, so this
        // dispatch hand-off serializes against any concurrent race-loss
        // render path on the same channel.
        shared
            .remove_queued_placeholder(channel_id, user_msg_id)
            .await
    } else {
        None
    };

    // #3182: the normal dequeue path removed the `📬 메시지 대기 중` placeholder
    // CARD above (`remove_queued_placeholder`) but historically left the
    // queue-pending REACTION (`📬` standalone / `➕` merged) on the user message,
    // so a processed message kept showing `📬`+`✅` together — the queue version
    // of the #3164 stuck-hourglass. The reaction was added in `intake_gate` via
    // the provider bot's `ctx.http`, which is the SAME @me identity as the `http`
    // available here (kickoff passes `&ctx.http`, the live dispatch passes
    // `&live_turn.ctx.http`; both resolve to `cached_serenity_ctx` = the provider
    // bot — see runtime_bootstrap.rs `cached_serenity_ctx.set(ctx)`), so this
    // `http` can actually remove the bot's own reaction (Discord remove-reaction
    // only clears the calling bot's @me reaction).
    //
    // AUTHORITATIVE cleanup lives at the two queued-dispatch ENTRYPOINTS, which
    // each clear `📬`/`➕` on EVERY `source_message_ids` entry (head + merged
    // tails) BEFORE re-entering `handle_text_message`:
    //   • live dispatch — `DiscordGateway::dispatch_queued_turn` (gateway.rs)
    //   • idle/restart kickoff — `kickoff_idle_queues` (mod.rs, #3182 codex P1)
    // That entrypoint pass covers the merged-tail and no-mapping cases the
    // per-head hand-off cannot see. This block is an idempotent HEAD-level
    // belt-and-suspenders for the dispatch hand-off (where a `queued_placeholders`
    // mapping for the head was consumed just above): it re-asserts the head is
    // clear even if the message reached here by some path other than the two
    // entrypoints. Gate: only when THIS message actually held a queued
    // placeholder mapping (`queued_placeholder_handoff.is_some()`), and not for
    // background-trigger turns (#796 — those keep their info-only placeholder and
    // never reacted the user message). A redundant remove is a no-op, so this
    // composes safely with the entrypoint drains.
    if queued_placeholder_handoff.is_some() && !turn_kind.is_background_trigger() {
        for emoji in crate::services::discord::queue_reactions::QUEUE_PENDING_REACTION_EMOJIS {
            queue_marker::note_removed_current(
                shared,
                http,
                channel_id,
                user_msg_id,
                emoji,
                "dequeue_head_queue_marker_clear",
            )
            .await;
        }
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 📭 DEQUEUE: cleared queue-pending reaction(s) on promoted message (channel {}, msg {})",
            channel_id,
            user_msg_id
        );
    }

    // codex review P1/P2: when this turn lost the race, drive the entire
    // race-loss path (placeholder POST, mapping insert, enqueue, idle-drain
    // safety net, queued-card edit) here and return. Splitting into a
    // dedicated `if !started` block — instead of folding it into the
    // `placeholder_msg_id` let-binding below — keeps the started==true
    // path linear and lets us bail out without the post-let main flow ever
    // running on a non-active turn.
    if !started {
        return race_loss::handle_race_loss_enqueue(
            http,
            shared,
            token,
            &provider,
            channel_id,
            original_channel_id,
            turn_kind,
            original_request_owner,
            user_msg_id,
            user_text,
            &reply_context,
            has_reply_boundary,
            merge_consecutive,
            &pending_uploads,
            &voice_announcement,
            reply_to_user_message,
            &dispatch_id_for_thread,
            turn_start_attempt,
            preserve_on_cancel,
        )
        .await;
    }

    let placeholder_msg_id = if let Some(existing) = queued_placeholder_handoff {
        // #3480: the queued `📬 대기 중` card is now BURIED under what the active
        // turn streamed while this message waited; reusing it as the live anchor
        // edits that buried card (looks like a relay drop / huge 2000-char gaps).
        // Instead POST a FRESH bottom anchor like the clean-start branch below
        // (same reply-target gating), then DELETE the stale card. The old code's
        // `ensure_active` visual morph is obsolete once deleted, but on SUCCESS we
        // must still `detach_by_message` its controller row (seeded Queued by
        // intake_gate's `ensure_queued`): `remove_queued_placeholder` clears only
        // the `queued_placeholders` map and `Queued` rows are excluded from
        // `evict_terminal_entries`, so skipping detach leaks one controller row
        // per dequeue — the same drop+detach invariant gateway.rs/mod.rs uphold.
        // Streaming owns the fresh id, so the fresh anchor needs no seeding.
        match send_intake_placeholder(
            http.clone(),
            shared.clone(),
            channel_id,
            if reply_to_user_message
                && dispatch_id_for_thread.is_none()
                && !super::super::super::voice_barge_in::is_synthetic_voice_message_id(user_msg_id)
            {
                Some((channel_id, user_msg_id))
            } else {
                None
            },
            false,
        )
        .await
        {
            Ok(fresh_msg_id) => {
                // Fresh anchor is live; tear down the buried queued card. Delete
                // failure is NON-fatal — never abort the turn over a lingering card.
                let deleted = channel_id.delete_message(http, existing).await;
                // Drop the stale card's controller row (else it leaks; see above).
                shared
                    .ui
                    .placeholder_controller
                    .detach_by_message(channel_id, existing);
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 📬➡️🔄 DISPATCH: queued dequeued; posted fresh anchor (channel {}, fresh_msg {}, stale {}, stale_deleted={})",
                    channel_id,
                    fresh_msg_id,
                    existing,
                    deleted.is_ok()
                );
                fresh_msg_id
            }
            Err(error) => {
                // Mirror the clean-start branch: release the mailbox slot so the
                // channel is not stuck at `current_msg_id == 0`. KEEP `existing`
                // (the queued card) visible as a fallback since the fresh POST failed.
                let bot_owner_provider = super::super::super::resolve_discord_bot_provider(token);
                let kicked = release_mailbox_after_placeholder_post_failure(
                    shared,
                    &bot_owner_provider,
                    channel_id,
                )
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ DEQUEUE: fresh anchor POST failed after mailbox slot acquired (channel {}, stale {}, error={}); released mailbox slot, kept stale card, kickoff_scheduled={}",
                    channel_id,
                    existing,
                    error,
                    kicked
                );
                let recovery = if kicked {
                    "mailbox_released_kickoff_rescheduled"
                } else {
                    "mailbox_released_kickoff_skipped"
                };
                crate::services::observability::emit_intake_placeholder_post_failed(
                    provider.as_str(),
                    channel_id.get(),
                    Some(user_msg_id.get()),
                    "dequeue_after_mailbox_slot",
                    recovery,
                    &error.to_string(),
                );
                return Err::<(), Error>(error.into());
            }
        }
    } else {
        // Active turn started cleanly — POST a fresh placeholder. On POST failure
        // we MUST release the mailbox slot we just acquired or the channel stalls
        // at `current_msg_id == 0` until the cancel token times out (codex P1).
        match send_intake_placeholder(
            http.clone(),
            shared.clone(),
            channel_id,
            if reply_to_user_message
                && dispatch_id_for_thread.is_none()
                && !super::super::super::voice_barge_in::is_synthetic_voice_message_id(user_msg_id)
            {
                Some((channel_id, user_msg_id))
            } else {
                None
            },
            // #3082 P2-3: this turn POSTs its OWN fresh placeholder (not a queued
            // "📬" notice) and is the one answering, so it must NOT gate behind a
            // multi-chunk answer flush — that would self-deadlock the active card.
            false,
        )
        .await
        {
            Ok(msg_id) => msg_id,
            Err(error) => {
                let bot_owner_provider = super::super::super::resolve_discord_bot_provider(token);
                let kicked = release_mailbox_after_placeholder_post_failure(
                    shared,
                    &bot_owner_provider,
                    channel_id,
                )
                .await;
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ INTAKE: placeholder POST failed after mailbox slot acquired (channel {}, error={}); released mailbox slot, kickoff_scheduled={}",
                    channel_id,
                    error,
                    kicked
                );
                // #1984 (codex C — observation): the mailbox slot is
                // released; whether a follow-up kickoff was scheduled
                // determines if the user message can still progress.
                let recovery = if kicked {
                    "mailbox_released_kickoff_rescheduled"
                } else {
                    "mailbox_released_kickoff_skipped"
                };
                crate::services::observability::emit_intake_placeholder_post_failed(
                    provider.as_str(),
                    channel_id.get(),
                    Some(user_msg_id.get()),
                    "intake_after_mailbox_slot",
                    recovery,
                    &error.to_string(),
                );
                return Err::<(), Error>(error.into());
            }
        }
    };
    let session_retry_context = take_session_retry_context(shared, channel_id, Some(&turn_id));
    let retry_reply_context = session_retry_context
        .as_ref()
        .map(|c| c.formatted_context.clone());
    let reply_context = merge_reply_contexts(reply_context, retry_reply_context);
    // #4307 PR-B: fold the voluntary tool_feedback reminder stashed last turn
    // into `reply_context`; the owned reminder is kept for the refusal put-back.
    let (feedback_reminder, reply_context) =
        take_and_merge_feedback_reminder(shared, &provider, channel_id, reply_context);
    // #4196: fold the turn-end WIP warning stashed last turn into `reply_context`
    // so the agent is reminded to commit/stash its uncommitted changes; the owned
    // warning is kept for the refusal put-back (same lifecycle as the reminder).
    let (wip_warning, reply_context) =
        take_and_merge_wip_warning(shared, &provider, channel_id, reply_context);

    // #3813 Phase 1a: the intake placeholder POST returned a live id.
    intake_latency.mark_placeholder_posted();
    crate::services::discord::increment_global_active(shared, "intake_after_mailbox_slot");
    shared
        .turn_start_times
        .insert(channel_id, std::time::Instant::now());
    log_session_strategy_diagnostic(
        channel_id,
        &provider,
        dispatch_profile,
        session_strategy_reason,
        session_id.as_deref(),
        adk_session_key.as_deref(),
        tmux_session_name.as_deref(),
        session_retry_context.is_some(),
        memento_context_loaded,
    )
    .await;
    let cli_was_just_spawned = cli_just_spawned_for_emit(tmux_session_name.as_deref());
    let recovery_message_count = session_retry_context
        .as_ref()
        .map(|ctx| ctx.recovery_message_count())
        .filter(|&count| count > 0);
    emit_session_strategy_lifecycle(
        shared,
        channel_id,
        &turn_id,
        adk_session_key.as_deref(),
        active_dispatch_id_for_prompt.as_deref(),
        session_id.as_deref(),
        session_strategy_reason,
        cli_was_just_spawned,
        recovery_message_count,
    )
    .await;

    let (memory_settings, memory_backend) = build_memory_backend(role_binding.as_ref());
    let memento_recall_gate = memento_recall_gate_decision(
        &memory_settings,
        memento_context_loaded,
        user_text,
        dispatch_profile,
    );
    let memory_recall = if !memento_recall_gate.should_recall {
        RecallResponse::default()
    } else {
        memory_backend
            .recall(RecallRequest {
                provider: provider.clone(),
                role_id: resolve_memory_role_id(role_binding.as_ref()),
                channel_id: memory_channel_id,
                channel_name: memory_channel_name.clone().or(channel_name.clone()),
                session_id: resolve_memory_session_id(session_id.as_deref(), memory_channel_id),
                dispatch_profile,
                user_text: user_text.to_string(),
                mode: memento_recall_gate.mode,
            })
            .await
    };
    if memory_settings.backend == settings::MemoryBackendKind::Memento {
        let ts = chrono::Local::now().format("%H:%M:%S");
        let recall_bytes = memory_recall
            .external_recall
            .as_deref()
            .map(str::len)
            .unwrap_or(0);
        let bucket = if !memento_recall_gate.should_recall {
            RecallSizeBucket::Skipped
        } else {
            match memento_recall_gate.mode {
                RecallMode::Full => RecallSizeBucket::Full,
                RecallMode::IdentityOnly => RecallSizeBucket::IdentityOnly,
            }
        };
        note_recall_context_size(bucket, recall_bytes);
        tracing::info!(
            "  [{ts}] [memory] memento recall gate for channel {}: decision={} mode={:?} reason={} context_loaded={} recall_bytes={} input_tokens={} output_tokens={}",
            channel_id.get(),
            if memento_recall_gate.should_recall {
                "inject"
            } else {
                "skip"
            },
            memento_recall_gate.mode,
            memento_recall_gate.reason,
            memento_context_loaded,
            recall_bytes,
            memory_recall.token_usage.input_tokens,
            memory_recall.token_usage.output_tokens
        );
    }
    if should_note_memento_context_loaded(&memory_settings, memento_context_loaded, &memory_recall)
    {
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
    let channel_recent_context = load_channel_recent_context(
        shared.pg_pool.as_ref(),
        channel_id,
        session_id.as_deref(),
        force_fresh_provider_session,
        session_was_cleared,
        // #4658: intake (live user) turns are never scheduled-snapshot turns.
        false,
        dispatch_profile,
        active_dispatch_id_for_prompt.as_deref(),
        session_retry_context.as_ref(),
    )
    .await;
    if !pending_uploads.is_empty() {
        context_chunks.push(pending_uploads.join("\n"));
    }
    if let Some(ref reply_ctx) = reply_context {
        context_chunks.push(reply_ctx.clone());
    }
    if let Some(ref recent_context) = channel_recent_context {
        recent_context.append_rendered_context_to(&mut context_chunks);
    }
    if let Some(ref knowledge) = memory_injection_plan.shared_knowledge_for_context {
        context_chunks.push(knowledge.to_string());
    }
    if let Some(external_recall) = memory_injection_plan.external_recall_for_context {
        context_chunks.push(external_recall.to_string());
    }
    context_chunks.push(wrap_user_prompt_with_author(
        request_owner_name,
        request_owner,
        sanitized_input,
    ));
    let context_prompt = crate::services::provider::compact_resumed_provider_turn_prompt(
        &provider,
        session_id.as_deref(),
        context_chunks.join("\n\n"),
    );
    // Build Discord context info
    let discord_context = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel_id);
        build_system_discord_context(
            session.and_then(|s| s.channel_name.as_deref()),
            session.and_then(|s| s.category_name.as_deref()),
            channel_id,
            false,
        )
    };
    // Claude keeps SAK in the system prompt for prefix-cache stability.
    // Non-Claude providers receive SAK in the user context instead.
    let sak_for_system = memory_injection_plan.sak_for_system_prompt();
    let current_task_context = active_dispatch_info.as_ref().map(|info| {
        super::super::super::prompt_builder::CurrentTaskContext {
            dispatch_id: active_dispatch_id_for_prompt.as_deref(),
            card_id: info.card_id.as_deref(),
            dispatch_title: info.dispatch_title.as_deref(),
            dispatch_context: info.context.as_deref(),
            card_title: info.card_title.as_deref(),
            github_issue_url: info.github_issue_url.as_deref(),
        }
    });
    let memory_recall_manifest = super::super::super::prompt_builder::MemoryRecallManifestInput {
        should_recall: memento_recall_gate.should_recall,
        gate_reason: memento_recall_gate.reason,
        external_recall: memory_recall.external_recall.as_deref(),
    };
    let recovery_context_for_manifest =
        session_retry_context
            .as_ref()
            .map(|context| RecoveryContextManifestInput {
                raw_context: context.raw_context.as_str(),
                audit_record: context.audit_record.as_ref(),
            });
    let built_system_prompt = build_system_prompt_with_manifest(
        &discord_context,
        &shared.channel_roster(channel_id, request_owner, request_owner_name),
        &current_path,
        channel_id,
        memory_scope_channel_id,
        token,
        role_binding.as_ref(),
        reply_to_user_message,
        PromptProfiles::foreground(dispatch_profile),
        dispatch_type_str.as_deref(),
        current_task_context.as_ref(),
        sak_for_system,
        memory_injection_plan.longterm_catalog_for_system_prompt,
        Some(&memory_settings),
        crate::services::mcp_config::provider_has_memento_mcp(&provider),
        matches!(&provider, ProviderKind::Claude),
        recovery_context_for_manifest.as_ref(),
        channel_recent_context.as_ref(),
        Some(&memory_recall_manifest),
        Some(&turn_id),
    );
    let system_prompt_owned = built_system_prompt.system_prompt;
    if let Some(manifest) = built_system_prompt.manifest {
        crate::db::prompt_manifests::spawn_save_prompt_manifest(shared.pg_pool.clone(), manifest);
    }
    if sak_for_system.is_some() {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 📦 SAK in system prompt ({} chars) for channel {}",
            sak_for_system.unwrap().len(),
            channel_id.get()
        );
    }
    let prompt_prep_duration_ms = prompt_prep_started.elapsed().as_millis();
    // #3813 Phase 1a: prompt prep complete — this mark sits INSIDE the
    // `[prompt-prep]` window below (overlaps it; do not sum — see latency_spans.rs).
    intake_latency.mark_prep_done();
    let provider_label = provider.as_str();
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] [prompt-prep] channel={} provider={} dispatch={} memory_backend={} reused_session={} duration_ms={}",
        channel_id.get(),
        provider_label,
        dispatch_profile_label(dispatch_profile),
        memory_settings.backend.as_str(),
        session_id.is_some(),
        prompt_prep_duration_ms
    );
    // #1085: track provider-session reuse rate so we can monitor whether the
    // idle-timeout extension and reset removals are actually translating into
    // reused sessions (vs. falling back to fresh sessions every turn).
    crate::services::observability::metrics::record_session_entry(
        channel_id.get(),
        provider_label,
        session_id.is_some(),
    );
    // Spawn turn watchdog — detects deadline expiry and hands off to cancel reconciliation.
    // The deadline is stored in cancel_token.watchdog_deadline_ms and can be
    // extended via POST /api/turns/{channel_id}/extend-timeout.
    turn_watchdog::spawn_text_turn_watchdog(
        &cancel_token,
        shared,
        http,
        channel_id,
        &provider,
        provider_label,
    );

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
    let prelaunch_runtime_kind = prelaunch_runtime_kind_for_managed_session(
        &provider,
        remote_profile.is_none(),
        tmux_session_name.is_some(),
        Some(channel_id.get()),
    );
    #[cfg(unix)]
    reconcile_managed_tmux_runtime_kind_for_config(
        &provider,
        channel_id,
        tmux_session_name.as_deref(),
        prelaunch_runtime_kind,
    );

    let model_for_turn =
        super::super::super::commands::resolve_model_for_turn(shared, channel_id, &provider).await;
    let adk_session_name = channel_name.clone();
    let adk_session_info = derive_adk_session_info(
        Some(user_text),
        channel_name.as_deref(),
        Some(&current_path),
    );
    let adk_thread_channel_id =
        adk_thread::resolve_channel_id(adk_session_name.as_deref(), shared, channel_id);
    // #222: DB-based dispatch lookup takes priority over text parsing.
    // In unified threads, user_text may contain a stale DISPATCH: prefix
    // from a previous dispatch in the same thread. DB lookup uses the
    // thread→card→dispatch link which is always current.
    let dispatch_id = super::super::super::adk_session::lookup_pending_dispatch_for_thread(
        shared.api_port,
        channel_id.get(),
    )
    .await
    .or_else(|| super::super::super::adk_session::parse_dispatch_id(user_text));
    post_adk_session_status(
        adk_session_key.as_deref(),
        adk_session_name.as_deref(),
        model_for_turn.as_deref(),
        "working",
        &provider,
        Some(&adk_session_info),
        None,
        Some(&current_path),
        dispatch_id.as_deref(),
        adk_thread_channel_id,
        Some(channel_id),
        role_binding
            .as_ref()
            .map(|binding| binding.role_id.as_str()),
        shared.api_port,
    )
    .await;

    let (inflight_tmux_name, inflight_output_path, inflight_input_fifo, mut inflight_offset) = {
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
    let watcher_tmux_name = inflight_tmux_name.clone();
    let watcher_output_path = inflight_output_path.clone();
    #[cfg(unix)]
    if let Some(result) =
        steering_hook::maybe_handle_intake_steering(steering_hook::IntakeSteeringContext {
            http,
            shared,
            token,
            channel_id,
            user_msg_id,
            placeholder_msg_id,
            provider: &provider,
            provider_label,
            tmux_session_name: tmux_session_name.as_deref(),
            current_path: &current_path,
            session_id: session_id.as_deref(),
            user_text,
            cancel_token: &cancel_token,
            intake_latency: &intake_latency,
            foreground: matches!(turn_kind, TurnKind::Foreground),
            local: remote_profile.is_none(),
            wait_for_completion,
            queued_drain,
            has_dispatch: dispatch_id.is_some() || dispatch_id_for_thread.is_some(),
            is_voice_announcement,
            has_pending_uploads: !pending_uploads.is_empty(),
        })
        .await
    {
        return result;
    }
    #[cfg(unix)]
    let mut recapture_offset_after_busy_wait = false;
    // #2416: compute claude_tui busy-followup diagnostic with a wait+retry step.
    // If the first snapshot says busy, run wait_for_prompt_ready (Followup kind,
    // ~45s default) via spawn_blocking. If the wait succeeds AND a fresh
    // diagnostic now says ready, fall through to normal dispatch instead of
    // dropping the user's message. Only emit the busy notice if the wait
    // times out / errors, or if the post-wait diagnostic is still busy.
    #[cfg(unix)]
    let tui_busy_diagnostic = {
        let initial = tui_busy_followup_diagnostic(
            shared,
            &provider,
            channel_id,
            tmux_session_name.as_deref(),
            remote_profile.is_some(),
            Some(&current_path),
            session_id.as_deref(),
        );
        if let Some(initial_diagnostic) = initial {
            // #3208 (A): when the authoritative JSONL turn-state says the prior
            // turn is genuinely in-flight (Streaming/UserSubmitted), do NOT
            // enter the up-to-45s readiness poll — it would always time out
            // (the prompt marker is suppressed for the whole agentic turn) and
            // surface a misleading error after the response already landed.
            // Route straight to the queue-defer path: the busy diagnostic is
            // already correct, and the watcher idle signal delivers the queued
            // input cleanly once the turn reaches Idle.
            if initial_diagnostic.transcript_turn_state.is_busy() {
                tracing::info!(
                    channel_id = channel_id.get(),
                    user_msg_id = user_msg_id.get(),
                    tmux_session_name = %initial_diagnostic.tmux_session_name,
                    transcript_turn_state = initial_diagnostic.transcript_turn_state.as_str(),
                    "tui follow-up: prior turn genuinely busy per JSONL; deferring to queue without readiness poll"
                );
                Some(initial_diagnostic)
            } else {
                let wait_session_name = initial_diagnostic.tmux_session_name.clone();
                let wait_cancel_token = cancel_token.clone();
                let wait_provider = provider.clone();
                let wait_readiness = hosted_tui_busy_preflight_readiness_wait(
                    &wait_provider,
                    Some(&current_path),
                    session_id.as_deref(),
                    tmux_session_name.as_deref(),
                );
                match &wait_readiness {
                    HostedTuiBusyPreflightReadinessWait::Codex => {
                        tracing::debug!(
                            channel_id = channel_id.get(),
                            user_msg_id = user_msg_id.get(),
                            tmux_session_name = %wait_session_name,
                            "hosted tui busy preflight will wait for codex composer readiness"
                        );
                    }
                    HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOrIdleTranscript(
                        transcript_path,
                    ) => {
                        tracing::debug!(
                            channel_id = channel_id.get(),
                            user_msg_id = user_msg_id.get(),
                            tmux_session_name = %wait_session_name,
                            transcript_path = %transcript_path.display(),
                            "hosted tui busy preflight will allow claude idle transcript readiness"
                        );
                    }
                    HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOnly => {
                        tracing::debug!(
                            channel_id = channel_id.get(),
                            user_msg_id = user_msg_id.get(),
                            tmux_session_name = %wait_session_name,
                            "hosted tui busy preflight will require claude prompt marker readiness"
                        );
                    }
                }
                let wait_result =
                    tokio::task::spawn_blocking(move || {
                        match wait_readiness {
                HostedTuiBusyPreflightReadinessWait::Codex => {
                    crate::services::codex_tui::input::wait_until_codex_tui_input_ready(
                        &wait_session_name,
                        crate::services::codex_tui::input::PromptReadinessKind::Followup,
                        Some(&wait_cancel_token),
                    )
                }
                HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOrIdleTranscript(
                    transcript_path,
                ) => crate::services::claude_tui::input::wait_for_prompt_ready_or_idle_transcript(
                    &wait_session_name,
                    crate::services::claude_tui::input::PromptReadinessKind::Followup,
                    Some(wait_cancel_token.as_ref()),
                    &transcript_path,
                ),
                HostedTuiBusyPreflightReadinessWait::ClaudePromptMarkerOnly => {
                    crate::services::claude_tui::input::wait_for_prompt_ready(
                        &wait_session_name,
                        crate::services::claude_tui::input::PromptReadinessKind::Followup,
                        Some(wait_cancel_token.as_ref()),
                    )
                }
            }
                    })
                    .await
                    .unwrap_or_else(|join_err| {
                        Err(format!("wait_for_prompt_ready join error: {join_err}"))
                    });
                let post_wait_diagnostic = tui_busy_followup_diagnostic(
                    shared,
                    &provider,
                    channel_id,
                    tmux_session_name.as_deref(),
                    remote_profile.is_some(),
                    Some(&current_path),
                    session_id.as_deref(),
                );
                // #2416: cancellation may have flipped during the up-to-45s wait
                // (user stop reaction, watchdog, etc.). If it did, do NOT continue
                // to inject the prompt — fall into the busy-notice / cleanup branch
                // below by surfacing the initial diagnostic. Closes a Codex-flagged
                // HIGH on the Discord path mirroring the same fix in claude.rs.
                match (
                    wait_result,
                    post_wait_diagnostic,
                    cancel_token
                        .cancelled
                        .load(std::sync::atomic::Ordering::Relaxed),
                ) {
                    (_, _, true) => {
                        tracing::warn!(
                            channel_id = channel_id.get(),
                            user_msg_id = user_msg_id.get(),
                            tmux_session_name = %initial_diagnostic.tmux_session_name,
                            "claude_tui follow-up: cancellation observed after busy wait; aborting injection"
                        );
                        Some(initial_diagnostic)
                    }
                    (Ok(()), None, _) => {
                        recapture_offset_after_busy_wait = true;
                        tracing::info!(
                            channel_id = channel_id.get(),
                            user_msg_id = user_msg_id.get(),
                            tmux_session_name = %initial_diagnostic.tmux_session_name,
                            "claude_tui follow-up: busy at first check, became ready after wait_for_prompt_ready"
                        );
                        None
                    }
                    (Ok(()), Some(diag), _) => {
                        tracing::warn!(
                            channel_id = channel_id.get(),
                            user_msg_id = user_msg_id.get(),
                            "claude_tui follow-up: wait_for_prompt_ready returned Ok but post-wait diagnostic still busy"
                        );
                        Some(diag)
                    }
                    (Err(err), diag_opt, _) => {
                        let timed_out = match &provider {
                            ProviderKind::Codex => {
                                crate::services::codex_tui::input::is_prompt_ready_timeout_error(
                                    &err,
                                )
                            }
                            _ => crate::services::claude_tui::input::is_prompt_ready_timeout_error(
                                &err,
                            ),
                        };
                        tracing::warn!(
                            channel_id = channel_id.get(),
                            user_msg_id = user_msg_id.get(),
                            timed_out,
                            error = %err,
                            "claude_tui follow-up: wait_for_prompt_ready failed; emitting busy notice"
                        );
                        Some(diag_opt.unwrap_or(initial_diagnostic))
                    }
                }
            }
        } else {
            None
        }
    };
    #[cfg(unix)]
    if let Some(diagnostic) = tui_busy_diagnostic {
        let bot_owner_provider = super::super::super::resolve_discord_bot_provider(token);
        let queue_kickoff_scheduled_by_release = release_mailbox_after_hosted_tui_busy_pre_submit(
            shared,
            &bot_owner_provider,
            channel_id,
        )
        .await;
        let enqueue_outcome = enqueue_busy_tui_followup_for_retry(
            shared,
            &bot_owner_provider,
            channel_id,
            original_request_owner,
            user_msg_id,
            user_text,
            preserve_on_cancel,
            reply_context.clone(),
            has_reply_boundary,
            merge_consecutive,
            pending_uploads.clone(),
            voice_announcement.clone(),
        )
        .await;
        let queue_depth_after_busy_enqueue =
            super::super::super::mailbox_snapshot(shared, channel_id)
                .await
                .intervention_queue
                .len();
        let retry_present_or_accepted = busy_retry::finalize_enqueue(
            busy_retry::FinalizeEnqueueContext {
                shared,
                http,
                provider: &provider,
                channel_id,
                user_msg_id,
                placeholder_msg_id,
                turn_start_attempt,
                session_retry_context: session_retry_context.as_ref(),
                feedback_reminder: feedback_reminder.as_deref(),
                wip_warning: wip_warning.as_deref(),
            },
            &enqueue_outcome,
        )
        .await;
        let queued_card_rendered = false;
        let queue_kickoff_scheduled =
            queue_kickoff_scheduled_by_release || retry_present_or_accepted;
        let mut diagnostic_json = diagnostic.to_json();
        if let Some(object) = diagnostic_json.as_object_mut() {
            object.insert(
                "queued_for_retry".to_string(),
                serde_json::json!(enqueue_outcome.enqueued),
            );
            object.insert(
                "queue_merged".to_string(),
                serde_json::json!(enqueue_outcome.merged),
            );
            object.insert(
                "queue_depth_after".to_string(),
                serde_json::json!(queue_depth_after_busy_enqueue),
            );
            object.insert(
                "queued_card_rendered".to_string(),
                serde_json::json!(queued_card_rendered),
            );
            object.insert(
                "queue_kickoff_scheduled".to_string(),
                serde_json::json!(queue_kickoff_scheduled),
            );
            // #2728: when `enqueued == false` we previously had no signal in
            // the producer-exit diagnostic to distinguish dup-guard / dedup /
            // actor-unreachable refusals. Surface the refusal kind so the
            // next adk-cc-style incident can be classified from the log.
            if let Some(reason) = enqueue_outcome.refusal_reason {
                object.insert(
                    "enqueue_refusal_reason".to_string(),
                    serde_json::json!(reason.as_str()),
                );
            }
        }
        tracing::warn!(
            channel_id = channel_id.get(),
            user_msg_id = user_msg_id.get(),
            diagnostics = %diagnostic_json,
            "claude_tui follow-up queued because hosted TUI is busy before prompt submission"
        );
        crate::services::observability::emit_inflight_lifecycle_event(
            provider.as_str(),
            channel_id.get(),
            dispatch_id.as_deref(),
            adk_session_key.as_deref(),
            Some(turn_id.as_str()),
            "claude_tui_followup_busy_pre_submit",
            diagnostic_json,
        );
        super::super::super::saturating_decrement_global_active(shared);
        shared.turn_start_times.remove(&channel_id);
        post_adk_session_status(
            adk_session_key.as_deref(),
            adk_session_name.as_deref(),
            Some(provider.as_str()),
            "awaiting_user",
            &provider,
            Some(&adk_session_info),
            None,
            Some(&current_path),
            dispatch_id.as_deref(),
            adk_thread_channel_id,
            Some(channel_id),
            role_binding
                .as_ref()
                .map(|binding| binding.role_id.as_str()),
            shared.api_port,
        )
        .await;
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] 📬 Claude TUI busy follow-up queued before prompt submission (channel {}, enqueued={}, merged={}, depth={}, card_rendered={}, queue_kickoff_scheduled={})",
            channel_id,
            enqueue_outcome.enqueued,
            enqueue_outcome.merged,
            queue_depth_after_busy_enqueue,
            queued_card_rendered,
            queue_kickoff_scheduled
        );
        cancel_token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        super::super::super::clear_watchdog_deadline_override(channel_id.get()).await;
        // #3813 Phase 1a: prep done but input deferred pre-submit (TUI busy) —
        // emit the partial span (input/total render `-`); the retry re-enters
        // intake and emits its own `submitted` span.
        intake_latency.log(channel_id.get(), provider_label, "deferred_busy");
        return Ok(());
    }
    #[cfg(unix)]
    if recapture_offset_after_busy_wait {
        let corrected_offset = recapture_inflight_offset_after_successful_busy_wait(
            inflight_output_path.as_deref(),
            inflight_offset,
        );
        if corrected_offset != inflight_offset {
            tracing::info!(
                channel_id = channel_id.get(),
                user_msg_id = user_msg_id.get(),
                previous_offset = inflight_offset,
                corrected_offset,
                "claude_tui follow-up recaptured inflight offset after successful busy wait"
            );
        }
        inflight_offset = corrected_offset;
    }

    let (logical_channel_id, thread_id, thread_title) =
        if let Some((parent_id, _parent_name)) = final_thread_parent {
            let (live_thread_title, _) =
                super::super::super::resolve_channel_category(http, cache, channel_id).await;
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
    inflight_state.turn_nonce = cancel_token.turn_nonce().map(str::to_owned);
    apply_prelaunch_runtime_kind(&mut inflight_state, prelaunch_runtime_kind);
    let (worktree_path, worktree_branch, base_commit) = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.worktree.as_ref())
            .map(|wt| {
                (
                    Some(wt.worktree_path.clone()),
                    Some(wt.branch_name.clone()),
                    crate::services::platform::git_head_commit(&wt.original_path),
                )
            })
            .unwrap_or((None, None, None))
    };
    inflight_state.set_worktree_context(worktree_path, worktree_branch, base_commit);
    // FIX #6 (Codex P2): persist the originating Intervention's follow-up
    // requeue context so a PRE-submit busy-timeout requeue
    // (`mailbox_requeue_inflight_for_followup_retry`) can rebuild the retry
    // Intervention without losing reply context / attachments / voice metadata.
    inflight_state.set_followup_requeue_context(
        reply_context.clone(),
        has_reply_boundary,
        merge_consecutive,
        pending_uploads.clone(),
        voice_announcement.clone(),
        preserve_on_cancel,
    );
    inflight_state.logical_channel_id = Some(logical_channel_id);
    inflight_state.thread_id = thread_id;
    inflight_state.thread_title = thread_title;
    if is_voice_announcement {
        inflight_state.source = crate::dispatch::Source::Voice;
    }
    inflight_state.session_key = adk_session_key.clone();
    inflight_state.dispatch_id = dispatch_id.clone();
    inflight_create_log::record_turn_start_origin(&provider, channel_id, &inflight_state).await;
    inflight_create_log::log_create_new_inflight_outcome(
        super::super::super::inflight::save_inflight_state_create_new(&inflight_state),
        &provider,
        &inflight_state,
    );

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

    // Pause the tmux-session owner watcher before writing to the provider
    // FIFO. In thread follow-ups, the watcher may be owned by the parent
    // channel rather than the requested thread channel.
    let _ = attach_paused_turn_watcher_for_inflight(
        shared,
        http.clone(),
        &provider,
        channel_id,
        watcher_tmux_name,
        watcher_output_path,
        inflight_offset,
        "turn_start_message",
        &mut inflight_state,
    );

    // Auto-sync worktree before sending message to session
    {
        let script = super::super::super::runtime_store::agentdesk_root()
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

    let native_fast_mode_override = native_fast_mode_override_for_turn(
        &provider,
        super::super::super::commands::channel_fast_mode_setting(shared, fast_mode_channel_id)
            .await,
    );
    let codex_goals_override = codex_goals_override_for_turn(
        &provider,
        super::super::super::commands::channel_codex_goals_setting(shared, fast_mode_channel_id)
            .await,
    );

    // Fetch context compact percent from ADK settings (provider-specific)
    let ctx_thresholds =
        super::super::super::adk_session::fetch_context_thresholds(shared.api_port).await;
    let compact_percent = ctx_thresholds.compact_pct_for(&provider);
    // Use model-specific context window (reads Codex models cache), falling
    // back to the provider default if the model isn't found.
    let model_context_window = provider.resolve_context_window(model_for_turn.as_deref());

    // Pre-compute provider-specific compact config
    let compact_percent_for_claude = Some(ctx_thresholds.compact_pct_for(&provider));
    let compact_lower_bound_tokens = ctx_thresholds.compact_lower_bound_tokens;
    let compact_token_limit_for_codex = {
        provider
            .compact_cli_config(compact_percent, model_context_window)
            .first()
            .map(|(_, v)| v.parse::<u64>().unwrap_or(0))
    };
    // #1088: per-channel prompt-cache TTL (None|5|60). Only consumed by Claude.
    let cache_ttl_minutes =
        super::super::super::settings::resolve_cache_ttl_minutes(channel_id, None);
    let provider_execution_context = crate::services::provider_cli::ProviderExecutionContext {
        provider: provider.as_str().to_string(),
        agent_id: role_binding.as_ref().map(|binding| binding.role_id.clone()),
        channel_id: Some(channel_id.get().to_string()),
        session_key: adk_session_key.clone(),
        tmux_session: tmux_session_name.clone(),
        channel_name: channel_name.clone(),
        execution_mode: Some("discord_turn".to_string()),
    };
    let dispatch_type_for_mcp = dispatch_type_str.clone();

    // Run the provider in a blocking thread
    if is_voice_announcement {
        crate::voice::metrics::mark_agent_start(channel_id.get());
    }
    let provider_for_blocking = provider.clone();
    tokio::task::spawn_blocking(move || {
        let result = crate::services::platform::with_provider_execution_context(
            provider_execution_context,
            || {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let system_prompt_for_turn =
                        crate::services::provider::system_prompt_for_provider_turn(
                            &provider_for_blocking,
                            session_id_clone.as_deref(),
                            &system_prompt_owned,
                        );
                    match &provider_for_blocking {
                        ProviderKind::Claude => claude::execute_command_streaming(
                            &context_prompt,
                            session_id_clone.as_deref(),
                            &current_path_clone,
                            tx.clone(),
                            system_prompt_for_turn,
                            Some(&allowed_tools),
                            Some(cancel_token_clone),
                            remote_profile.as_ref(),
                            tmux_session_name.as_deref(),
                            Some(channel_id.get()),
                            Some(provider_for_blocking.clone()),
                            model_for_turn.as_deref(),
                            native_fast_mode_override,
                            compact_percent_for_claude,
                            compact_lower_bound_tokens,
                            cache_ttl_minutes,
                            dispatch_type_for_mcp.as_deref(),
                        ),
                        ProviderKind::Codex => codex::execute_command_streaming(
                            &context_prompt,
                            session_id_clone.as_deref(),
                            &current_path_clone,
                            tx.clone(),
                            system_prompt_for_turn,
                            Some(&allowed_tools),
                            Some(cancel_token_clone),
                            remote_profile.as_ref(),
                            tmux_session_name.as_deref(),
                            Some(channel_id.get()),
                            Some(provider_for_blocking.clone()),
                            model_for_turn.as_deref(),
                            native_fast_mode_override,
                            codex_goals_override,
                            compact_token_limit_for_codex,
                            force_fresh_provider_session,
                        ),
                        ProviderKind::Gemini => gemini::execute_command_streaming(
                            &context_prompt,
                            session_id_clone.as_deref(),
                            &current_path_clone,
                            tx.clone(),
                            system_prompt_for_turn,
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
                            system_prompt_for_turn,
                            Some(&allowed_tools),
                            Some(cancel_token_clone),
                            remote_profile.as_ref(),
                            tmux_session_name.as_deref(),
                            Some(channel_id.get()),
                            Some(provider_for_blocking.clone()),
                            model_for_turn.as_deref(),
                            None, // Qwen: compact not supported
                        ),
                        ProviderKind::OpenCode => opencode::execute_command_streaming(
                            &context_prompt,
                            session_id_clone.as_deref(),
                            &current_path_clone,
                            tx.clone(),
                            system_prompt_for_turn,
                            Some(&allowed_tools),
                            Some(cancel_token_clone),
                            remote_profile.as_ref(),
                            tmux_session_name.as_deref(),
                            Some(channel_id.get()),
                            Some(provider_for_blocking.clone()),
                            model_for_turn.as_deref(),
                            None,
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
                    }
                }))
            },
        );

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

    // #3813 Phase 1a: provider input is about to be handed to the turn bridge.
    intake_latency.mark_input_written();
    super::typing_indicator::spawn_native_typing_indicator(
        shared,
        http.clone(),
        channel_id,
        inflight_state.effective_finalizer_turn_id(),
    );
    spawn_turn_bridge(
        shared.clone(),
        cancel_token.clone(),
        rx,
        TurnBridgeContext {
            provider: provider.clone(),
            gateway: Arc::new(DiscordGateway::new(
                http.clone(),
                shared.clone(),
                provider.clone(),
                ctx_for_chained_dispatch.map(|live_ctx| LiveDiscordTurnContext {
                    ctx: live_ctx.clone(),
                    token: token.to_string(),
                    request_owner,
                }),
            )),
            channel_id,
            user_msg_id: Some(user_msg_id),
            user_text_owned: user_text.to_string(),
            request_owner_name: request_owner_name.to_string(),
            role_binding: role_binding.clone(),
            adk_session_key,
            adk_session_name,
            adk_session_info: Some(adk_session_info),
            adk_cwd: Some(current_path.clone()),
            dispatch_id,
            dispatch_kind: super::super::super::turn_bridge::classify_turn_finished_dispatch_kind(
                active_dispatch_info
                    .as_ref()
                    .and_then(|info| info.context.as_deref()),
                dispatch_type_str.as_deref(),
            )
            .map(str::to_string),
            memory_recall_usage: memory_recall.token_usage,
            context_window_tokens: model_context_window,
            context_compact_percent: compact_percent,
            current_msg_id: Some(placeholder_msg_id),
            response_sent_offset: 0,
            full_response: String::new(),
            tmux_last_offset: Some(inflight_offset),
            new_session_id: session_id.clone(),
            defer_watcher_resume,
            reuse_status_panel_message: false,
            completion_tx,
            is_external_input_tui_direct: false, // #3089 A6b: Discord-origin intake turn
            inflight_state,
        },
    );

    // #3813 Phase 1a: full intake span complete — emit the structured line + event.
    intake_latency.log(channel_id.get(), provider_label, "submitted");

    if let Some(rx) = completion_rx {
        rx.await
            .map_err(|_| "queued turn completion wait failed".to_string())?;
    }

    Ok(())
}

#[cfg(test)]
mod tui_busy_pre_submit_queue_reaction_tests {
    #[test]
    fn busy_pre_submit_enqueue_keeps_the_authoritative_queue_view() {
        let module_src = include_str!("intake_turn.rs");
        let busy_branch_pos = module_src
            .find("claude_tui follow-up queued because hosted TUI is busy before prompt submission")
            .expect("hosted-TUI busy pre-submit queue branch exists");
        let busy_branch = &module_src[..busy_branch_pos];
        assert!(
            busy_branch.contains("busy_retry::finalize_enqueue("),
            "busy pre-submit branch must route enqueue finalization through the shared helper"
        );

        let helper_src = include_str!("busy_retry.rs");
        let accepted_guard_pos = helper_src
            .find("if outcome.enqueued {")
            .expect("busy pre-submit reaction must be gated on accepted enqueue");
        let accepted_helper = "note_busy_tui_pre_submit_queue_pending(";
        let refusal_guard =
            "} else {\n        super::tui_followup::apply_tui_busy_enqueue_refusal(";
        let accepted_clear = "note_intake_turn_cleared_current(";
        let refusal_guard_pos = helper_src[accepted_guard_pos..]
            .find(refusal_guard)
            .map(|offset| accepted_guard_pos + offset)
            .expect("busy pre-submit enqueue refusal branch exists");
        let reaction_call_pos = helper_src[accepted_guard_pos..refusal_guard_pos]
            .find(accepted_helper)
            .map(|offset| accepted_guard_pos + offset)
            .expect("accepted busy pre-submit enqueue must apply a queue-pending reaction");
        let accepted_branch = &helper_src[accepted_guard_pos..refusal_guard_pos];
        let refusal_branch = &helper_src[refusal_guard_pos..];

        assert!(
            accepted_guard_pos < reaction_call_pos,
            "an accepted hosted-TUI busy pre-submit enqueue must reach the shared queue-pending reaction helper"
        );
        assert!(
            !accepted_branch.contains(accepted_clear),
            "accepted busy requeue must preserve the reconciler-owned queued marker"
        );
        assert!(
            refusal_branch.contains(accepted_clear),
            "refused busy enqueue must still clear the optimistic pending view"
        );
    }
}

#[cfg(test)]
mod recovery_context_take_order_tests {
    fn recovery_context_take_call() -> String {
        format!(
            "{}{}",
            "let session_retry_context = ",
            "take_session_retry_context(shared, channel_id, Some(&turn_id));"
        )
    }

    #[test]
    fn discord_user_turn_keeps_user_and_streaming_placeholder_ids_separate() {
        let module_src = include_str!("intake_turn.rs");
        let bridge_context_pos = module_src
            .find("TurnBridgeContext {")
            .expect("Discord intake builds a turn-bridge context");
        let bridge_context = &module_src[bridge_context_pos..];
        let user_message_field = format!("{}{}", "user_msg_id: Some(", "user_msg_id),");
        let placeholder_field = format!("{}{}", "current_msg_id: Some(", "placeholder_msg_id),");
        let synthetic_flag = format!("{}{}", "is_external_input_tui_", "direct: false");

        assert!(
            bridge_context.contains(&user_message_field),
            "Discord-origin user turns must retain the real user message as their request identity"
        );
        assert!(
            bridge_context.contains(&placeholder_field),
            "Discord-origin user turns must keep the posted intake placeholder as their streaming edit target"
        );
        assert!(
            bridge_context.contains(&synthetic_flag),
            "Discord-origin user turns must remain outside the synthetic TUI-direct path"
        );
    }

    #[test]
    fn recovery_context_survives_intake_stale_dispatch_abort() {
        let root = tempfile::tempdir().expect("create temp runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let module_src = include_str!("intake_turn.rs");
        // #4552: the stale-dispatch guard body now lives in the
        // `stale_dispatch_guard` submodule; the caller keeps the guarded early
        // return, so anchor on the call site (still ahead of the take below).
        let stale_guard_pos = module_src
            .find("abort_terminal_dispatch_at_turn_start(")
            .expect("intake stale-dispatch guard call exists");
        let stale_return_pos = stale_guard_pos
            + module_src[stale_guard_pos..]
                .find("return Ok(());")
                .expect("intake stale-dispatch abort return exists");
        let take_call = recovery_context_take_call();
        let take_pos = module_src
            .find(&take_call)
            .expect("intake recovery context take exists");

        assert!(
            stale_return_pos < take_pos,
            "intake stale-dispatch abort must happen before the destructive recovery-context take"
        );
    }

    #[test]
    fn intake_real_turn_consumes_recovery_context_once_after_non_dispatch_guards() {
        let root = tempfile::tempdir().expect("create temp runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let module_src = include_str!("intake_turn.rs");
        let take_call = recovery_context_take_call();
        let take_positions: Vec<_> = module_src.match_indices(&take_call).collect();
        assert_eq!(
            take_positions.len(),
            1,
            "intake turn start must have exactly one destructive recovery-context take"
        );
        let take_pos = take_positions[0].0;
        let race_loss_return_pos = module_src
            .find("return race_loss::handle_race_loss_enqueue(")
            .expect("intake race-loss enqueue return exists");
        let placeholder_posted_pos = module_src
            .find("intake_latency.mark_placeholder_posted();")
            .expect("intake placeholder-post success mark exists");
        let prompt_use_pos = module_src
            .find("if let Some(ref reply_ctx) = reply_context")
            .expect("intake prompt includes reply context");
        let manifest_use_pos = module_src
            .find("let recovery_context_for_manifest =")
            .expect("intake prompt manifest receives recovery context");

        assert!(
            race_loss_return_pos < take_pos,
            "queued/race-loss intake turns must not destructively take recovery context before returning"
        );
        assert!(
            take_pos < placeholder_posted_pos,
            "active intake turn must take recovery context immediately after placeholder success"
        );
        assert!(
            take_pos < prompt_use_pos,
            "active intake turn must take recovery context before adding it to the prompt"
        );
        assert!(
            take_pos < manifest_use_pos,
            "active intake turn must take recovery context before prompt manifest capture"
        );
    }

    #[test]
    fn tui_busy_enqueue_refusal_puts_back_recovery_context_for_next_turn() {
        // The refusal else-branch must route through the sibling helper, which
        // puts the taken recovery context back BEFORE rewriting the refusal
        // notice (put-back-then-notice ordering pinned in tui_followup.rs).
        let finalize_src = include_str!("busy_retry.rs");
        finalize_src
            .find("} else {\n        super::tui_followup::apply_tui_busy_enqueue_refusal(")
            .expect("TUI-busy enqueue refusal routes through the put-back helper");

        let helper_src = include_str!("tui_followup.rs");
        let helper_fn_pos = helper_src
            .find("async fn apply_tui_busy_enqueue_refusal(")
            .expect("refusal helper exists in tui_followup.rs");
        let helper_body = &helper_src[helper_fn_pos..];
        let put_back_pos = helper_body
            .find("put_back_session_retry_context(")
            .expect("refusal helper restores recovery context");
        let notice_pos = helper_body
            .find("claude_tui_busy_followup_refusal_notice(")
            .expect("refusal helper renders the notice");
        assert!(
            put_back_pos < notice_pos,
            "TUI-busy enqueue refusal, including dup-guard refusal, must restore recovery context before returning the notice"
        );
    }
}

/// #4307 PR-B: pins the reminder take/inject/put-back wiring with the same
/// source-order invariants as the session-retry recovery context it rides.
#[cfg(test)]
mod feedback_reminder_take_order_tests {
    // Assembled from fragments so this test's own source (scanned via
    // `include_str!`) does not match the literal it is looking for — mirroring
    // `recovery_context_take_call` above.
    fn reminder_take_call() -> String {
        format!(
            "{}{}",
            "take_and_merge_feedback_reminder(", "shared, &provider, channel_id, reply_context);"
        )
    }

    #[test]
    fn intake_takes_feedback_reminder_once_after_guards_and_before_prompt_use() {
        let root = tempfile::tempdir().expect("create temp runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let module_src = include_str!("intake_turn.rs");

        let take_call = reminder_take_call();
        let take_positions: Vec<_> = module_src.match_indices(&take_call).collect();
        assert_eq!(
            take_positions.len(),
            1,
            "intake turn start must have exactly one destructive feedback-reminder take"
        );
        let take_pos = take_positions[0].0;

        let race_loss_return_pos = module_src
            .find("return race_loss::handle_race_loss_enqueue(")
            .expect("intake race-loss enqueue return exists");
        let placeholder_posted_pos = module_src
            .find("intake_latency.mark_placeholder_posted();")
            .expect("intake placeholder-post success mark exists");
        let prompt_use_pos = module_src
            .find("if let Some(ref reply_ctx) = reply_context")
            .expect("intake prompt includes reply context");

        assert!(
            race_loss_return_pos < take_pos,
            "queued/race-loss intake turns must not destructively take the reminder before returning"
        );
        assert!(
            take_pos < placeholder_posted_pos,
            "active intake turn must take the reminder immediately after placeholder success"
        );
        assert!(
            take_pos < prompt_use_pos,
            "active intake turn must take the reminder before injecting it into the prompt"
        );
    }

    #[test]
    fn tui_busy_enqueue_refusal_puts_back_feedback_reminder_for_next_turn() {
        // The refusal branch forwards the taken reminder to the sibling helper,
        // which puts it back BEFORE rewriting the refusal notice (mirroring the
        // recovery-context put-back-then-notice ordering).
        let finalize_src = include_str!("busy_retry.rs");
        // The `\n` escape resolves to a real newline, which only the production
        // call site contains — this test's own copy of the snippet stores it as
        // the two characters `\n`, so the search does not self-match (same trick
        // the recovery-context refusal test relies on).
        assert!(
            finalize_src.contains("session_retry_context,\n            feedback_reminder,"),
            "refusal call site must forward the taken reminder for put-back, right after the recovery context"
        );

        let helper_src = include_str!("tui_followup.rs");
        let helper_fn_pos = helper_src
            .find("async fn apply_tui_busy_enqueue_refusal(")
            .expect("refusal helper exists in tui_followup.rs");
        let helper_body = &helper_src[helper_fn_pos..];
        let put_back_pos = helper_body
            .find("put_back_voluntary_feedback_reminder(")
            .expect("refusal helper restores the feedback reminder");
        let notice_pos = helper_body
            .find("claude_tui_busy_followup_refusal_notice(")
            .expect("refusal helper renders the notice");
        assert!(
            put_back_pos < notice_pos,
            "TUI-busy enqueue refusal must restore the feedback reminder before returning the notice"
        );
    }
}

#[cfg(test)]
mod queue_pending_reaction_clear_tests {
    use super::*;

    #[test]
    fn clears_every_queue_marker_reaction() {
        let emojis = crate::services::discord::queue_reactions::QUEUE_PENDING_REACTION_EMOJIS;
        assert!(
            emojis.contains(&'📬'),
            "standalone queue-head 📬 must be cleared on dequeue"
        );
        assert!(
            emojis.contains(&'➕'),
            "merged queue ➕ must be cleared on dequeue"
        );
        assert!(
            emojis.contains(&'🔄'),
            "reconcile queue 🔄 must be cleared on dequeue"
        );
        assert_eq!(
            emojis.len(),
            crate::services::discord::queue_reactions::QUEUE_PENDING_REACTION_EMOJIS.len(),
            "exactly the shared queue-pending emojis are cleared"
        );
    }

    /// The cleared set must match exactly what the intake gate ADDS via
    /// `queue_pending_reaction_for`, so no queued message can be reacted with an
    /// emoji the dequeue path will not later remove.
    #[test]
    fn cleared_set_covers_every_intake_gate_queue_reaction() {
        let cleared = crate::services::discord::queue_reactions::QUEUE_PENDING_REACTION_EMOJIS;
        for merged in [false, true] {
            let added = crate::services::discord::router::intake_gate::queue_pending_reaction_for(
                crate::services::discord::MailboxEnqueueOutcome {
                    enqueued: true,
                    merged,
                    ..Default::default()
                },
            );
            assert!(
                cleared.contains(&added),
                "intake-gate add emoji {added:?} (merged={merged}) must be in the dequeue clear set"
            );
        }
    }
}

#[cfg(test)]
mod turn_start_dispatch_guard_preservation_tests {
    // #4247 FIX 1: the turn-start DISPATCH-GUARD must gate its raw
    // `stale_dispatch_turn_for_text` lookup on `!preserve_on_cancel`, mirroring
    // the dequeue guard's `filter_queued_dispatch_exit(preserve, stale)` — else a
    // preserved (marked) genuine human instruction that survives the dequeue
    // guard is dropped anyway on re-entry just because its text carries a stale
    // `DISPATCH:<id>` prefix, silently defeating the feature end-to-end.
    #[test]
    fn turn_start_guard_is_gated_on_preserve_on_cancel() {
        // #4552: the guard body moved to the `stale_dispatch_guard` submodule.
        // The invariant (gate the raw stale-text lookup on `!preserve_on_cancel`)
        // now lives entirely inside that helper, so scan it directly.
        let guard_src = include_str!("intake_turn/stale_dispatch_guard.rs");
        let stale_call_pos = guard_src
            .find("stale_dispatch_turn_for_text(")
            .expect("turn-start dispatch-guard raw stale-text lookup exists");
        let gate_before_lookup = guard_src[..stale_call_pos].find("!preserve_on_cancel");

        assert!(
            gate_before_lookup.is_some(),
            "turn-start DISPATCH-GUARD must gate the raw stale-text lookup on \
             `!preserve_on_cancel` (#4247 FIX 1) — removing this gate lets a \
             preserved (marked) genuine human instruction get dropped at turn \
             start just because it carries a stale DISPATCH: prefix, silently \
             defeating the fail-safe queue-preservation feature end-to-end"
        );
    }

    // Companion: the gated stale lookup must still feed the SAME abort branch
    // (finish turn, advance checkpoint, exit emoji) and signal abort back to the
    // caller, i.e. the gate did not get attached to some other unrelated
    // `stale_dispatch_turn_for_text` use.
    #[test]
    fn gated_stale_lookup_feeds_the_turn_abort_branch() {
        // #4552: the abort branch body lives in the `stale_dispatch_guard`
        // helper; the caller keeps the guarded early return.
        let guard_src = include_str!("intake_turn/stale_dispatch_guard.rs");
        let stale_call_pos = guard_src
            .find("stale_dispatch_turn_for_text(")
            .expect("turn-start dispatch-guard raw stale-text lookup exists");
        // Anchor on the abort log, then look for the branch's finish + abort
        // signal by RELATIVE offset — no fixed byte window (multibyte-safe, and
        // resilient to added log lines).
        let abort_pos = guard_src[stale_call_pos..]
            .find("DISPATCH-GUARD: aborted terminal dispatch at turn start")
            .map(|offset| stale_call_pos + offset)
            .expect("gated stale lookup must still feed the turn-start abort log/branch");
        let abort_region = &guard_src[abort_pos..];
        let finish = abort_region.find("mailbox_finish_turn");
        let ret = abort_region.find("return true;");
        assert!(
            finish.is_some_and(|f| f < 600),
            "turn-start abort branch must still finish the mailbox turn"
        );
        assert!(
            ret.is_some_and(|r| r < 1200),
            "turn-start abort branch must still signal abort to the caller"
        );

        // The caller must honor that abort signal with an early return.
        let module_src = include_str!("intake_turn.rs");
        let call_pos = module_src
            .find("abort_terminal_dispatch_at_turn_start(")
            .expect("live intake calls the turn-start guard");
        assert!(
            module_src[call_pos..]
                .find("return Ok(());")
                .is_some_and(|r| r < 400),
            "the live-intake caller must return early when the guard aborts"
        );
    }

    // #4247 FIX 1/FIX 4: pin the LIVE (non-queued) intake path threading
    // `preserve_on_cancel` from the function signature into the turn-start guard
    // unmodified — distinct from the queued path's `into_intervention`
    // computation (already pinned by `intake_queue_transaction.rs`).
    #[test]
    fn preserve_on_cancel_parameter_reaches_the_turn_start_guard_unmodified() {
        let module_src = include_str!("intake_turn.rs");
        // Needle assembled via concat! so the exact fn-call token never appears
        // verbatim here (would trip `intake_dispatch::tests`'s occurrence-count
        // invariant on `intake_turn.rs`).
        let signature_pos = module_src
            .find(concat!("pub(super) async fn handle_text_message", "("))
            .expect("handle_text_message signature exists");
        let param_pos = module_src[signature_pos..]
            .find("preserve_on_cancel: bool,")
            .map(|offset| signature_pos + offset)
            .expect("handle_text_message receives preserve_on_cancel as a parameter");
        // #4552: the guard body moved to the `stale_dispatch_guard` submodule.
        // The parameter must still be forwarded into the guard call unmodified,
        // and the extracted guard must still gate on it.
        let call_pos = module_src[param_pos..]
            .find("abort_terminal_dispatch_at_turn_start(")
            .map(|offset| param_pos + offset)
            .expect("the live-intake path must call the turn-start guard");
        let call_end = module_src[call_pos..]
            .find(".await")
            .map(|offset| call_pos + offset)
            .expect("the turn-start guard call must be awaited");
        assert!(
            module_src[call_pos..call_end].contains("preserve_on_cancel"),
            "the live-intake preserve_on_cancel parameter must flow into the \
             FIX 1 turn-start guard call; mutating the parameter plumbing breaks this"
        );

        let guard_src = include_str!("intake_turn/stale_dispatch_guard.rs");
        assert!(
            guard_src.contains("&& !preserve_on_cancel"),
            "the extracted turn-start guard must still gate on `!preserve_on_cancel`"
        );
    }
}
