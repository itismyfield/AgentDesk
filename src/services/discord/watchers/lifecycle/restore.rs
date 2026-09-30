use super::*;

use crate::services::discord::inflight::KeyedTeardown;
use crate::services::discord::tmux_lifecycle::DispatchTmuxProtection;
use crate::services::session_host::HostLiveness;
use crate::services::tmux_common::{current_tmux_owner_marker, tmux_owner_path};

pub(in crate::services::discord) fn session_belongs_to_current_runtime(
    session_name: &str,
    current_owner_marker: &str,
) -> bool {
    std::fs::read_to_string(tmux_owner_path(session_name))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(|value| value == current_owner_marker)
        .unwrap_or(false)
}

/// On startup, scan for surviving tmux sessions (AgentDesk-*) and restore watchers.
/// This handles the case where AgentDesk was restarted but tmux sessions are still alive.
pub(in crate::services::discord) async fn restore_tmux_watchers(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
) {
    let settings_snapshot = { shared.settings.read().await.clone() };
    let provider = settings_snapshot.provider.clone();

    // List tmux sessions matching our naming convention
    let output = match tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(crate::services::platform::tmux::list_session_names),
    )
    .await
    {
        Ok(Ok(Ok(names))) => names,
        _ => return, // No tmux, timeout, or no sessions
    };

    let agent_sessions: Vec<&str> = output
        .iter()
        .map(|l| l.trim())
        .filter(|l| {
            parse_provider_and_channel_from_tmux_name(l)
                .map(|(session_provider, _)| session_provider == provider)
                .unwrap_or(false)
        })
        .collect();

    if agent_sessions.is_empty() {
        return;
    }

    // Build channel name → ChannelId map from Discord API (sessions map may be empty after restart)
    let mut name_to_channel: std::collections::HashMap<String, (ChannelId, String)> =
        std::collections::HashMap::new();

    // Try from in-memory sessions first
    {
        let data = shared.core.lock().await;
        for (&ch_id, session) in &data.sessions {
            if let Some(ref ch_name) = session.channel_name {
                let tmux_name = provider.build_tmux_session_name(ch_name);
                name_to_channel.insert(tmux_name, (ch_id, ch_name.clone()));
            }
        }
    }

    // Durable tmux channel bindings cover DM sessions whose channel ID cannot be
    // reconstructed from the `dm-<user_id>` session name after restart.
    for session_name in &agent_sessions {
        if name_to_channel.contains_key(*session_name) {
            continue;
        }
        if let Some(channel_id) =
            crate::services::tmux_common::read_tmux_channel_binding(session_name)
        {
            if let Some((_, channel_name)) = parse_provider_and_channel_from_tmux_name(session_name)
            {
                name_to_channel.insert(
                    session_name.to_string(),
                    (ChannelId::new(channel_id), channel_name),
                );
            }
        }
    }

    // If in-memory sessions don't cover all tmux sessions, fetch from Discord API
    // (durable bindings above intentionally handle DMs before guild-only lookup).
    let unresolved: Vec<&&str> = agent_sessions
        .iter()
        .filter(|s| !name_to_channel.contains_key(**s))
        .collect();

    if !unresolved.is_empty() {
        // Fetch guild channels via Discord API
        if let Ok(guilds) = http.get_guilds(None, None).await {
            for guild_info in &guilds {
                if let Ok(channels) = guild_info.id.channels(http).await {
                    for (ch_id, channel) in &channels {
                        let role_binding = resolve_role_binding(*ch_id, Some(&channel.name));
                        if !channel_supports_provider(
                            &provider,
                            Some(&channel.name),
                            false,
                            role_binding.as_ref(),
                        ) {
                            continue;
                        }
                        let tmux_name = provider.build_tmux_session_name(&channel.name);
                        name_to_channel
                            .entry(tmux_name)
                            .or_insert((*ch_id, channel.name.clone()));
                    }
                }
            }
        }

        // Fallback for thread sessions: guild.channels() doesn't return threads.
        // Extract thread_id from the channel name suffix (-t{id}) and use it
        // as the channel_id directly, since Discord thread IDs are channel IDs.
        let still_unresolved: Vec<&&str> = agent_sessions
            .iter()
            .filter(|s| !name_to_channel.contains_key(**s))
            .collect();
        for session_name in &still_unresolved {
            if let Some((_, ch_name)) = parse_provider_and_channel_from_tmux_name(session_name) {
                if let Some(pos) = ch_name.rfind("-t") {
                    let suffix = &ch_name[pos + 2..];
                    if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_digit()) {
                        if let Ok(thread_id) = suffix.parse::<u64>() {
                            let channel_id = ChannelId::new(thread_id);
                            name_to_channel
                                .entry(session_name.to_string())
                                .or_insert((channel_id, ch_name.clone()));
                        }
                    }
                }
            }
        }

        // agentdesk.yaml-backed settings are the final source of truth, so
        // consult them only for sessions all earlier paths left unresolved.
        let config_unresolved: Vec<&str> = agent_sessions
            .iter()
            .copied()
            .filter(|session| !name_to_channel.contains_key(*session))
            .collect();
        if !config_unresolved.is_empty() {
            add_configured_channel_bindings(
                &config_unresolved,
                &provider,
                super::super::super::settings::list_registered_channel_bindings(),
                &mut name_to_channel,
            );
        }
    }

    // Collect sessions to restore
    struct PendingWatcher {
        channel_id: ChannelId,
        output_path: String,
        session_name: String,
        initial_offset: u64,
        restored_turn: Option<RestoredWatcherTurn>,
        thread_parent: Option<ThreadFollowUpParent>,
        codex_direct_resume_fallback: Option<codex_restore::DirectResumeFallback>,
    }

    let mut pending: Vec<PendingWatcher> = Vec::new();
    let mut dead_cleanups: Vec<DeadSessionCleanup> = Vec::new();
    let mut owned_sessions: std::collections::HashMap<ChannelId, String> =
        std::collections::HashMap::new();
    let mut restore_claimed_claude_tui_transcripts: std::collections::HashSet<std::path::PathBuf> =
        std::collections::HashSet::new();

    for session_name in &agent_sessions {
        let Some((channel_id, channel_name)) = name_to_channel.get(*session_name) else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — channel mapping not found",
                session_name
            );
            continue;
        };

        // #148: Do NOT register in owned_sessions yet — QUARANTINE check below may
        // skip this session. Registering early blocks new session creation for the channel.
        let is_dm = matches!(
            channel_id.to_channel(http.as_ref()).await,
            Ok(serenity::model::channel::Channel::Private(_))
        );
        // Resolve thread parent so validation uses the same semantics
        // as normal message routing (router.rs).
        let (allowlist_channel_id, provider_channel_name) = if let Some((pid, pname)) =
            super::super::super::resolve_thread_parent(http, *channel_id).await
        {
            (pid, pname.unwrap_or_else(|| channel_name.clone()))
        } else {
            (*channel_id, channel_name.clone())
        };
        if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
            &settings_snapshot,
            &provider,
            allowlist_channel_id,
            Some(&channel_name),
            Some(&provider_channel_name),
            is_dm,
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — {reason} for channel {}",
                session_name,
                channel_id
            );
            continue;
        }

        if let Some(started) = super::super::super::mailbox_snapshot(&shared, *channel_id)
            .await
            .recovery_started_at
        {
            // #2443 — `recovery_done.wait()` is the deterministic graduation
            // signal for this skip. `restore_tmux_watchers` is a one-shot
            // caller (the loop body simply `continue`s and the upper
            // restore-loop tick reruns later), so we cannot block here for
            // ~60s. Instead, we race a *short* `recovery_done.wait()` against
            // a near-zero timeout: if recovery has already completed (latch
            // set), we proceed immediately; otherwise we fall through to the
            // legacy 60s skip / stale-cleanup heuristic which acts as the
            // hook-miss safety net the issue body asked us to retain.
            //
            // The 100ms grace window catches the common case where recovery
            // completed *just before* the watcher loop reached this check
            // (the producer in `mailbox_clear_recovery_marker` / `finish_turn`
            // calls `mark_done()` *after* clearing `recovery_started_at`, so
            // a clean completion already short-circuits via the snapshot
            // being `None` — this branch only runs when the snapshot still
            // sees a started marker, i.e. we *just* missed the wake-up).
            let recovery_done =
                crate::services::turn_orchestrator::ChannelMailboxRegistry::global_recovery_done(
                    *channel_id,
                );
            let recovery_completed = if let Some(signal) = recovery_done.as_ref() {
                tokio::time::timeout(std::time::Duration::from_millis(100), signal.wait())
                    .await
                    .is_ok()
            } else {
                false
            };

            if recovery_completed {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ✅ recovery_done signal observed for {} — proceeding with watcher restore",
                    session_name
                );
                super::super::super::mailbox_clear_recovery_marker(&shared, *channel_id).await;
            } else if started.elapsed() < std::time::Duration::from_secs(60) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏳ watcher skip for {} — recovery in progress ({:.0}s ago, hook-miss fallback)",
                    session_name,
                    started.elapsed().as_secs_f64()
                );
                continue;
            } else {
                // Stale recovery — remove marker and proceed with watcher.
                // Reaching this branch means the 60s hook-miss fallback
                // tripped; track it so we can monitor `recovery_done`
                // signal coverage in the field.
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ clearing stale recovery marker for {} ({:.0}s elapsed) — recovery_done hook missed",
                    session_name,
                    started.elapsed().as_secs_f64()
                );
                super::super::super::mailbox_clear_recovery_marker(&shared, *channel_id).await;
            }
        }

        // Accept either the new persistent location or the legacy /tmp
        // location — older wrappers still write to /tmp, and a dcserver
        // restart that lost /tmp files should not falsely flag a live
        // session as "no output file". See issue #892.
        //
        // #2795: codex_tui writes its rollout transcript directly to
        // `~/.codex/sessions/...` and never lands a JSONL at the AgentDesk
        // resolve path. When a dcserver restart happens mid-turn (agent ran
        // deploy from inside its own turn), the inflight row is preserved
        // but the AgentDesk relay JSONL is absent. Fall back to the actual
        // codex rollout looked up by the inflight `session_id` so the
        // restore loop can still attach a watcher and keep the live pane
        // relayed.
        let configured_workspace = super::super::super::settings::resolve_workspace(
            *channel_id,
            Some(channel_name.as_str()),
        );
        let session_keys = super::super::super::adk_session::build_session_key_candidates(
            &shared.token_hash,
            &provider,
            session_name,
        );
        let restored_cwd =
            load_restored_session_cwd(shared.pg_pool.as_ref(), &session_keys, channel_id.get());

        let mut selected_claude_tui_fallback_transcript: Option<std::path::PathBuf> = None;
        let mut codex_direct_resume_fallback = None;
        let output_path =
            match crate::services::tmux_common::resolve_session_temp_path(session_name, "jsonl") {
                Some(path) => path,
                None => {
                    if let Some(path) =
                        codex_restore::rollout_fallback_for_session(&provider, *channel_id)
                    {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ↻ watcher restore for {} — codex rollout fallback {}",
                            session_name,
                            path
                        );
                        path
                    } else if let Some(path) =
                        codex_restore::rollout_fallback_for_live_direct_resume(
                            &provider,
                            session_name,
                            *channel_id,
                        )
                    {
                        let output_path = path.output_path().to_string();
                        codex_direct_resume_fallback = Some(path);
                        output_path
                    } else if let Some(path) = claude_tui_transcript_fallback_path(
                        &provider,
                        session_name,
                        configured_workspace.as_deref(),
                        restored_cwd.as_deref(),
                        shared,
                        None,
                        &restore_claimed_claude_tui_transcripts,
                    ) {
                        // #2853: claude_tui never lands the wrapper JSONL, so
                        // recover the watcher onto the freshest safe Claude
                        // rollout transcript for the actual launched cwd,
                        // bounded by launch time and other live-session claims.
                        selected_claude_tui_fallback_transcript =
                            Some(std::path::PathBuf::from(&path));
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ↻ watcher restore for {} — claude transcript fallback {}",
                            session_name,
                            path
                        );
                        path
                    } else {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ⏭ watcher skip for {} — no output file",
                            session_name
                        );
                        continue;
                    }
                }
            };

        if let Some((owner_channel_id, cancelled, paused, existing_output_path)) =
            find_watcher_by_tmux_session(&shared.tmux_watchers, session_name)
        {
            if restore_scan_should_skip_existing_watcher(
                cancelled,
                paused,
                &existing_output_path,
                &output_path,
            ) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux session already watched by channel {}",
                    session_name,
                    owner_channel_id
                );
                continue;
            }
            if !cancelled {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ watcher replace for {} — existing output path {} differs from restored output path {}",
                    session_name,
                    existing_output_path,
                    output_path
                );
            }
        }

        // Old-gen sessions: adopt instead of killing.
        // The tmux session and Claude CLI process are still alive from the
        // previous dcserver — just update the generation marker and re-attach
        // a watcher. Auto-retry handles stale Claude session IDs if needed.
        let gen_marker_path =
            crate::services::tmux_common::session_temp_path(session_name, "generation");
        let session_gen = std::fs::read_to_string(&gen_marker_path)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        let current_gen = super::super::super::runtime_store::process_generation();
        if session_gen < current_gen && current_gen > 0 {
            // Skip sessions belonging to other runtimes
            let current_owner_marker = current_tmux_owner_marker();
            if !session_belongs_to_current_runtime(session_name, &current_owner_marker) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — owned by other runtime",
                    session_name
                );
                continue;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ Adopting old-gen session {} (gen {} → {})",
                session_name,
                session_gen,
                current_gen
            );
            // Update through one fd so a concurrent spawn replacement cannot
            // receive this wrapper's old mtime. A missing marker is recovered
            // with create-new/retry inside the helper.
            //
            // #1275 P2 #1: the `.generation` mtime is the wrapper-identity
            // signal used by `watermark_after_output_regression`. Adoption
            // does NOT respawn the wrapper (the tmux session and Claude CLI
            // process are still alive from the previous dcserver), so the
            // mtime must stay pinned to its original value. Otherwise a
            // restored watcher with `last_watcher_relayed_generation_mtime_ns`
            // captured before the dcserver restart will mismatch the freshly
            // touched `.generation` mtime, the regression check classifies
            // as fresh wrapper, clears `last_relayed_offset`, and a rotated
            // jsonl re-relays surviving content.
            preserve_session_generation_mtime_after_write(
                session_name,
                &gen_marker_path,
                current_gen.to_string().as_bytes(),
                "adoption_marker_rewrite",
            );
        }

        let dead = DeadSessionCleanup::probe(channel_id.get(), &channel_name, session_name);
        if let Some(dc) = dead.await {
            let ts = chrono::Local::now().format("%H:%M:%S");
            let observed = dc.observed;
            if let Some(diag) = build_tmux_death_diagnostic(session_name, Some(&output_path)) {
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux pane {observed:?} ({diag})",
                    session_name
                );
            } else {
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux pane {observed:?}",
                    session_name
                );
            }
            // Schedule DB cleanup + tmux kill for this dead session
            dead_cleanups.push(dc);
            continue;
        }

        // #148: Only register in owned_sessions after passing QUARANTINE + live-pane checks.
        // Earlier registration blocked new session creation for quarantined/dead channels.
        owned_sessions
            .entry(*channel_id)
            .or_insert_with(|| channel_name.clone());

        let mut restored_turn = None;
        let mut thread_parent = None;
        let initial_offset = if let Some(state) =
            super::super::super::inflight::load_inflight_state(&provider, channel_id.get())
        {
            thread_parent = thread_follow_up_parent_channel_id(
                *channel_id,
                state.logical_channel_id,
                state.thread_id,
            );
            if let Some(restored_tmux) =
                restored_watcher_turn_from_inflight(&state, session_name, false)
            {
                let rebound =
                    rebind_restored_dispatch_if_missing(shared.pg_pool.as_ref(), &state).await;
                if rebound == RestoreDispatchRebindOutcome::NotRebound
                    && consume_dispatched_origin_ghost_if_current(shared.pg_pool.as_ref(), &state)
                        .await
                {
                    tracing::info!(
                        channel_id = state.channel_id,
                        "cleared orphaned dispatched-origin turn during watcher restore"
                    );
                    continue;
                }
                let finish_mailbox_on_completion =
                    super::super::super::recovery::reregister_active_turn_from_inflight(
                        &shared, &state,
                    )
                    .await;
                restored_turn = Some(RestoredWatcherTurn {
                    finish_mailbox_on_completion,
                    ..restored_tmux
                });
                let file_len = std::fs::metadata(&output_path)
                    .map(|m| m.len())
                    .unwrap_or(0);
                if file_len >= state.last_offset {
                    state.last_offset
                } else {
                    0
                }
            } else {
                std::fs::metadata(&output_path)
                    .map(|m| m.len())
                    .unwrap_or(0)
            }
        } else {
            std::fs::metadata(&output_path)
                .map(|m| m.len())
                .unwrap_or(0)
        };

        pending.push(PendingWatcher {
            channel_id: *channel_id,
            output_path,
            session_name: session_name.to_string(),
            initial_offset,
            restored_turn,
            thread_parent,
            codex_direct_resume_fallback,
        });
        if let Some(path) = selected_claude_tui_fallback_transcript {
            restore_claimed_claude_tui_transcripts.insert(path);
        }
    }

    // Register sessions in CoreState so cleanup_orphan_tmux_sessions recognizes them
    // and message handlers find an active session with current_path
    if !owned_sessions.is_empty() {
        let mut data = shared.core.lock().await;
        for (channel_id, channel_name) in &owned_sessions {
            let persisted_path = load_last_session_path(
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                channel_id.get(),
            );
            let persisted_session_id = load_restored_provider_session_id(
                shared.pg_pool.as_ref(),
                &shared.token_hash,
                &provider,
                channel_name,
            );
            let configured_path = super::super::super::settings::resolve_workspace(
                *channel_id,
                Some(channel_name.as_str()),
            );
            let tmux_name = provider.build_tmux_session_name(channel_name);
            let session_keys = super::super::super::adk_session::build_session_key_candidates(
                &shared.token_hash,
                &provider,
                &tmux_name,
            );
            let db_cwd =
                load_restored_session_cwd(shared.pg_pool.as_ref(), &session_keys, channel_id.get());

            let session = data.sessions.entry(*channel_id).or_insert_with(|| {
                super::super::super::DiscordSession {
                    session_id: persisted_session_id.clone(),
                    memento_context_loaded:
                        super::super::super::session_runtime::restored_memento_context_loaded(
                            false,
                            None,
                            persisted_session_id.as_deref(),
                        ),
                    memento_reflected: false,
                    current_path: None,
                    history: Vec::new(),
                    pending_uploads: Vec::new(),
                    cleared: false,
                    channel_name: Some(channel_name.clone()),
                    category_name: None,
                    remote_profile_name: None,
                    channel_id: Some(channel_id.get()),

                    last_active: tokio::time::Instant::now(),
                    worktree: None,

                    born_generation: super::super::super::runtime_store::process_generation(),
                }
            });

            if session.session_id.is_none() && persisted_session_id.is_some() {
                session.restore_provider_session(persisted_session_id.clone());
            }

            // Restore current_path: DB cwd (worktree-aware) > last_sessions (yaml, main workspace)
            if session.current_path.is_none() {
                // #3219: prefer the channel's own reusable managed worktree over
                // the configured base; only log "ignoring" when it is NOT reused.
                let reusable_worktree =
                    super::super::super::session_runtime::db_cwd_is_reusable_worktree(
                        configured_path.as_deref(),
                        db_cwd.as_deref(),
                    );
                if let (Some(configured), Some(restored)) =
                    (configured_path.as_ref(), db_cwd.as_ref())
                {
                    if configured != restored && !reusable_worktree {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::info!(
                            "  [{ts}] ⚠ Ignoring restored DB cwd for channel {}: {} (configured workspace: {})",
                            channel_id,
                            restored,
                            configured
                        );
                    }
                }
                let effective_path = super::super::super::select_restored_session_path(
                    configured_path,
                    db_cwd,
                    persisted_path,
                    reusable_worktree,
                );
                if let Some(path) = effective_path {
                    session.current_path = Some(path);
                }
            }
        }
    }

    // Spawn watchers
    // #226: Use try_claim_watcher for atomic check-and-insert. The pending list
    // was built during the scan phase, which includes async Discord API calls.
    // A normal turn may have created a watcher in the meantime.
    for pw in pending {
        // #226: Skip channels that recovery already handled — their watchers may have
        // ended quickly (session died), removing themselves from the DashMap, but we
        // should not create a second watcher because recovery already processed the turn.
        let recovery_handled =
            recovery_handled_channel_exists(shared.as_ref(), pw.channel_id.get());
        if recovery_handled {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — recovery already handled this channel",
                pw.session_name
            );
            continue;
        }

        if pw.restored_turn.is_none() {
            reconcile_orphan_suppressed_placeholder_for_restored_watcher(
                http,
                shared,
                &provider,
                pw.channel_id,
                &pw.session_name,
            )
            .await;
        }

        let cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let paused = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let resume_offset = Arc::new(std::sync::Mutex::new(None::<u64>));
        let pause_epoch = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let turn_delivered = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let last_heartbeat_ts_ms = Arc::new(std::sync::atomic::AtomicI64::new(
            super::super::super::tmux_watcher_now_ms(),
        ));

        let handle = TmuxWatcherHandle {
            tmux_session_name: pw.session_name.clone(),
            output_path: pw.output_path.clone(),
            paused: paused.clone(),
            resume_offset: resume_offset.clone(),
            cancel: cancel.clone(),
            pause_epoch: pause_epoch.clone(),
            turn_delivered: turn_delivered.clone(),
            last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
        };
        let claimed = codex_restore::commit_live_direct_resume_fallback(
            &pw.session_name,
            pw.channel_id,
            pw.codex_direct_resume_fallback,
            || {
                try_claim_watcher_with_thread_parent(
                    &shared.tmux_watchers,
                    pw.channel_id,
                    handle,
                    Some(&provider),
                    pw.thread_parent,
                )
            },
        );
        if !claimed {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — already watching or source changed during scan",
                pw.session_name
            );
            continue;
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ Restoring tmux watcher for {} (offset {})",
            pw.session_name,
            pw.initial_offset
        );

        shared.record_tmux_watcher_reconnect(pw.channel_id);
        super::super::super::task_supervisor::spawn_observed_tmux_watcher(
            "watchers_lifecycle_tmux_output_watcher_with_restore",
            shared.clone(),
            pw.session_name.clone(),
            cancel.clone(),
            tmux_output_watcher_with_restore(
                pw.channel_id,
                http.clone(),
                shared.clone(),
                pw.output_path,
                pw.session_name,
                pw.initial_offset,
                cancel,
                paused,
                resume_offset,
                pause_epoch,
                turn_delivered,
                last_heartbeat_ts_ms,
                pw.restored_turn,
            ),
        );
    }

    // Clean up dead sessions: report idle to DB and kill tmux sessions
    if !dead_cleanups.is_empty() {
        let provider = shared.settings.read().await.provider.clone();
        let effects = StartupDeadSessionEffects {
            shared,
            provider: &provider,
        };

        let mut cleaned_dead_sessions = 0usize;
        for dc in &dead_cleanups {
            let (pool, token_hash) = (shared.pg_pool.as_ref(), shared.token_hash.as_str());
            if clean_dead_startup_session(pool, token_hash, &provider, dc, &effects).await {
                cleaned_dead_sessions += 1;
            }
        }

        if cleaned_dead_sessions > 0 {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 Cleaned {} dead tmux session(s) on startup",
                cleaned_dead_sessions
            );
        }

        // Sweep orphan session temp files (no matching tmux session AND
        // owner marker older than the threshold). Conservative: skip the
        // legacy /tmp directory (those files may still be held open by
        // pre-migration wrappers) — we only clean the new persistent
        // directory. See issue #892.
        sweep_orphan_session_files().await;
    }
}

/// A tmux session found not live at startup, with the pane liveness that found it.
struct DeadSessionCleanup {
    channel_id: u64,
    channel_name: String,
    session_name: String,
    observed: HostLiveness,
}

impl DeadSessionCleanup {
    /// A cleanup candidate unless the pane is live. A failed probe stays a candidate
    /// with its answer, so the host guard keeps the session instead of reading it as dead.
    async fn probe(channel_id: u64, channel_name: &str, session_name: &str) -> Option<Self> {
        let probe = crate::services::tmux_diagnostics::probe_tmux_session_pane_liveness;
        let observed = HostLiveness::from(probe(session_name).await);
        let marker = crate::services::tmux_common::session_dead_marker_path(session_name);
        if observed != HostLiveness::Live {
            let (channel_name, session_name) = (channel_name.into(), session_name.into());
            return Some(Self {
                channel_id,
                channel_name,
                session_name,
                observed,
            });
        }
        if std::path::Path::new(&marker).exists() {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] 🧹 clearing stale .pane_dead marker for {session_name} — tmux session is alive"
            );
            let _ = std::fs::remove_file(&marker);
        }
        None
    }
}

/// The dispatch failure and idle report a startup cleanup makes; tests record them.
trait DeadSessionEffects {
    fn dispatch_protection(&self, dc: &DeadSessionCleanup) -> Option<DispatchTmuxProtection>;
    async fn fail_dispatch(&self, protection: &DispatchTmuxProtection, name: &str) -> bool;
    async fn report_idle(&self, dc: &DeadSessionCleanup, session_key: &str);
}

struct StartupDeadSessionEffects<'a> {
    shared: &'a SharedData,
    provider: &'a ProviderKind,
}

impl DeadSessionEffects for StartupDeadSessionEffects<'_> {
    fn dispatch_protection(&self, dc: &DeadSessionCleanup) -> Option<DispatchTmuxProtection> {
        super::super::super::tmux_lifecycle::resolve_dispatch_tmux_protection(
            self.shared.pg_pool.as_ref(),
            &self.shared.token_hash,
            self.provider,
            &dc.session_name,
            Some(&dc.channel_name),
        )
    }

    async fn fail_dispatch(&self, protection: &DispatchTmuxProtection, name: &str) -> bool {
        let api_port = self.shared.api_port;
        super::super::super::tmux_lifecycle::fail_active_dispatch_for_dead_tmux_session(
            api_port,
            protection,
            name,
            "tmux_startup",
        )
        .await
    }

    async fn report_idle(&self, dc: &DeadSessionCleanup, session_key: &str) {
        let thread_channel_id =
            super::super::super::adk_session::parse_thread_channel_id_from_name(&dc.channel_name);
        let agent_id = resolve_role_binding(ChannelId::new(dc.channel_id), Some(&dc.channel_name))
            .map(|binding| binding.role_id);
        super::super::super::adk_session::post_adk_session_status(
            Some(session_key),
            Some(&dc.channel_name),
            None,
            "idle",
            self.provider,
            None,
            None,
            None,
            None,
            thread_channel_id,
            Some(ChannelId::new(dc.channel_id)),
            agent_id.as_deref(),
            self.shared.api_port,
        )
        .await;
    }
}

/// Cleans one session found dead at startup. The host guard reads the rows as stored
/// before the dispatch failure, the idle report or the kill; a refusal skips all three.
async fn clean_dead_startup_session(
    pool: Option<&sqlx::PgPool>,
    token_hash: &str,
    provider: &ProviderKind,
    dc: &DeadSessionCleanup,
    effects: &impl DeadSessionEffects,
) -> bool {
    let tmux_name = provider.build_tmux_session_name(&dc.channel_name);
    let session_key = super::super::super::adk_session::build_namespaced_session_key(
        token_hash, provider, &tmux_name,
    );
    let (key, name) = (Some(session_key.as_str()), dc.session_name.as_str());
    let caller = "startup_dead_session";
    let teardown = crate::services::discord::inflight::keyed_teardown(
        pool,
        provider,
        dc.channel_id,
        key,
        name,
        Some(dc.observed),
        caller,
    );
    let teardown = teardown.await;
    if matches!(teardown, KeyedTeardown::Kept) {
        return false;
    }
    let dispatch_protection = effects.dispatch_protection(dc);
    let dispatch_failed_for_dead_session = match dispatch_protection.as_ref() {
        Some(protection) => effects.fail_dispatch(protection, name).await,
        None => false,
    };
    let cleanup_plan = dead_session_cleanup_plan(
        dispatch_protection.is_some() && !dispatch_failed_for_dead_session,
    );

    if let Some(protection) = dispatch_protection {
        let ts = chrono::Local::now().format("%H:%M:%S");
        if dispatch_failed_for_dead_session {
            tracing::warn!(
                "  [{ts}] tmux startup: failed active dispatch for dead session {} — {}",
                name,
                protection.log_reason()
            );
        } else {
            tracing::info!(
                "  [{ts}] ♻ tmux startup: preserving dispatch session {} — {}",
                name,
                protection.log_reason()
            );
        }
    }

    if cleanup_plan.report_idle_status {
        effects.report_idle(dc, &session_key).await;
    }
    if cleanup_plan.preserve_tmux_session {
        return false;
    }

    // Kill the dead tmux session; one with no sessions row keeps main's name-only audit.
    let name = name.to_string();
    let _ = tokio::task::spawn_blocking(move || {
        let reason = "startup cleanup: dead session";
        let (component, code) = ("tmux_startup", "startup_dead_session");
        use crate::services::termination_audit as audit;
        match &teardown {
            KeyedTeardown::Cleared(session) => audit::record_termination_for_cleared(
                session,
                None,
                component,
                code,
                Some(reason),
                None,
            ),
            KeyedTeardown::RowMissing | KeyedTeardown::Kept => {
                audit::record_termination_for_tmux(&name, None, component, code, Some(reason), None)
            }
        }
        record_tmux_exit_reason(&name, reason);
        crate::services::platform::tmux::kill_session(&name, reason);
    })
    .await;
    true
}

pub(super) fn add_configured_channel_bindings(
    agent_sessions: &[&str],
    provider: &ProviderKind,
    configured_bindings: impl IntoIterator<
        Item = super::super::super::settings::RegisteredChannelBinding,
    >,
    name_to_channel: &mut std::collections::HashMap<String, (ChannelId, String)>,
) {
    for binding in configured_bindings {
        if binding.owner_provider != *provider {
            continue;
        }
        let Some(channel_name) = binding
            .fallback_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
        else {
            continue;
        };
        let tmux_name = provider.build_tmux_session_name(channel_name);
        if agent_sessions.iter().any(|session| **session == tmux_name) {
            name_to_channel
                .entry(tmux_name)
                .or_insert_with(|| (ChannelId::new(binding.channel_id), channel_name.to_string()));
        }
    }
}

#[cfg(test)]
mod keyed_teardown_tests {
    use std::sync::Mutex;

    use super::*;
    use crate::db::dispatched_sessions::hosted_execution::HostedState;
    use crate::db::dispatched_sessions::hosted_execution::tests::{TOKEN, owner, record, wire};
    use crate::services::discord::adk_session::build_namespaced_session_key;
    use crate::services::discord::inflight::seed_session_row_keyed;
    use crate::services::platform::tmux::PaneLiveness::{DeadOrAbsent, ProbeError};
    use crate::services::tmux_diagnostics::PaneLivenessOverrideGuard;

    /// Protects every session with an active dispatch and records what main would change.
    #[derive(Default)]
    struct Recorded(Mutex<Vec<&'static str>>);

    impl DeadSessionEffects for Recorded {
        fn dispatch_protection(&self, _dc: &DeadSessionCleanup) -> Option<DispatchTmuxProtection> {
            Some(DispatchTmuxProtection::SessionRow {
                dispatch_id: "p4c3w1-dispatch".to_string(),
                session_status: "turn_active".to_string(),
                dispatch_status: "dispatched".to_string(),
            })
        }

        async fn fail_dispatch(&self, _protection: &DispatchTmuxProtection, _name: &str) -> bool {
            self.0.lock().unwrap().push("fail_dispatch");
            true
        }

        async fn report_idle(&self, _dc: &DeadSessionCleanup, _session_key: &str) {
            self.0.lock().unwrap().push("idle");
        }
    }

    // A session found not live at startup: the guard reads the rows as stored before the
    // dispatch failure, the idle report or the kill, and a refusal skips all three.
    #[tokio::test]
    async fn startup_cleanup_runs_only_after_the_host_guard_admits_the_stored_rows_pg() {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let claude = ProviderKind::Claude;
        let channel = |n: u64| 1_479_671_301_387_059_600 + n;
        let tmux = |channel_name: &str| claude.build_tmux_session_name(channel_name);
        let key =
            |channel_name: &str| build_namespaced_session_key(TOKEN, &claude, &tmux(channel_name));
        let bound = wire(&record(
            &owner(&channel(2).to_string()),
            "n1",
            HostedState::Bound,
        ));
        for (channel_name, n, raw) in [
            ("p4c3w1-start-legacy", 1, None),
            ("p4c3w1-start-bound", 2, Some(bound)),
            ("p4c3w1-start-probe", 3, None),
        ] {
            seed_session_row_keyed(&pool, &key(channel_name), channel(n), raw).await;
        }
        let marker = crate::services::tmux_common::session_temp_path(
            &tmux("p4c3w1-start-missing-herdr"),
            "host_kind",
        );
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, "herdr").unwrap();

        // (channel name, channel, pane liveness, cleaned in main's order)
        let cases = [
            ("p4c3w1-start-legacy", 1, DeadOrAbsent, true),
            ("p4c3w1-start-bound", 2, DeadOrAbsent, false),
            ("p4c3w1-start-probe", 3, ProbeError, false),
            // No row before the idle report: main's name-only cleanup, unless a trace says otherwise.
            ("p4c3w1-start-missing", 4, DeadOrAbsent, true),
            ("p4c3w1-start-missing-herdr", 5, DeadOrAbsent, false),
            ("p4c3w1-start-missing-probe", 6, ProbeError, false),
        ];
        for (channel_name, n, liveness, cleaned) in cases {
            let session_name = tmux(channel_name);
            let _pane = PaneLivenessOverrideGuard::set(&session_name, liveness);
            let dc = DeadSessionCleanup::probe(channel(n), channel_name, &session_name);
            let dc = dc.await.expect("a pane that is not live is a candidate");
            let effects = Recorded::default();
            let done = clean_dead_startup_session(Some(&pool), TOKEN, &claude, &dc, &effects);
            assert_eq!(done.await, cleaned, "{channel_name}");
            let changed: &[&str] = if cleaned {
                &["fail_dispatch", "idle"]
            } else {
                &[]
            };
            assert_eq!(*effects.0.lock().unwrap(), changed, "{channel_name}");
            let exit_reason =
                crate::services::tmux_common::session_temp_path(&session_name, "exit_reason");
            let killed = std::path::Path::new(&exit_reason).exists();
            assert_eq!(killed, cleaned, "{channel_name}");
        }
        pool.close().await;
        db.drop().await;
    }
}
