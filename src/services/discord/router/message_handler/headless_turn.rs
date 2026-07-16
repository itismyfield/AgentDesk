use super::*;

fn valid_routine_metadata(metadata: Option<&serde_json::Value>) -> Option<&serde_json::Value> {
    let metadata = metadata?;
    metadata
        .get("routine_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())?;
    Some(metadata)
}

fn routine_metadata_agent_id(metadata: Option<&serde_json::Value>) -> Option<&str> {
    valid_routine_metadata(metadata)?
        .get("agent_id")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

/// Whether an explicit routine turn must sever provider and transcript continuity.
/// Only `persistent` routines retain continuity; absent strategy preserves the
/// legacy routine default of `fresh`. Non-routine metadata must never reset a
/// provider session.
fn fresh_routine_turn(metadata: Option<&serde_json::Value>) -> bool {
    let Some(metadata) = valid_routine_metadata(metadata) else {
        return false;
    };
    metadata
        .get("execution_strategy")
        .and_then(|value| value.as_str())
        != Some("persistent")
}

async fn persist_boundary_before_provider_clear<B, BFut, C, CFut, E>(
    persist_boundary: bool,
    clear_provider: bool,
    boundary: B,
    clear: C,
) -> Result<(), E>
where
    B: FnOnce() -> BFut,
    BFut: std::future::Future<Output = Result<(), E>>,
    C: FnOnce() -> CFut,
    CFut: std::future::Future<Output = ()>,
{
    if persist_boundary {
        boundary().await?;
    }
    if clear_provider {
        clear().await;
    }
    Ok(())
}

pub(in crate::services::discord) async fn start_headless_turn(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    shared: &Arc<SharedData>,
    token: &str,
    source: Option<&str>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    start_reserved_headless_turn(
        ctx,
        channel_id,
        prompt,
        request_owner_name,
        shared,
        token,
        source,
        metadata,
        channel_name_hint,
        None,
        None,
        reserve_headless_turn(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn start_reserved_headless_turn(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    shared: &Arc<SharedData>,
    token: &str,
    source: Option<&str>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    // #5: synthetic tmux-session label for routine turns (see
    // `start_reserved_headless_turn_with_owner`); `None` for all other callers.
    tmux_session_label: Option<String>,
    is_dm_hint: Option<bool>,
    reservation: HeadlessTurnReservation,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    start_reserved_headless_turn_with_owner(
        ctx,
        channel_id,
        prompt,
        request_owner_name,
        UserId::new(1),
        shared,
        token,
        source,
        metadata,
        channel_name_hint,
        tmux_session_label,
        is_dm_hint,
        reservation,
    )
    .await
}

fn routine_metadata_role_binding(
    metadata: Option<&serde_json::Value>,
    provider: &ProviderKind,
) -> Option<settings::RoleBinding> {
    let metadata = valid_routine_metadata(metadata)?;
    let agent_id = routine_metadata_agent_id(Some(metadata))?;
    // Resolve the agent's configured prompt path instead of hardcoding
    // IDENTITY.md under config/agents: `default_prompt_path` reads the managed
    // agents root and falls back to the legacy `<id>.prompt.md` layout, so
    // agents on either layout still get their role prompt for routine turns
    // (#3463). Falls back to the canonical IDENTITY.md path when unset.
    let prompt_file = crate::services::discord::agentdesk_config::default_prompt_path(agent_id)
        .unwrap_or_default();

    Some(settings::RoleBinding {
        role_id: agent_id.to_string(),
        prompt_file,
        provider: Some(provider.clone()),
        model: None,
        reasoning_effort: None,
        peer_agents_enabled: true,
        quality_feedback_injection_enabled: true,
        memory: settings::resolve_memory_settings(None, None),
    })
}

#[allow(dead_code)] // #3034: exported voice entry point, wired-but-dormant (no live dispatch yet).
pub(in crate::services::discord) async fn start_voice_headless_turn(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    request_owner: UserId,
    shared: &Arc<SharedData>,
    token: &str,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    start_reserved_headless_turn_with_owner(
        ctx,
        channel_id,
        prompt,
        request_owner_name,
        request_owner,
        shared,
        token,
        Some(crate::dispatch::Source::Voice.as_str()),
        metadata,
        channel_name_hint,
        None,
        Some(false),
        reserve_headless_turn(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn start_reserved_headless_turn_with_owner(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    request_owner: UserId,
    shared: &Arc<SharedData>,
    token: &str,
    source: Option<&str>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    // #5: When set, this synthetic label (e.g. a routine's distinct tmux session
    // name) drives ONLY the tmux-session naming below, decoupled from
    // `channel_name_hint`, which must stay the agent's REAL primary
    // channel/alias so `resolve_workspace`/worktree isolation/dispatch/role and
    // the routine-identity reset guard keep resolving against the real channel.
    // Non-routine callers pass `None` and behave exactly as before.
    tmux_session_label: Option<String>,
    is_dm_hint: Option<bool>,
    reservation: HeadlessTurnReservation,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err(HeadlessTurnStartError::Internal(
            "prompt is required".to_string(),
        ));
    }

    shared.record_channel_speaker(
        channel_id,
        request_owner,
        request_owner_name,
        is_dm_hint.unwrap_or(false),
    );
    let user_msg_id = reservation.user_msg_id;
    let placeholder_msg_id = reservation.placeholder_msg_id;
    let (settings_provider, allowed_tools) = {
        let settings = shared.settings.read().await;
        (settings.provider.clone(), settings.allowed_tools.clone())
    };
    let routine_role_binding = routine_metadata_role_binding(metadata.as_ref(), &settings_provider);
    let (early_stale_session_id, early_channel_name) = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .map(|session| (session.session_id.clone(), session.channel_name.clone()))
            .unwrap_or_default()
    };
    let early_thread_parent =
        super::super::super::resolve_thread_parent(&ctx.http, channel_id).await;
    let early_resolved_channel_name = if early_channel_name.is_none() && channel_name_hint.is_none()
    {
        let (channel_name, _) = resolve_channel_category(&ctx.http, None, channel_id).await;
        channel_name
    } else {
        None
    };
    let early_role_binding = routine_role_binding
        .clone()
        .or_else(|| {
            resolve_thread_role_binding(
                channel_id,
                early_channel_name
                    .as_deref()
                    .or(channel_name_hint.as_deref())
                    .or(early_resolved_channel_name.as_deref()),
                early_thread_parent.as_ref(),
            )
            .role_binding
        })
        .or_else(|| {
            early_thread_parent.is_none().then(|| {
                metadata_parent_channel_id(metadata.as_ref())
                    .and_then(|parent_id| resolve_role_binding(parent_id, None))
            })?
        });
    let early_provider = early_role_binding
        .as_ref()
        .and_then(|binding| binding.provider.clone())
        .unwrap_or_else(|| settings_provider.clone());
    let resolved_channel_name_for_session = channel_name_hint
        .clone()
        .or_else(|| early_resolved_channel_name.clone())
        .or_else(|| {
            super::super::super::adk_session::registered_channel_fallback_name(
                channel_id,
                &early_provider,
            )
        });
    let early_fast_mode_channel_id =
        effective_fast_mode_channel_id(channel_id, early_thread_parent.clone());
    if let GoalCommandKind::Lifecycle(command) = classify_codex_goal_command_for_provider(
        &early_provider,
        prompt,
        super::super::super::commands::channel_codex_goals_setting(
            shared,
            early_fast_mode_channel_id,
        )
        .await,
    ) {
        consume_codex_goal_lifecycle_command(
            &ctx.http,
            shared,
            &early_provider,
            channel_id,
            command,
            early_stale_session_id,
        )
        .await;
        return Ok(HeadlessTurnStartOutcome {
            turn_id: reservation.turn_id(channel_id),
            status: HeadlessTurnStartStatus::Consumed,
        });
    }
    let cancel_token = Arc::new(CancelToken::new());
    let started = super::super::super::mailbox_try_start_turn(
        shared,
        channel_id,
        cancel_token.clone(),
        request_owner,
        user_msg_id,
    )
    .await;
    if !started {
        return Err(HeadlessTurnStartError::Conflict(format!(
            "agent mailbox is busy for channel {}",
            channel_id.get()
        )));
    }
    // Compute the routine continuity policy once at the turn-start boundary.
    // The shared `/goal fresh` machinery below clears every provider restore path
    // and leaves memento (caseId) as the only cross-run continuity.
    let fresh_routine = fresh_routine_turn(metadata.as_ref());
    let (mut session_id, mut memento_context_loaded, mut current_path) = {
        let mut data = shared.core.lock().await;
        if let Some(info) = load_session_runtime_state(&mut data.sessions, channel_id) {
            // Existing sessions retain their child-channel runtime identity.
            if let Some(channel_name) = resolved_channel_name_for_session.as_ref()
                && let Some(session) = data.sessions.get_mut(&channel_id)
                && session.channel_name.is_none()
            {
                session.channel_name = Some(channel_name.clone());
            }
            info
        } else {
            let workspace = resolve_headless_workspace(
                channel_id,
                resolved_channel_name_for_session.as_deref(),
                early_thread_parent.as_ref(),
                metadata.as_ref(),
            )
            .ok_or_else(|| {
                HeadlessTurnStartError::Internal(format!(
                    "no workspace resolved for headless turn channel {}",
                    channel_id.get()
                ))
            });
            let workspace = match workspace {
                Ok(workspace) => workspace,
                Err(error) => {
                    let _ = release_mailbox_after_placeholder_post_failure(
                        shared,
                        &early_provider,
                        channel_id,
                    )
                    .await;
                    return Err(error);
                }
            };
            let workspace_path = std::path::Path::new(&workspace);
            if !workspace_path.is_dir() {
                let _ = release_mailbox_after_placeholder_post_failure(
                    shared,
                    &early_provider,
                    channel_id,
                )
                .await;
                return Err(HeadlessTurnStartError::Internal(format!(
                    "resolved workspace does not exist for headless turn: {workspace}"
                )));
            }
            let canonical = workspace_path
                .canonicalize()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|_| workspace.clone());
            let session = data
                .sessions
                .entry(channel_id)
                .or_insert_with(|| DiscordSession {
                    session_id: None,
                    memento_context_loaded: false,
                    memento_reflected: false,
                    current_path: None,
                    history: Vec::new(),
                    pending_uploads: Vec::new(),
                    cleared: false,
                    channel_name: resolved_channel_name_for_session.clone(),
                    category_name: None,
                    remote_profile_name: None,
                    channel_id: Some(channel_id.get()),
                    last_active: tokio::time::Instant::now(),
                    worktree: None,
                    born_generation: super::super::super::runtime_store::load_generation(),
                });
            session.current_path = Some(canonical.clone());
            if session.channel_name.is_none() {
                session.channel_name = channel_name_hint.clone();
            }
            session.channel_id = Some(channel_id.get());
            session.last_active = tokio::time::Instant::now();
            (
                session.session_id.clone(),
                session.memento_context_loaded,
                canonical,
            )
        }
    };
    let mut session_strategy_reason = if session_id.is_some() {
        "runtime_cached_provider_session"
    } else {
        "no_runtime_provider_session"
    };

    let (pending_uploads, session_was_cleared) = {
        let mut data = shared.core.lock().await;
        data.sessions
            .get_mut(&channel_id)
            .map(|session| {
                let was_cleared = session.cleared;
                session.cleared = false;
                (std::mem::take(&mut session.pending_uploads), was_cleared)
            })
            .unwrap_or_default()
    };

    let turn_id = reservation.turn_id(channel_id);
    let mut resolved_role_binding = {
        let data = shared.core.lock().await;
        let channel_name = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.as_deref());
        if let Some(binding) = routine_role_binding.clone() {
            ResolvedThreadRoleBinding::direct(Some(binding))
        } else {
            let mut resolved =
                resolve_thread_role_binding(channel_id, channel_name, early_thread_parent.as_ref());
            if resolved.role_binding.is_none() && early_thread_parent.is_none() {
                resolved.role_binding = metadata_parent_channel_id(metadata.as_ref())
                    .and_then(|parent_id| resolve_role_binding(parent_id, None));
            }
            resolved
        }
    };
    let memory_scope_channel_id = resolved_role_binding.memory_channel_id(channel_id);
    let memory_channel_id = memory_scope_channel_id.get();
    let inherited_memory_channel_name = resolved_role_binding.memory_channel_name(None);
    let role_binding = resolved_role_binding.role_binding.take();
    let provider = role_binding
        .as_ref()
        .and_then(|binding| binding.provider.clone())
        .unwrap_or(settings_provider);
    let routine_metadata_agent_id = routine_metadata_agent_id(metadata.as_ref());
    let routine_targets_resolved_role = routine_metadata_agent_id
        .zip(
            role_binding
                .as_ref()
                .map(|binding| binding.role_id.as_str()),
        )
        .is_some_and(|(metadata_agent_id, role_id)| metadata_agent_id == role_id);
    let routine_agent_identity_changed = if routine_targets_resolved_role {
        if let Some(channel_name_hint) = channel_name_hint
            .as_ref()
            .filter(|value| !value.trim().is_empty())
        {
            let data = shared.core.lock().await;
            data.sessions.get(&channel_id).is_some_and(|session| {
                session.channel_name.as_deref() != Some(channel_name_hint.as_str())
            })
        } else {
            false
        }
    } else {
        false
    };
    if routine_agent_identity_changed {
        if let Err(error) = crate::db::session_transcripts::record_channel_clear_boundary(
            shared.pg_pool.as_ref(),
            &channel_id.get().to_string(),
        )
        .await
        {
            let _ =
                release_mailbox_after_placeholder_post_failure(shared, &provider, channel_id).await;
            return Err(HeadlessTurnStartError::Internal(format!(
                "failed to persist routine agent identity context boundary: {error}"
            )));
        }
        let mut data = shared.core.lock().await;
        if let Some(session) = data.sessions.get_mut(&channel_id) {
            session.channel_name = channel_name_hint.clone();
            session.clear_provider_session();
            session_id = None;
            memento_context_loaded = false;
            session_strategy_reason = "routine_agent_identity_changed";
        }
    }
    {
        let channel_name_for_isolation = {
            let data = shared.core.lock().await;
            data.sessions
                .get(&channel_id)
                .and_then(|session| session.channel_name.clone())
                .or_else(|| channel_name_hint.clone())
        };
        let isolation_outcome = ensure_provider_worktree_isolation(
            shared,
            channel_id,
            &mut current_path,
            &provider,
            channel_name_for_isolation.as_deref(),
            None,
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
    let dispatch_profile = {
        let data = shared.core.lock().await;
        let channel_name = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.as_deref());
        DispatchProfile::for_turn(
            None,
            settings::resolve_dispatch_profile(channel_id, channel_name),
        )
    };

    let fast_mode_channel_id = effective_fast_mode_channel_id(channel_id, early_thread_parent);
    super::super::super::commands::reset_provider_session_if_pending(
        &ctx.http,
        shared,
        &provider,
        channel_id,
        fast_mode_channel_id,
    )
    .await;
    refresh_session_strategy_after_pending_reset(
        shared,
        channel_id,
        &mut session_id,
        &mut memento_context_loaded,
        &mut session_strategy_reason,
    )
    .await;

    let prompt_prep_started = std::time::Instant::now();
    let (channel_name, tmux_session_name, category_name) = {
        let data = shared.core.lock().await;
        let channel_name = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.channel_name.clone())
            .or_else(|| channel_name_hint.clone());
        let tmux_session_name = if provider.uses_managed_tmux_backend() {
            // #5: Prefer the synthetic routine label (when supplied) so the
            // routine keeps its DISTINCT tmux session (#3463: routine-name-first
            // label avoids two routines on one agent colliding) while
            // `channel_name` stays the REAL channel for workspace/dispatch/role.
            tmux_session_label
                .as_deref()
                .or(channel_name.as_deref())
                .map(|name| provider.build_tmux_session_name(name))
        } else {
            None
        };
        let category_name = data
            .sessions
            .get(&channel_id)
            .and_then(|session| session.category_name.clone());
        (channel_name, tmux_session_name, category_name)
    };
    let adk_session_key = build_adk_session_key(shared, channel_id, &provider).await;
    if valid_routine_metadata(metadata.as_ref()).is_some()
        && let (Some(pool), Some(binding), Some(session_key)) = (
            shared.pg_pool.as_ref(),
            role_binding.as_ref(),
            adk_session_key.as_deref(),
        )
        && let Err(error) = sqlx::query(
            // Scope to the live session row for this channel. `status` never
            // takes the literal 'closed' (closure is tracked by `closed_at`; the
            // 4-state column is turn_active/awaiting_*/idle/disconnected/aborted),
            // so the old `status <> 'closed'` guard matched EVERY row for the
            // channel and rewrote the UNIQUE `session_key` on all of them —
            // corrupting stale rows or hitting the session_key unique constraint.
            // Restrict to non-closed rows and never overwrite a different
            // session's key (only stamp an unset key or refresh this same key).
            "UPDATE sessions
                SET agent_id = $1,
                    provider = $2,
                    session_key = $3
              WHERE channel_id = $4
                AND closed_at IS NULL
                AND (session_key IS NULL OR session_key = $3)",
        )
        .bind(&binding.role_id)
        .bind(provider.as_str())
        .bind(session_key)
        .bind(channel_id.get().to_string())
        .execute(pool)
        .await
    {
        tracing::warn!(
            channel_id = channel_id.get(),
            agent_id = %binding.role_id,
            provider = %provider.as_str(),
            error = %error,
            "failed to refresh routine headless session identity"
        );
    }
    let headless_goal_kind = classify_codex_goal_command_for_provider(
        &provider,
        prompt,
        super::super::super::commands::channel_codex_goals_setting(shared, fast_mode_channel_id)
            .await,
    );
    if let GoalCommandKind::Lifecycle(command) = headless_goal_kind {
        consume_codex_goal_lifecycle_command(
            &ctx.http,
            shared,
            &provider,
            channel_id,
            command,
            session_id.clone(),
        )
        .await;
        let _ = release_mailbox_after_placeholder_post_failure(shared, &provider, channel_id).await;
        return Ok(HeadlessTurnStartOutcome {
            turn_id: reservation.turn_id(channel_id),
            status: HeadlessTurnStartStatus::Consumed,
        });
    }
    let session_retry_context = take_session_retry_context(shared, channel_id, Some(&turn_id));
    let retry_context = session_retry_context.as_ref();
    let reply_context = retry_context.map(|c| c.formatted_context.clone());
    let goal_fresh = matches!(headless_goal_kind, GoalCommandKind::FreshStart);
    // Routine metadata reasserts the fresh boundary on every routine run, while
    // later user-authored turns carry no routine marker. Persist both deliberate
    // severances so no later turn can re-inject transcript pairs from before one.
    let fresh_context_severance = goal_fresh || fresh_routine;
    // Fresh routines use the same provider-severance machinery as `/goal fresh`:
    // clear in-memory, DB, stale IDs, and live-TUI bindings; skip restoration;
    // and force a cold provider launch. Prompt rewriting remains goal-only.
    let force_fresh_provider_session = fresh_context_severance || routine_agent_identity_changed;
    let severance = persist_boundary_before_provider_clear(
        fresh_context_severance,
        force_fresh_provider_session,
        || record_fresh_session_context_boundary(shared, channel_id),
        || {
            clear_codex_goal_start_provider_session(
                shared,
                channel_id,
                adk_session_key.as_deref(),
                &mut session_id,
                &mut memento_context_loaded,
                &mut session_strategy_reason,
            )
        },
    )
    .await;
    if let Err(error) = severance {
        let _ = release_mailbox_after_placeholder_post_failure(shared, &provider, channel_id).await;
        return Err(HeadlessTurnStartError::Internal(format!(
            "failed to persist fresh-session context boundary: {error}"
        )));
    }
    if force_fresh_provider_session {
        session_strategy_reason = if goal_fresh {
            "goal_fresh_provider_session"
        } else if routine_agent_identity_changed {
            "routine_agent_identity_changed"
        } else {
            "fresh_routine_provider_session"
        };
        // The provider-state clear does not remove Claude's live-TUI runtime
        // binding. Clear it explicitly so fresh routine and goal turns cannot
        // recover resume mode from a warm tmux pane.
        if let Some(ref tmux_session) = tmux_session_name {
            crate::services::tui_prompt_dedupe::clear_tmux_runtime_binding(tmux_session);
        }
    }
    let effective_prompt: std::borrow::Cow<str> = if goal_fresh {
        std::borrow::Cow::Owned(rewrite_fresh_goal_prompt(prompt))
    } else {
        std::borrow::Cow::Borrowed(prompt)
    };
    if session_id.is_none() {
        if force_fresh_provider_session {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let reason = if goal_fresh {
                "/goal fresh session request"
            } else if routine_agent_identity_changed {
                "routine agent identity change"
            } else {
                "fresh routine turn"
            };
            tracing::info!(
                "  [{ts}] ↻ Skipping DB/live provider session restore for headless channel {} due to {reason}",
                channel_id.get()
            );
        } else if session_was_cleared {
            session_strategy_reason = "session_cleared_by_user";
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Skipping DB provider session restore for headless channel {} due to prior /clear",
                channel_id.get()
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
                    "  [{ts}] ↻ Restored provider session_id from DB for headless {}",
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

    cleanup_terminal_delivery_marker_after_turn_start(
        shared,
        channel_id,
        adk_session_key.as_deref(),
    )
    .await;
    crate::services::discord::increment_global_active(shared, "headless_turn_start");
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
        None,
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
        prompt,
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
                channel_name: inherited_memory_channel_name
                    .clone()
                    .or_else(|| channel_name.clone()),
                session_id: resolve_memory_session_id(session_id.as_deref(), memory_channel_id),
                dispatch_profile,
                user_text: prompt.to_string(),
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
            "  [{ts}] [memory] memento recall gate for headless channel {}: decision={} mode={:?} reason={} context_loaded={} recall_bytes={} input_tokens={} output_tokens={}",
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
            "  [{ts}] [memory] recall warning for headless channel {}: {}",
            channel_id.get(),
            warning
        );
    }

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
        dispatch_profile,
        None,
        session_retry_context.as_ref(),
    )
    .await;
    if !pending_uploads.is_empty() {
        context_chunks.push(pending_uploads.join("\n"));
    }
    if let Some(headless_context) = build_headless_trigger_context(source, metadata.as_ref()) {
        context_chunks.push(headless_context);
    }
    if let Some(reply_context) = reply_context {
        context_chunks.push(reply_context);
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
        ai_screen::sanitize_user_input(&effective_prompt),
    ));
    let context_prompt = crate::services::provider::compact_resumed_provider_turn_prompt(
        &provider,
        session_id.as_deref(),
        context_chunks.join("\n\n"),
    );

    let discord_context = build_system_discord_context(
        channel_name.as_deref(),
        category_name.as_deref(),
        channel_id,
        true,
    );

    let sak_for_system = memory_injection_plan.sak_for_system_prompt();
    let longterm_catalog_for_prompt = memory_injection_plan.longterm_catalog_for_system_prompt;
    let memento_mcp_available = crate::services::mcp_config::provider_has_memento_mcp(&provider);
    let channel_participants = shared.channel_roster(channel_id, request_owner, request_owner_name);
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
        &channel_participants,
        &current_path,
        channel_id,
        memory_scope_channel_id,
        token,
        role_binding.as_ref(),
        false,
        PromptProfiles::headless(dispatch_profile),
        None,
        None,
        sak_for_system,
        longterm_catalog_for_prompt,
        Some(&memory_settings),
        memento_mcp_available,
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
    let prompt_prep_duration_ms = prompt_prep_started.elapsed().as_millis();
    let memory_backend_label = memory_settings.backend.as_str();
    let provider_label = match &provider {
        ProviderKind::Claude => "claude",
        ProviderKind::Codex => "codex",
        ProviderKind::Gemini => "gemini",
        ProviderKind::OpenCode => "opencode",
        ProviderKind::Qwen => "qwen",
        ProviderKind::Unsupported(_) => "unsupported",
    };
    let ts = chrono::Local::now().format("%H:%M:%S");
    tracing::info!(
        "  [{ts}] [prompt-prep] headless channel={} provider={} dispatch={} memory_backend={} reused_session={} duration_ms={}",
        channel_id.get(),
        provider_label,
        dispatch_profile_label(dispatch_profile),
        memory_backend_label,
        session_id.is_some(),
        prompt_prep_duration_ms
    );
    // #1085: same session-reuse counter as the foreground path so headless (background-trigger) turns are reflected in the reuse-rate metric.
    crate::services::observability::metrics::record_session_entry(
        channel_id.get(),
        provider_label,
        session_id.is_some(),
    );

    spawn_headless_turn_watchdog(
        &cancel_token,
        shared,
        &ctx.http,
        channel_id,
        &provider,
        provider_label,
    );

    let remote_profile = {
        let data = shared.core.lock().await;
        data.sessions
            .get(&channel_id)
            .and_then(|session| session.remote_profile_name.as_ref())
            .and_then(|name| {
                let settings = crate::config::Settings::load();
                settings
                    .remote_profiles
                    .iter()
                    .find(|profile| profile.name == *name)
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
    let adk_session_info =
        derive_adk_session_info(Some(prompt), channel_name.as_deref(), Some(&current_path));
    let adk_thread_channel_id = adk_session_name
        .as_deref()
        .and_then(super::super::super::adk_session::parse_thread_channel_id_from_name);
    post_adk_session_status(
        adk_session_key.as_deref(),
        adk_session_name.as_deref(),
        model_for_turn.as_deref(),
        "working",
        &provider,
        Some(&adk_session_info),
        None,
        Some(&current_path),
        None,
        adk_thread_channel_id,
        Some(channel_id),
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
                        .map(|metadata| metadata.len())
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

    let mut inflight_state = InflightTurnState::new(
        provider.clone(),
        channel_id.get(),
        channel_name.clone(),
        request_owner.get(),
        user_msg_id.get(),
        placeholder_msg_id.get(),
        prompt.to_string(),
        session_id.clone(),
        inflight_tmux_name,
        inflight_output_path,
        inflight_input_fifo.clone(),
        inflight_offset,
    );
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
    inflight_state.logical_channel_id = Some(channel_id.get());
    inflight_state.session_key = adk_session_key.clone();
    inflight_state.delivery_bot = metadata_delivery_bot(metadata.as_ref());
    inflight_state.silent_turn = metadata_silent_flag(metadata.as_ref());
    inflight_state.source = metadata_turn_source(source, metadata.as_ref());
    if let Err(error) = save_inflight_state(&inflight_state) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!("  [{ts}]   ⚠ inflight state save failed: {error}");
    }

    let _ = attach_paused_turn_watcher_for_inflight(
        shared,
        ctx.http.clone(),
        &provider,
        channel_id,
        watcher_tmux_name,
        watcher_output_path,
        inflight_offset,
        "turn_start_headless",
        &mut inflight_state,
    );
    let (tx, rx) = mpsc::channel();
    let session_id_clone = session_id.clone();
    let current_path_clone = current_path.clone();
    let cancel_token_clone = cancel_token.clone();

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
                Err(error) => tracing::warn!(
                    "  [{ts}] ⚠ worktree-autosync: failed to run for headless turn — {error}"
                ),
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
    let ctx_thresholds =
        super::super::super::adk_session::fetch_context_thresholds(shared.api_port).await;
    let compact_percent = ctx_thresholds.compact_pct_for(&provider);
    let model_context_window = provider.resolve_context_window(model_for_turn.as_deref());
    let compact_percent_for_claude = Some(ctx_thresholds.compact_pct_for(&provider));
    let compact_token_limit_for_codex = {
        let cli_config = provider.compact_cli_config(compact_percent, model_context_window);
        cli_config
            .first()
            .map(|(_, value)| value.parse::<u64>().unwrap_or(0))
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

    let prompt_owned = prompt.to_string();
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
                            cache_ttl_minutes,
                            None,
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
                            None,
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
                            None,
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
            Ok(Err(error)) => {
                tracing::warn!("  [headless streaming] Error: {}", error);
                let _ = tx.send(StreamMessage::Error {
                    message: error,
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_code: None,
                });
            }
            Err(panic_info) => {
                let msg = if let Some(value) = panic_info.downcast_ref::<String>() {
                    value.clone()
                } else if let Some(value) = panic_info.downcast_ref::<&str>() {
                    value.to_string()
                } else {
                    "unknown panic".to_string()
                };
                tracing::warn!("  [headless streaming] PANIC: {}", msg);
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
        cancel_token,
        rx,
        TurnBridgeContext {
            provider,
            gateway: Arc::new(HeadlessGateway),
            channel_id,
            user_msg_id: Some(user_msg_id),
            user_text_owned: prompt_owned,
            request_owner_name: request_owner_name.to_string(),
            role_binding,
            adk_session_key,
            adk_session_name,
            adk_session_info: Some(adk_session_info),
            adk_cwd: Some(current_path),
            dispatch_id: None,
            dispatch_kind: None,
            memory_recall_usage: memory_recall.token_usage,
            context_window_tokens: model_context_window,
            context_compact_percent: compact_percent,
            current_msg_id: Some(placeholder_msg_id),
            response_sent_offset: 0,
            full_response: String::new(),
            tmux_last_offset: Some(inflight_offset),
            new_session_id: session_id,
            defer_watcher_resume: false,
            reuse_status_panel_message: false,
            completion_tx: None,
            is_external_input_tui_direct: false, // #3089 A6b: Discord-origin (not external-input)
            inflight_state,
        },
    );

    Ok(HeadlessTurnStartOutcome {
        turn_id: reservation.turn_id(channel_id),
        status: HeadlessTurnStartStatus::Started,
    })
}

#[cfg(test)]
mod recovery_context_take_order_tests {
    use super::super::super::super::prompt_builder::{
        DispatchProfile, PromptProfiles, build_system_prompt_with_manifest,
    };
    use super::super::super::super::settings::RoleBinding;
    use poise::serenity_prelude::ChannelId;

    fn recovery_context_take_call() -> String {
        format!(
            "{}{}",
            "let session_retry_context = ",
            "take_session_retry_context(shared, channel_id, Some(&turn_id));"
        )
    }

    #[test]
    fn reserved_headless_start_has_no_post_spawn_error_path() {
        let module_src = include_str!("headless_turn.rs");
        let spawn_pos = module_src
            .find("tokio::task::spawn_blocking")
            .expect("headless provider spawn boundary exists");
        let started_return = module_src[spawn_pos..]
            .find("status: HeadlessTurnStartStatus::Started")
            .map(|offset| spawn_pos + offset)
            .expect("headless Started return exists after provider spawn");
        let post_spawn = &module_src[spawn_pos..started_return];
        assert!(
            !post_spawn.contains("HeadlessTurnStartError::"),
            "post-spawn failures must flow through the bridge, never a retryable start error"
        );
    }

    #[test]
    fn recovery_context_survives_headless_goal_lifecycle_consumed_return() {
        let root = tempfile::tempdir().expect("create temp runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let module_src = include_str!("headless_turn.rs");
        let lifecycle_pos = module_src
            .find("if let GoalCommandKind::Lifecycle(command) = headless_goal_kind")
            .expect("headless lifecycle command branch exists");
        let consumed_return_pos = lifecycle_pos
            + module_src[lifecycle_pos..]
                .find("status: HeadlessTurnStartStatus::Consumed")
                .expect("headless lifecycle consumed return exists");
        let take_call = recovery_context_take_call();
        let take_pos = module_src
            .find(&take_call)
            .expect("headless recovery context take exists");

        assert!(
            consumed_return_pos < take_pos,
            "headless lifecycle Consumed return must happen before the destructive recovery-context take"
        );
    }

    #[test]
    fn headless_real_turn_consumes_recovery_context_once_before_prompt_use() {
        let root = tempfile::tempdir().expect("create temp runtime root");
        let _env = crate::config::set_agentdesk_root_for_test(root.path());
        let module_src = include_str!("headless_turn.rs");
        let take_call = recovery_context_take_call();
        let take_positions: Vec<_> = module_src.match_indices(&take_call).collect();
        assert_eq!(
            take_positions.len(),
            1,
            "headless turn start must have exactly one destructive recovery-context take"
        );
        let take_pos = take_positions[0].0;
        let reply_context_use_pos = module_src
            .find("context_chunks.push(reply_context);")
            .expect("headless prompt includes recovered reply context");
        let manifest_use_pos = module_src
            .find("let recovery_context_for_manifest =")
            .expect("headless prompt manifest receives recovery context");

        assert!(
            take_pos < reply_context_use_pos,
            "headless real turn must take recovery context before adding it to the prompt"
        );
        assert!(
            take_pos < manifest_use_pos,
            "headless real turn must take recovery context before prompt manifest capture"
        );
    }

    #[test]
    fn actual_headless_assembly_selects_only_all_and_headless_shared_sections() {
        let module_src = include_str!("headless_turn.rs");
        let builder_pos = module_src
            .find("let built_system_prompt = build_system_prompt_with_manifest(")
            .expect("headless prompt assembly exists");
        let builder_call = &module_src[builder_pos..];
        let call_end = builder_call
            .find("\n    );")
            .expect("headless prompt assembly call closes");
        let builder_call = &builder_call[..call_end];
        assert!(builder_call.contains("PromptProfiles::headless(dispatch_profile)"));
        assert!(!builder_call.contains("PromptProfiles::foreground"));

        let runtime_root = tempfile::tempdir().expect("runtime root");
        let _runtime_guard = crate::config::set_agentdesk_root_for_test(runtime_root.path());
        let shared_prompt_path = crate::runtime_layout::shared_prompt_path(runtime_root.path());
        std::fs::create_dir_all(shared_prompt_path.parent().expect("shared prompt parent"))
            .expect("create shared prompt parent");
        std::fs::write(
            shared_prompt_path,
            "<!-- profile: all -->\nHEADLESS ACTUAL ALL 4560\n<!-- /profile -->\n\
             <!-- profile: full -->\nHEADLESS ACTUAL FULL 4560\n<!-- /profile -->\n\
             <!-- profile: headless -->\nHEADLESS ACTUAL HEADLESS 4560\n<!-- /profile -->\n",
        )
        .expect("write shared prompt");
        let binding = RoleBinding {
            role_id: "headless-actual-profile-4560".to_string(),
            prompt_file: runtime_root
                .path()
                .join("missing-role-prompt.md")
                .display()
                .to_string(),
            provider: None,
            model: None,
            reasoning_effort: None,
            peer_agents_enabled: false,
            quality_feedback_injection_enabled: false,
            memory: Default::default(),
        };
        let built = build_system_prompt_with_manifest(
            "ctx",
            &[],
            "/nonexistent-headless-workspace-4560",
            ChannelId::new(1),
            ChannelId::new(1),
            "tok",
            Some(&binding),
            false,
            PromptProfiles::headless(DispatchProfile::Full),
            None,
            None,
            None,
            None,
            None,
            false,
            false,
            None,
            None,
            None,
            Some("turn-headless-actual-profile-4560"),
        );

        assert!(built.system_prompt.contains("HEADLESS ACTUAL ALL 4560"));
        assert!(
            built
                .system_prompt
                .contains("HEADLESS ACTUAL HEADLESS 4560")
        );
        assert!(!built.system_prompt.contains("HEADLESS ACTUAL FULL 4560"));
    }
}

#[cfg(test)]
mod headless_hard_ceiling_tests {
    //! #3557 (A) Codex-review r2: the headless watchdog now mirrors the
    //! foreground intake path's per-turn hard ceiling cap (the headless path
    //! had been missing it, and it also `mark_async_managed`s the token so the
    //! sync watchdog does not enforce — leaving this async loop as the ONLY
    //! bound). These tests reproduce the exact arithmetic the headless loop
    //! applies (initial-deadline `min` cap + auto-extend clamp) so a regression
    //! that drops the cap is caught at the headless call site, not only in the
    //! shared helper tests in `discord/mod.rs`.
    use super::super::super::super::{
        ProviderKind, clamp_auto_extend_deadline_ms, turn_hard_ceiling_deadline_ms,
        turn_watchdog_timeout,
    };

    /// Codex's tighter 4h ceiling must cap the headless INITIAL deadline below
    /// the 6h watchdog timeout — exactly the `min(now + timeout, ceiling)` the
    /// headless spawn now uses. Skipped when env overrides the defaults.
    #[test]
    fn headless_initial_deadline_capped_at_codex_ceiling() {
        if std::env::var("AGENTDESK_CODEX_TURN_HARD_CEILING_SECS").is_ok()
            || std::env::var("AGENTDESK_TURN_TIMEOUT_SECS").is_ok()
        {
            return;
        }
        let now_ms: i64 = 1_700_000_000_000;
        let proposed_initial_dl = now_ms + turn_watchdog_timeout().as_millis() as i64; // ~6h
        let codex_ceiling = turn_hard_ceiling_deadline_ms(now_ms, &ProviderKind::Codex);
        let initial = std::cmp::min(proposed_initial_dl, codex_ceiling);
        assert_eq!(
            initial, codex_ceiling,
            "headless Codex initial deadline must land at the 4h ceiling, not 6h"
        );
        assert!(
            initial < proposed_initial_dl,
            "the headless cap must actually lower the deadline below the watchdog timeout"
        );
        // The init-time one-shot warn fires exactly when proposed > ceiling.
        assert!(proposed_initial_dl > codex_ceiling);
    }

    /// For a default-Claude turn (generic ceiling == watchdog timeout) the
    /// headless initial cap is a no-op and the init warn must NOT fire.
    #[test]
    fn headless_initial_deadline_uncapped_for_default_claude() {
        if std::env::var("AGENTDESK_TURN_HARD_CEILING_SECS").is_ok()
            || std::env::var("AGENTDESK_TURN_TIMEOUT_SECS").is_ok()
        {
            return;
        }
        let now_ms: i64 = 1_700_000_000_000;
        let proposed_initial_dl = now_ms + turn_watchdog_timeout().as_millis() as i64;
        let claude_ceiling = turn_hard_ceiling_deadline_ms(now_ms, &ProviderKind::Claude);
        let initial = std::cmp::min(proposed_initial_dl, claude_ceiling);
        assert_eq!(initial, proposed_initial_dl);
        assert!(
            proposed_initial_dl <= claude_ceiling,
            "with equal defaults the headless init warn (proposed > ceiling) must not fire"
        );
    }

    /// The headless AUTO-EXTEND must clamp to the ceiling: a turn that keeps
    /// inflight warm can no longer push the deadline past its Codex ceiling.
    /// Mirrors `clamp_auto_extend_deadline_ms(now + timeout, ceiling)`.
    #[test]
    fn headless_auto_extend_clamped_at_codex_ceiling() {
        if std::env::var("AGENTDESK_CODEX_TURN_HARD_CEILING_SECS").is_ok()
            || std::env::var("AGENTDESK_TURN_TIMEOUT_SECS").is_ok()
        {
            return;
        }
        // Turn started 3h ago; an auto-extend would propose now + 6h, well past
        // the 4h Codex ceiling (1h of budget left), so the clamp must bind.
        let turn_started_ms: i64 = 1_700_000_000_000;
        let now_ms_check = turn_started_ms + 3 * 3600 * 1000;
        let ceiling_ms = turn_hard_ceiling_deadline_ms(turn_started_ms, &ProviderKind::Codex);
        let proposed_dl = now_ms_check + turn_watchdog_timeout().as_millis() as i64;
        let (new_dl, clamped) = clamp_auto_extend_deadline_ms(proposed_dl, ceiling_ms);
        assert!(
            clamped,
            "auto-extend past the Codex ceiling must be clamped"
        );
        assert_eq!(
            new_dl, ceiling_ms,
            "clamped deadline must park at the ceiling"
        );
        assert!(
            new_dl < proposed_dl,
            "the clamp must lower the proposed extension to the ceiling"
        );
    }
}

#[cfg(test)]
mod fresh_routine_tests {
    use super::{
        fresh_routine_turn, persist_boundary_before_provider_clear, routine_metadata_agent_id,
        routine_metadata_role_binding,
    };
    use crate::services::provider::ProviderKind;
    use serde_json::json;
    use std::cell::RefCell;

    #[test]
    fn legacy_routine_without_strategy_is_fresh() {
        assert!(fresh_routine_turn(Some(&json!({
            "routine_id": "routine-1",
            "is_dm": true
        }))));
    }

    #[test]
    fn explicit_fresh_routine_is_fresh_for_every_channel_path() {
        assert!(fresh_routine_turn(Some(&json!({
            "routine_id": "routine-1",
            "is_dm": true,
            "execution_strategy": "fresh"
        }))));
        assert!(fresh_routine_turn(Some(&json!({
            "routine_id": "routine-1",
            "is_dm": false,
            "execution_strategy": "fresh"
        }))));
        assert!(fresh_routine_turn(Some(&json!({
            "routine_id": "routine-1",
            "execution_strategy": "fresh"
        }))));
    }

    #[test]
    fn persistent_routine_keeps_provider_continuity() {
        assert!(!fresh_routine_turn(Some(&json!({
            "routine_id": "routine-1",
            "execution_strategy": "persistent"
        }))));
    }

    #[test]
    fn malformed_routine_metadata_cannot_enter_routine_paths() {
        for metadata in [
            json!({ "agent_id": "other-agent", "execution_strategy": "fresh" }),
            json!({ "routine_id": null, "agent_id": "other-agent" }),
            json!({ "routine_id": 7, "agent_id": "other-agent" }),
            json!({ "routine_id": " ", "agent_id": "other-agent" }),
        ] {
            assert!(!fresh_routine_turn(Some(&metadata)));
            assert!(routine_metadata_agent_id(Some(&metadata)).is_none());
            assert!(
                routine_metadata_role_binding(Some(&metadata), &ProviderKind::Claude).is_none()
            );
        }
        assert!(!fresh_routine_turn(None));
    }

    #[tokio::test]
    async fn fresh_routine_path_records_durable_boundary_before_provider_clear() {
        let events = RefCell::new(Vec::new());

        let result = persist_boundary_before_provider_clear(
            true,
            true,
            || async {
                events.borrow_mut().push("boundary");
                Ok::<_, &'static str>(())
            },
            || async {
                events.borrow_mut().push("clear");
            },
        )
        .await;

        assert_eq!(result, Ok(()));
        assert_eq!(events.into_inner(), vec!["boundary", "clear"]);
    }

    #[tokio::test]
    async fn fresh_routine_path_does_not_clear_provider_when_boundary_fails() {
        let events = RefCell::new(Vec::new());

        let result = persist_boundary_before_provider_clear(
            true,
            true,
            || async {
                events.borrow_mut().push("boundary");
                Err::<(), _>("persistence failed")
            },
            || async {
                events.borrow_mut().push("clear");
            },
        )
        .await;

        assert_eq!(result, Err("persistence failed"));
        assert_eq!(events.into_inner(), vec!["boundary"]);
    }
}
