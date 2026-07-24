use super::*;

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
    }

    // Collect sessions to restore
    struct PendingWatcher {
        channel_id: ChannelId,
        output_path: String,
        session_name: String,
        initial_offset: u64,
        restored_turn: Option<RestoredWatcherTurn>,
        codex_direct_resume_fallback: Option<codex_restore::DirectResumeFallback>,
    }

    // Dead sessions that need DB cleanup (idle status report + tmux kill)
    struct DeadSessionCleanup {
        channel_id: u64,
        channel_name: String,
        session_name: String,
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
            super::super::resolve_thread_parent(http, *channel_id).await
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

        if let Some(started) = super::super::mailbox_snapshot(&shared, *channel_id)
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
                super::super::mailbox_clear_recovery_marker(&shared, *channel_id).await;
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
                super::super::mailbox_clear_recovery_marker(&shared, *channel_id).await;
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
        let configured_workspace =
            super::super::settings::resolve_workspace(*channel_id, Some(channel_name.as_str()));
        let session_keys = super::super::adk_session::build_session_key_candidates(
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
        let current_gen = super::super::runtime_store::process_generation();
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
            // Update generation marker to current gen, preserving the
            // existing mtime.
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
            preserve_mtime_after_write(
                &gen_marker_path,
                current_gen.to_string().as_bytes(),
                "adoption_marker_rewrite",
            );
        }

        if !probe_tmux_session_liveness(session_name).await {
            let ts = chrono::Local::now().format("%H:%M:%S");
            if let Some(diag) = build_tmux_death_diagnostic(session_name, Some(&output_path)) {
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux pane dead ({diag})",
                    session_name
                );
            } else {
                tracing::info!(
                    "  [{ts}] ⏭ watcher skip for {} — tmux pane dead",
                    session_name
                );
            }
            // Schedule DB cleanup + tmux kill for this dead session
            dead_cleanups.push(DeadSessionCleanup {
                channel_id: channel_id.get(),
                channel_name: channel_name.clone(),
                session_name: session_name.to_string(),
            });
            continue;
        }

        // #148: Only register in owned_sessions after passing QUARANTINE + live-pane checks.
        // Earlier registration blocked new session creation for quarantined/dead channels.
        owned_sessions
            .entry(*channel_id)
            .or_insert_with(|| channel_name.clone());

        let mut restored_turn = None;
        let initial_offset = if let Some(state) =
            super::super::inflight::load_inflight_state(&provider, channel_id.get())
        {
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
                    super::super::recovery::reregister_active_turn_from_inflight(&shared, &state)
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
            let configured_path =
                super::super::settings::resolve_workspace(*channel_id, Some(channel_name.as_str()));
            let tmux_name = provider.build_tmux_session_name(channel_name);
            let session_keys = super::super::adk_session::build_session_key_candidates(
                &shared.token_hash,
                &provider,
                &tmux_name,
            );
            let db_cwd =
                load_restored_session_cwd(shared.pg_pool.as_ref(), &session_keys, channel_id.get());

            let session =
                data.sessions
                    .entry(*channel_id)
                    .or_insert_with(|| super::super::DiscordSession {
                        session_id: persisted_session_id.clone(),
                        memento_context_loaded:
                            super::super::session_runtime::restored_memento_context_loaded(
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

                        born_generation: super::super::runtime_store::process_generation(),
                    });

            if session.session_id.is_none() && persisted_session_id.is_some() {
                session.restore_provider_session(persisted_session_id.clone());
            }

            // Restore current_path: DB cwd (worktree-aware) > last_sessions (yaml, main workspace)
            if session.current_path.is_none() {
                // #3219: prefer the channel's own reusable managed worktree over
                // the configured base; only log "ignoring" when it is NOT reused.
                let reusable_worktree = super::super::session_runtime::db_cwd_is_reusable_worktree(
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
                let effective_path = super::super::select_restored_session_path(
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
            super::super::tmux_watcher_now_ms(),
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
        if !try_claim_watcher(&shared.tmux_watchers, pw.channel_id, handle) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ watcher skip for {} — already watching (created during scan)",
                pw.session_name
            );
            continue;
        }
        if let Some(fallback) = pw.codex_direct_resume_fallback {
            codex_restore::commit_live_direct_resume_fallback(
                &pw.session_name,
                pw.channel_id,
                fallback,
            );
        }

        let ts = chrono::Local::now().format("%H:%M:%S");
        tracing::info!(
            "  [{ts}] ↻ Restoring tmux watcher for {} (offset {})",
            pw.session_name,
            pw.initial_offset
        );

        shared.record_tmux_watcher_reconnect(pw.channel_id);
        super::super::task_supervisor::spawn_observed_tmux_watcher(
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
        let api_port = shared.api_port;
        let provider = shared.settings.read().await.provider.clone();

        let mut cleaned_dead_sessions = 0usize;
        for dc in &dead_cleanups {
            let dispatch_protection =
                super::super::tmux_lifecycle::resolve_dispatch_tmux_protection(
                    shared.pg_pool.as_ref(),
                    &shared.token_hash,
                    &provider,
                    &dc.session_name,
                    Some(&dc.channel_name),
                );
            let dispatch_failed_for_dead_session =
                if let Some(protection) = dispatch_protection.as_ref() {
                    super::super::tmux_lifecycle::fail_active_dispatch_for_dead_tmux_session(
                        api_port,
                        protection,
                        &dc.session_name,
                        "tmux_startup",
                    )
                    .await
                } else {
                    false
                };
            let cleanup_plan = dead_session_cleanup_plan(
                dispatch_protection.is_some() && !dispatch_failed_for_dead_session,
            );

            if let Some(protection) = dispatch_protection {
                let ts = chrono::Local::now().format("%H:%M:%S");
                if dispatch_failed_for_dead_session {
                    tracing::warn!(
                        "  [{ts}] tmux startup: failed active dispatch for dead session {} — {}",
                        dc.session_name,
                        protection.log_reason()
                    );
                } else {
                    tracing::info!(
                        "  [{ts}] ♻ tmux startup: preserving dispatch session {} — {}",
                        dc.session_name,
                        protection.log_reason()
                    );
                }
            }

            let tmux_name = provider.build_tmux_session_name(&dc.channel_name);
            let thread_channel_id =
                super::super::adk_session::parse_thread_channel_id_from_name(&dc.channel_name);
            let session_key = super::super::adk_session::build_namespaced_session_key(
                &shared.token_hash,
                &provider,
                &tmux_name,
            );
            let agent_id =
                resolve_role_binding(ChannelId::new(dc.channel_id), Some(&dc.channel_name))
                    .map(|binding| binding.role_id);

            if cleanup_plan.report_idle_status {
                super::super::adk_session::post_adk_session_status(
                    Some(&session_key),
                    Some(&dc.channel_name),
                    None,
                    "idle",
                    &provider,
                    None,
                    None,
                    None,
                    None,
                    thread_channel_id,
                    Some(ChannelId::new(dc.channel_id)),
                    agent_id.as_deref(),
                    api_port,
                )
                .await;
            }

            if cleanup_plan.preserve_tmux_session {
                continue;
            }

            // Kill the dead tmux session
            let sess = dc.session_name.clone();
            let _ = tokio::task::spawn_blocking(move || {
                crate::services::termination_audit::record_termination_for_tmux(
                    &sess,
                    None,
                    "tmux_startup",
                    "startup_dead_session",
                    Some("startup cleanup: dead session"),
                    None,
                );
                record_tmux_exit_reason(&sess, "startup cleanup: dead session");
                crate::services::platform::tmux::kill_session(
                    &sess,
                    "startup cleanup: dead session",
                );
            })
            .await;
            cleaned_dead_sessions += 1;
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
