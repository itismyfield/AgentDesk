//! Restart-path inflight recovery scan (#3834 decompose split).
//!
//! Behavior-preserving extraction from `recovery_engine.rs`: retry-aware tmux
//! probes, recovered-turn mailbox finalization, live output-path detection, and
//! the restart `restore_inflight_turns` scan/reattach/session-retry path. The
//! root facade re-exports the public entry points so existing external paths stay
//! stable. Moved verbatim except for module-local path qualification required by
//! the new child-module boundary.

use super::terminal_watcher::restart_report_watcher_start;
use super::*;

/// Retry-aware tmux session check for recovery after dcserver restart.
/// The first check can false-negative if tmux CLI hasn't fully initialized yet.
pub(super) fn tmux_session_alive_with_retry(name: &str) -> bool {
    if tmux_session_has_live_pane(name) {
        return true;
    }
    // #2428 H5: retry up to 2 more times with exponential backoff + jitter
    // (was a fixed 1s gap; see `recovery_retry_backoff`).
    for attempt in 1..=2u32 {
        std::thread::sleep(recovery_retry_backoff(attempt));
        if tmux_session_has_live_pane(name) {
            tracing::info!(
                "  [recovery] tmux pane alive on retry {} for {}",
                attempt,
                name
            );
            return true;
        }
    }
    false
}

/// Retry-aware tmux has_session check.
fn tmux_has_session_with_retry(name: &str) -> bool {
    if crate::services::platform::tmux::has_session(name) {
        return true;
    }
    // #2428 H5: see `recovery_retry_backoff`.
    for attempt in 1..=2u32 {
        std::thread::sleep(recovery_retry_backoff(attempt));
        if crate::services::platform::tmux::has_session(name) {
            tracing::info!(
                "  [recovery] tmux session found on retry {} for {}",
                attempt,
                name
            );
            return true;
        }
    }
    false
}

#[cfg(not(unix))]
fn build_tmux_death_diagnostic(_name: &str, _output_path: Option<&str>) -> Option<String> {
    None
}

pub(in crate::services::discord) async fn finish_recovered_turn_mailbox(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    stop_source: &'static str,
) {
    // #3016 phase 4: route the recovery terminal through the single-authority
    // finalizer. The recovered turn is channel-scoped here (the caller did not
    // thread its real `user_msg_id`), so we submit `user_msg_id == 0` — the
    // finalizer resolves it to the channel's single live entry (or finalizes
    // the orphan directly) and runs the SAME channel-scoped `mailbox_finish_turn`
    // + gated counter decrement + watchdog-override clear + dispatch_thread_parents
    // retain + role-override cleanup + queue kickoff this code did inline. The
    // ledger phase gate keeps a racing watcher/bridge terminal exactly-once safe.
    // `FinalizeContext::monitor` reproduces the inline side-effect set (no
    // inflight clear, no completion-cleanup, no voice drain, kick off backlog).
    //
    // Recovery is single-turn-per-channel (the channel is being recovered, not
    // running a fresh turn), so id-0 here is safe: the finalizer's id-0 guard
    // makes an AMBIGUOUS submission (a recently-Finalized entry AND a different
    // live turn) a NO-OP — it never releases a newer turn's token — and the
    // unambiguous case (the recovered turn is the single live entry) finalizes
    // it exactly as the inline code did. This reproduces the prior
    // channel-scoped `mailbox_finish_turn` semantics, now ledger-gated.
    let _ = shared
        .turn_finalizer
        .submit_terminal(
            super::turn_finalizer::TurnKey::new(channel_id, 0, shared.restart.current_generation),
            provider.clone(),
            super::turn_finalizer::TerminalEvent::Complete,
            super::turn_finalizer::FinalizeContext::monitor(),
            shared.clone(),
        )
        .await;
    let _ = stop_source;
}

#[cfg(unix)]
fn tmux_pane_pid(tmux_session_name: &str) -> Option<u32> {
    let mut cmd = Command::new("tmux");
    binary_resolver::apply_runtime_path(&mut cmd);
    let output = cmd
        .args([
            "display-message",
            "-p",
            "-t",
            &tmux_exact_target(tmux_session_name),
            "#{pane_pid}",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()
}

#[cfg(unix)]
pub(super) fn detect_live_tmux_output_path(
    tmux_session_name: &str,
    fallback_path: &str,
) -> Result<Option<DetectedRebindOutputPath>, StaleOutputCandidate> {
    let Some(pane_pid) = tmux_pane_pid(tmux_session_name) else {
        return Ok(None);
    };
    let mut cmd = Command::new("lsof");
    binary_resolver::apply_runtime_path(&mut cmd);
    let output = match cmd.args(["-Fn", "-p", &pane_pid.to_string()]).output() {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    if !output.status.success() {
        return Ok(None);
    }
    let stdout = match String::from_utf8(output.stdout) {
        Ok(stdout) => stdout,
        Err(_) => return Ok(None),
    };
    let candidates = parse_lsof_output_candidates(&stdout);
    detect_rebind_output_path_from_candidates(fallback_path, candidates)
}

pub(in crate::services::discord) async fn restore_inflight_turns(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    let states = load_inflight_states(provider);
    if states.is_empty() {
        return;
    }

    let settings_snapshot = shared.settings.read().await.clone();

    for state in states {
        // #897 round-4 High: rebind_origin inflights are synthetic
        // placeholders owned by `/api/inflight/rebind` and do NOT carry
        // a real user message, dispatch context, or placeholder Discord
        // message. Restart recovery has nothing meaningful to do with
        // them — running `replace_long_message_raw(msg_id=0)`, writing
        // `discord:<channel>:0` analytics rows, or emitting reactions
        // against `MessageId::new(0)` would all produce bogus state
        // (flagged by #897 round-4 review). The operator is expected to
        // re-invoke `/api/inflight/rebind` after dcserver comes back up
        // if the orphan tmux is still alive. Clear the stale state and
        // skip further processing for this entry.
        if state.rebind_origin {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⏭ recovery: skipping rebind-origin inflight for channel {} — operator must re-invoke /api/inflight/rebind post-restart",
                state.channel_id
            );
            clear_inflight_state(provider, state.channel_id);
            continue;
        }

        let Some(channel_id) = super::inflight::opt_channel_id(state.channel_id) else {
            tracing::warn!(
                provider = %provider.as_str(),
                "inflight recovery skipped because persisted channel id is zero"
            );
            continue;
        };

        // #2235: silent-skip rows whose on-disk `runtime_kind` was a
        // present-but-unknown variant string. `load_inflight_states_from_root`
        // distinguishes this from "field absent" (legacy v7 rows) via the
        // transient `runtime_kind_unknown_on_disk` flag, so the existing
        // heuristic recovery path still runs for absent-field legacy rows.
        // Belt-and-suspenders: also silent-skip when a row's persisted
        // `version` is ahead of this binary and `runtime_kind` is missing —
        // forward-marked rows authored by a newer binary should not be
        // guessed at.
        let runtime_kind_skew_detected = state.runtime_kind_unknown_on_disk
            || (state.runtime_kind.is_none()
                && state.version > super::inflight::inflight_state_version());
        if runtime_kind_skew_detected {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::debug!(
                "  [{ts}] ↩ inflight recovery silent-skip for channel {}: runtime_kind unknown/forward-marked (version={}, local={}, unknown_on_disk={})",
                state.channel_id,
                state.version,
                super::inflight::inflight_state_version(),
                state.runtime_kind_unknown_on_disk
            );
            finish_recovered_turn_mailbox(
                shared,
                provider,
                channel_id,
                "recovery_runtime_kind_unknown_skip",
            )
            .await;
            clear_inflight_state(provider, state.channel_id);
            continue;
        }
        let is_dm = matches!(
            channel_id.to_channel(http).await,
            Ok(serenity::model::channel::Channel::Private(_))
        );
        let restart_report_exists =
            super::restart_report::load_restart_report(provider, state.channel_id).is_some();
        // #3562: derive the turn identity and agent role so the recovery_fired
        // observability events back-trace to a specific agent/turn. `turn_id`
        // matches the recovered-transcript key (joins turn_started/turn_finished
        // for message-bearing turns); `agent_id` is the role bound to the channel.
        let recovery_turn_id = super::analytics_transcript::recovered_transcript_turn_id(
            state.channel_id,
            state.user_msg_id,
            state.session_key.as_deref(),
            state.turn_start_offset,
            &state.started_at,
        );
        let recovery_agent_id =
            resolve_role_binding(channel_id, state.channel_name.as_deref()).map(|b| b.role_id);
        let recovery_reason = if restart_report_exists {
            "restart_report"
        } else {
            "restore_inflight"
        };
        crate::services::observability::emit_recovery_fired(
            provider.as_str(),
            state.channel_id,
            state.dispatch_id.as_deref(),
            state.session_key.as_deref(),
            Some(recovery_turn_id.as_str()),
            recovery_agent_id.as_deref(),
            recovery_reason,
        );
        emit_recovery_quality_event(
            provider,
            state.channel_id,
            state.dispatch_id.as_deref(),
            state.session_key.as_deref(),
            Some(recovery_turn_id.as_str()),
            recovery_agent_id.as_deref(),
            recovery_reason,
        );

        // No generation gate — adopt mode allows old-gen session recovery. If a
        // restart report exists, check whether the agent already finished before
        // skipping recovery: a completed result is delivered directly and clears
        // both the inflight state and the restart report, so the flush loop won't
        // overwrite the message with a generic follow-up.
        if restart_report_exists {
            let output_path_for_check: Option<String> = state
                .output_path
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    state
                        .channel_name
                        .as_ref()
                        .map(|name| tmux_runtime_paths(&provider.build_tmux_session_name(name)).0)
                });
            let restart_tmux_name = recovery_tmux_session_name(provider, &state);
            let completed_during_downtime_end = output_path_for_check
                .as_deref()
                .and_then(|path| success_result_end_offset_after_offset(path, state.last_offset));
            let completed_during_downtime = completed_during_downtime_end.is_some();
            let completed_during_downtime_drained = match (
                output_path_for_check.as_deref(),
                completed_during_downtime_end,
            ) {
                (Some(path), Some(confirmed_end)) => {
                    terminal_success_output_drained_for_recovery(
                        path,
                        confirmed_end,
                        restart_tmux_name.as_deref(),
                    )
                    .await
                }
                (None, Some(_)) => true,
                _ => false,
            };

            if completed_during_downtime && !completed_during_downtime_drained {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ recovery: terminal success observed for channel {} but tmux output has not stayed drained; reattaching watcher",
                    state.channel_id
                );
            }

            if completed_during_downtime_drained {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ✓ recovering completed turn for channel {} (restart report exists but output has result)",
                    state.channel_id
                );
                let (recovered_session_id, recovered_usage) = output_path_for_check
                    .as_deref()
                    .map(|path| {
                        extract_turn_analytics_from_output(
                            path,
                            state.turn_start_offset.unwrap_or(state.last_offset),
                        )
                    })
                    .unwrap_or((None, None));
                let extracted = output_path_for_check
                    .as_deref()
                    .map(|p| extract_response_from_output(p, state.last_offset))
                    .unwrap_or_default();
                let assistant_response = if extracted.trim().is_empty() {
                    state.full_response.clone()
                } else {
                    extracted
                };
                let final_text = if assistant_response.trim().is_empty() {
                    "(복구됨 — 응답 텍스트 없음)".to_string()
                } else {
                    super::formatting::format_for_discord_with_provider(
                        &assistant_response,
                        provider,
                    )
                };
                // An un-anchored TUI-direct/recovery turn (current_msg_id == 0)
                // delivers the recovered text as a NEW channel message, not an
                // in-place edit (the helper handles both); `relay_ok` still
                // reflects actual delivery so recovery never advances without
                // posting. `MessageId::new(0)` would panic.
                let recovery_context = RecoveryDeliveryContext::from_state(
                    provider,
                    &state,
                    completed_during_downtime_end
                        .map(|confirmed_end| (state.last_offset, confirmed_end)),
                    shared.restart.current_generation,
                );
                let relay_ok = relay_recovered_terminal_text_to_placeholder(
                    http,
                    shared,
                    channel_id,
                    optional_message_id(state.current_msg_id),
                    &final_text,
                    recovery_context.as_ref(),
                )
                .await
                .delivered();
                if !should_advance_recovery_dispatch_after_relay(relay_ok) {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ recovery: Discord relay failed before downtime dispatch completion — preserving inflight for retry"
                    );
                    continue;
                }
                // Mark the user message completed only after terminal delivery
                // commits (else the channel shows completion without the final
                // message). A message-less turn (user_msg_id == 0) has no analytics
                // row to key (`discord:<ch>:0` is bogus) and `MessageId::new(0)`
                // panics; the transcript persist below stays unconditional.
                let user_msg_id = optional_message_id(state.user_msg_id);
                let visible_outcome = complete_recovery_visible_turn(
                    http,
                    shared,
                    provider,
                    &state,
                    false,
                    "completed_during_downtime",
                )
                .await;
                if !visible_outcome.should_proceed() {
                    // Reserved for future non-proceeding recovery outcomes.
                    // A TUI quiescence timeout is not one: terminal delivery
                    // evidence is authoritative for mailbox/inflight cleanup.
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        provider = %provider.as_str(),
                        channel_id = channel_id.get(),
                        "[{ts}] ⚠ recovery (completed_during_downtime) deferred by non-proceeding visible outcome"
                    );
                    continue;
                }
                // Complete the dispatch if this was a work dispatch turn — the
                // normal completion path was lost when dcserver restarted.
                // #142: implementation/rework need explicit completion. Review
                // and review-decision stay pending until their API handlers run.
                // Parse saved DISPATCH evidence first; reused threads may
                // already have a newer pending dispatch on the same thread.
                let recovered_dispatch_id = parse_dispatch_id(&state.user_text).or(
                    lookup_pending_dispatch_for_thread(shared.api_port, state.channel_id).await,
                );
                let role_binding = resolve_role_binding(channel_id, state.channel_name.as_deref());
                let duration_ms =
                    recovered_turn_duration_ms(Some(state.started_at.as_str())).unwrap_or(0);
                let has_completion_evidence = if shared.pg_pool.is_some() {
                    if let Some(user_msg_id) = user_msg_id {
                        super::turn_bridge::persist_turn_analytics_row_with_handles(
                            shared.pg_pool.as_ref(),
                            provider,
                            channel_id,
                            user_msg_id,
                            role_binding.as_ref(),
                            recovered_dispatch_id
                                .as_deref()
                                .or(state.dispatch_id.as_deref()),
                            state.session_key.as_deref(),
                            recovered_session_id
                                .as_deref()
                                .or(state.session_id.as_deref()),
                            &state,
                            recovered_usage.unwrap_or_default(),
                            duration_ms,
                        );
                    }
                    persist_recovered_transcript(
                        shared.pg_pool.as_ref(),
                        provider,
                        &state,
                        recovered_dispatch_id
                            .as_deref()
                            .or(state.dispatch_id.as_deref()),
                        &assistant_response,
                    )
                    .await
                } else {
                    !assistant_response.trim().is_empty()
                };
                let completion_context = has_completion_evidence
                    .then(|| serde_json::json!({ "agent_response_present": true }));
                let fallback_result = completion_context
                    .clone()
                    .map(|mut result| {
                        if let Some(obj) = result.as_object_mut() {
                            obj.insert(
                                "completion_source".to_string(),
                                serde_json::Value::String("recovery_db_fallback".to_string()),
                            );
                            obj.insert(
                                "needs_reconcile".to_string(),
                                serde_json::Value::Bool(true),
                            );
                        }
                        result
                    })
                    .unwrap_or_else(|| {
                        serde_json::json!({
                            "completion_source": "recovery_db_fallback",
                            "needs_reconcile": true,
                        })
                    });
                let mut dispatch_completed = recovered_dispatch_id.is_none();
                if let Some(ref did) = recovered_dispatch_id {
                    if !has_completion_evidence {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ recovery: refusing to complete work dispatch {did} without assistant response"
                        );
                    } else if let Some(engine) = &shared.policy.engine {
                        // #143: Use finalize_dispatch directly with retry.
                        for attempt in 1..=3u8 {
                            match crate::dispatch::finalize_dispatch_with_backends(
                                engine,
                                did,
                                "recovery_completed_during_downtime",
                                completion_context.as_ref(),
                            ) {
                                Ok(_) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        "  [{ts}] ✓ recovery: completed dispatch {did} via finalize_dispatch"
                                    );
                                    let _ =
                                        super::turn_bridge::queue_dispatch_followup_with_handles(
                                            shared.pg_pool.as_ref(),
                                            did,
                                            "recovery_completed_during_downtime",
                                        )
                                        .await;
                                    dispatch_completed = true;
                                    break;
                                }
                                Err(e) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::warn!(
                                        "  [{ts}] ⚠ recovery: finalize_dispatch failed for {did} (attempt {attempt}/3): {e}"
                                    );
                                    if attempt < 3 {
                                        // #2428 H5: exponential backoff + jitter.
                                        tokio::time::sleep(recovery_retry_backoff(u32::from(
                                            attempt,
                                        )))
                                        .await;
                                    }
                                }
                            }
                        }
                        // All retries exhausted — use the canonical runtime-root
                        // Postgres fallback instead of mutating legacy SQLite state.
                        if !dispatch_completed {
                            dispatch_completed =
                                super::turn_bridge::runtime_db_fallback_complete_with_result(
                                    did,
                                    &fallback_result,
                                );
                            if dispatch_completed {
                                let _ = super::turn_bridge::queue_dispatch_followup_with_handles(
                                    shared.pg_pool.as_ref(),
                                    did,
                                    "recovery_completed_during_downtime_fallback",
                                )
                                .await;
                            }
                        }
                    } else {
                        // Db/Engine not available — fall back to direct dispatch update with retry
                        let payload = crate::services::dispatches::UpdateDispatchBody {
                                status: Some("completed".to_string()),
                                result: Some(completion_context.clone().map(|mut result| {
                                    if let Some(obj) = result.as_object_mut() {
                                        obj.insert(
                                            "completion_source".to_string(),
                                            serde_json::Value::String(
                                                "recovery_completed_during_downtime".to_string(),
                                            ),
                                        );
                                    }
                                    result
                                }).unwrap_or_else(|| {
                                    serde_json::json!({
                                        "completion_source": "recovery_completed_during_downtime"
                                    })
                                })),
                                allowed_from: None,
                            };
                        use super::internal_api::DispatchUpdateOutcome;
                        let mut already_terminal = false;
                        for attempt in 1..=3u8 {
                            match super::internal_api::update_dispatch(did, payload.clone()).await {
                                Ok(DispatchUpdateOutcome::Updated(_)) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!("  [{ts}] ✓ recovery: completed dispatch {did}");
                                    dispatch_completed = true;
                                    break;
                                }
                                Ok(DispatchUpdateOutcome::Conflict { body }) => {
                                    // #2194 follow-up: dispatch is already in a
                                    // terminal status. Treat as success — do NOT
                                    // run DB fallback, which would overwrite the
                                    // existing result.
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::info!(
                                        dispatch_id = %did,
                                        response = %body,
                                        "  [{ts}] ✓ recovery: dispatch {did} already terminal (409); leaving prior result intact"
                                    );
                                    dispatch_completed = true;
                                    already_terminal = true;
                                    break;
                                }
                                Err(err) => {
                                    let ts = chrono::Local::now().format("%H:%M:%S");
                                    tracing::warn!(
                                        "  [{ts}] ⚠ recovery: dispatch {did} completion failed (attempt {attempt}/3): {err}"
                                    );
                                }
                            }
                            if attempt < 3 {
                                // #2428 H5: exponential backoff + jitter.
                                tokio::time::sleep(recovery_retry_backoff(u32::from(attempt)))
                                    .await;
                            }
                        }
                        // API retries exhausted — runtime-root DB fallback.
                        // Skip when the dispatch was already terminal (409) so we
                        // don't clobber its preserved result.
                        if !dispatch_completed && !already_terminal {
                            dispatch_completed =
                                super::turn_bridge::runtime_db_fallback_complete_with_result(
                                    did,
                                    &fallback_result,
                                );
                        }
                    }
                }
                // Only clear recovery bookkeeping if dispatch was completed (or no dispatch).
                // Preserving state on failure allows the next recovery pass to retry.
                if dispatch_completed {
                    super::restart_report::clear_restart_report(provider, state.channel_id);
                    finish_recovered_turn_mailbox(
                        shared,
                        provider,
                        channel_id,
                        "recovery_completed_during_downtime",
                    )
                    .await;
                    clear_inflight_state(provider, state.channel_id);
                } else if let Some(ref did) = recovered_dispatch_id {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ❌ recovery: dispatch {did} completion failed — preserving state for next recovery pass"
                    );
                }
                continue;
            }

            // Agent may still be running.  If the tmux session is alive, clear
            // the restart report and fall through to normal recovery (which
            // re-attaches a watcher to pick up the remaining output).
            // If the session is dead, delegate to the flush loop for fallback.
            let tmux_name = restart_tmux_name;
            let session_alive = tmux_name
                .as_deref()
                .map_or(false, tmux_session_alive_with_retry);
            // Derive channel_name from tmux session name if not in inflight state.
            // Validate before mutating restart-report state so other same-provider
            // bots do not log/clear reports for channels they do not own.
            let effective_channel_name = state.channel_name.clone().or_else(|| {
                tmux_name.as_deref().and_then(|name| {
                    crate::services::provider::parse_provider_and_channel_from_tmux_name(name)
                        .map(|(_, ch)| ch)
                })
            });
            let (allowlist_channel_id, provider_channel_name) =
                if let Some((pid, pname)) = super::resolve_thread_parent(http, channel_id).await {
                    (pid, pname.or(effective_channel_name.clone()))
                } else {
                    (channel_id, effective_channel_name.clone())
                };
            if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
                &settings_snapshot,
                provider,
                allowlist_channel_id,
                effective_channel_name.as_deref(),
                provider_channel_name.as_deref(),
                is_dm,
            ) {
                // #3869: orphan→finalize, else preserve (`false` ⇒ suppress expected-skip logs).
                routing_orphan::route_recovery_skip(
                    http,
                    shared,
                    provider,
                    &state,
                    tmux_name.as_deref(),
                    reason,
                    false,
                )
                .await;
                continue;
            }

            if session_alive {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] ↻ restart report exists but tmux session alive for channel {}: clearing report, spawning watcher immediately",
                    state.channel_id
                );
                super::restart_report::clear_restart_report(provider, state.channel_id);
                // Register session in-memory so handlers can find it.
                // Derive channel_name from tmux session name if not in inflight state.
                let effective_channel_name = state.channel_name.clone().or_else(|| {
                    tmux_name.as_deref().and_then(|name| {
                        crate::services::provider::parse_provider_and_channel_from_tmux_name(name)
                            .map(|(_, ch)| ch)
                    })
                });
                // Resolve thread parent so validation uses the same semantics
                // as normal message routing (router.rs).
                let (allowlist_channel_id, provider_channel_name) = if let Some((pid, pname)) =
                    super::resolve_thread_parent(http, channel_id).await
                {
                    (pid, pname.or(effective_channel_name.clone()))
                } else {
                    (channel_id, effective_channel_name.clone())
                };
                if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
                    &settings_snapshot,
                    provider,
                    allowlist_channel_id,
                    effective_channel_name.as_deref(),
                    provider_channel_name.as_deref(),
                    is_dm,
                ) {
                    // #3869: orphan→finalize, else preserve (`true` ⇒ log skips).
                    routing_orphan::route_recovery_skip(
                        http,
                        shared,
                        provider,
                        &state,
                        tmux_name.as_deref(),
                        reason,
                        true,
                    )
                    .await;
                    continue;
                }
                {
                    let mut data = shared.core.lock().await;
                    let session =
                        data.sessions
                            .entry(channel_id)
                            .or_insert_with(|| DiscordSession {
                                session_id: state.session_id.clone(),
                                memento_context_loaded: false,
                                memento_reflected: false,
                                current_path: None,
                                history: Vec::new(),
                                pending_uploads: Vec::new(),
                                cleared: false,
                                remote_profile_name: None,
                                channel_id: Some(state.channel_id),
                                channel_name: effective_channel_name.clone(),
                                category_name: None,
                                last_active: tokio::time::Instant::now(),
                                worktree: None,
                                born_generation: super::runtime_store::load_generation(),
                            });
                    session.channel_id = Some(state.channel_id);
                    session.last_active = tokio::time::Instant::now();
                    if session.channel_name.is_none() {
                        session.channel_name = effective_channel_name;
                    }
                    restore_recovered_session_worktree(session, &state);
                }

                let finish_mailbox_on_completion =
                    reregister_active_turn_from_inflight(shared, &state).await;

                // Spawn the tmux watcher immediately rather than deferring to
                // restore_tmux_watchers(): the "watcher will adopt" approach raced
                // — the session could die in the ~50s gap and lose the response.
                if let Some(ref tmux_session_name) = tmux_name {
                    if let Some((output_path, initial_offset, current_len, truncated)) =
                        restart_report_watcher_start(tmux_session_name, &state)
                    {
                        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let resume_offset = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
                        let pause_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                        let turn_delivered =
                            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                        let last_heartbeat_ts_ms = std::sync::Arc::new(
                            std::sync::atomic::AtomicI64::new(super::tmux_watcher_now_ms()),
                        );
                        let handle = TmuxWatcherHandle {
                            tmux_session_name: tmux_session_name.clone(),
                            output_path: output_path.clone(),
                            paused: paused.clone(),
                            resume_offset: resume_offset.clone(),
                            cancel: cancel.clone(),
                            pause_epoch: pause_epoch.clone(),
                            turn_delivered: turn_delivered.clone(),
                            last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
                        };
                        let watcher_claimed = {
                            #[cfg(unix)]
                            {
                                let claim = super::tmux::claim_or_reuse_watcher(
                                    &shared.tmux_watchers,
                                    channel_id,
                                    handle,
                                    provider,
                                    "restart_report_recovery",
                                );
                                claim.should_spawn()
                            }
                            #[cfg(not(unix))]
                            {
                                let _ = handle;
                                false
                            }
                        };
                        if watcher_claimed {
                            let ts2 = chrono::Local::now().format("%H:%M:%S");
                            if truncated {
                                tracing::info!(
                                    "  [{ts2}] ↻ recovery: output truncated for #{} (saved offset {}, file len {}), restarting watcher from 0",
                                    tmux_session_name,
                                    state.last_offset,
                                    current_len
                                );
                            }
                            tracing::info!(
                                "  [{ts2}] 👁 recovery: spawned watcher for #{} at offset {}",
                                tmux_session_name,
                                initial_offset
                            );
                            #[cfg(unix)]
                            {
                                let restored_turn =
                                    super::tmux::restored_watcher_turn_from_inflight(
                                        &state,
                                        tmux_session_name,
                                        finish_mailbox_on_completion,
                                    );
                                shared.record_tmux_watcher_reconnect(channel_id);
                                super::task_supervisor::spawn_observed_tmux_watcher(
                                    "recovery_tmux_output_watcher_with_restore",
                                    shared.clone(),
                                    tmux_session_name.clone(),
                                    cancel.clone(),
                                    super::tmux::tmux_output_watcher_with_restore(
                                        channel_id,
                                        http.clone(),
                                        shared.clone(),
                                        output_path,
                                        tmux_session_name.clone(),
                                        initial_offset,
                                        cancel,
                                        paused,
                                        resume_offset,
                                        pause_epoch,
                                        turn_delivered,
                                        last_heartbeat_ts_ms,
                                        restored_turn,
                                    ),
                                );
                            }
                        }
                    }
                }

                // Keep the inflight state until the watcher either relays the
                // final response or triggers watcher-death handoff. Clearing it
                // here breaks the handoff path if the recovered tmux session
                // dies before producing a result.
                continue;
            } else {
                let ts = chrono::Local::now().format("%H:%M:%S");
                if let Some(diag) = tmux_name.as_deref().and_then(|name| {
                    build_tmux_death_diagnostic(name, output_path_for_check.as_deref())
                }) {
                    tracing::info!(
                        "  [{ts}] ↻ restart report exists but tmux session is dead for channel {}: clearing report, continuing with direct fallback recovery ({diag})",
                        state.channel_id
                    );
                } else {
                    tracing::info!(
                        "  [{ts}] ↻ restart report exists but tmux session is dead for channel {}: clearing report, continuing with direct fallback recovery",
                        state.channel_id
                    );
                }
                super::restart_report::clear_restart_report(provider, state.channel_id);
            }
        }

        // current_msg_id/user_msg_id == 0 are LEGITIMATE (TUI-direct / un-anchored
        // recovery turn). `MessageId::new(0)` PANICS, and this loop runs inline at
        // startup, so one such inflight would abort it before `reconcile_done` is
        // set → provider permanently degraded. Carry both as `Option`, skip the
        // placeholder/analytics step per use site, still recover the tmux session.
        let current_msg_id = optional_message_id(state.current_msg_id);
        let user_msg_id = optional_message_id(state.user_msg_id);
        let channel_name = state.channel_name.clone();
        let tmux_session_name = state.tmux_session_name.clone().or_else(|| {
            channel_name
                .as_ref()
                .map(|name| provider.build_tmux_session_name(name))
        });
        let channel_name = channel_name.or_else(|| {
            tmux_session_name.as_deref().and_then(|name| {
                crate::services::provider::parse_provider_and_channel_from_tmux_name(name)
                    .map(|(_, ch)| ch)
            })
        });
        // Resolve thread parent so validation uses the same semantics
        // as normal message routing (router.rs).
        let (allowlist_channel_id, provider_channel_name) =
            if let Some((pid, pname)) = super::resolve_thread_parent(http, channel_id).await {
                (pid, pname.or(channel_name.clone()))
            } else {
                (channel_id, channel_name.clone())
            };
        if let Err(reason) = validate_bot_channel_routing_with_provider_channel(
            &settings_snapshot,
            provider,
            allowlist_channel_id,
            channel_name.as_deref(),
            provider_channel_name.as_deref(),
            is_dm,
        ) {
            routing_orphan::route_recovery_skip(
                http,
                shared,
                provider,
                &state,
                tmux_session_name.as_deref(),
                reason,
                true,
            )
            .await;
            continue;
        }
        let (fallback_output, fallback_input) = tmux_session_name
            .as_deref()
            .map(tmux_runtime_paths)
            .unwrap_or_else(|| (String::new(), String::new()));
        let runtime_kind = state.runtime_kind_for_recovery();
        let output_path = state
            .output_path
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                if !fallback_output.is_empty() {
                    Some(fallback_output.clone())
                } else {
                    None
                }
            });
        let input_fifo_path = if runtime_kind.requires_input_fifo() {
            state
                .input_fifo_path
                .clone()
                .filter(|s| !s.is_empty())
                .or_else(|| {
                    if !fallback_input.is_empty() {
                        Some(fallback_input.clone())
                    } else {
                        None
                    }
                })
        } else {
            state.input_fifo_path.clone().filter(|s| !s.is_empty())
        };
        // Check exit reason file for post-mortem diagnostics
        if let Some(ref op) = output_path {
            let exit_reason_path = format!("{}.exit_reason", op);
            if let Ok(reason) = std::fs::read_to_string(&exit_reason_path) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::info!(
                    "  [{ts}] 🔍 exit_reason for channel {}: {}",
                    state.channel_id,
                    reason.trim()
                );
                // Clean up exit reason file after reading
                let _ = std::fs::remove_file(&exit_reason_path);
            }
        }

        let terminal_success_end = output_path
            .as_deref()
            .and_then(|path| success_result_end_offset_after_offset(path, state.last_offset));
        let output_already_completed = terminal_success_end.is_some();
        let terminal_success_drained = match (output_path.as_deref(), terminal_success_end) {
            (Some(path), Some(confirmed_end)) => {
                terminal_success_output_drained_for_recovery(
                    path,
                    confirmed_end,
                    tmux_session_name.as_deref(),
                )
                .await
            }
            (None, Some(_)) => true,
            _ => false,
        };
        if output_already_completed && !terminal_success_drained {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                "  [{ts}] ⚠ recovery: terminal success observed for channel {} but tmux output has not stayed drained; reattaching watcher",
                state.channel_id
            );
        }
        let output_has_new_bytes = output_path
            .as_deref()
            .map(|path| output_has_bytes_after_offset(path, state.last_offset))
            .unwrap_or(false);

        if can_fast_path_captured_full_response(&state, terminal_success_drained) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ recovery fast-path: delivering captured full_response for channel {}",
                state.channel_id,
            );

            let assistant_response = state.full_response.clone();
            let final_text =
                super::formatting::format_for_discord_with_provider(&assistant_response, provider);
            let recovery_context = RecoveryDeliveryContext::from_state(
                provider,
                &state,
                terminal_success_end.map(|confirmed_end| (state.last_offset, confirmed_end)),
                shared.restart.current_generation,
            );
            let relay_ok = relay_recovered_terminal_text_to_placeholder(
                http,
                shared,
                channel_id,
                current_msg_id,
                &final_text,
                recovery_context.as_ref(),
            )
            .await
            .delivered();

            if !should_advance_recovery_dispatch_after_relay(relay_ok) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ recovery: Discord relay failed before dispatch completion — preserving inflight for retry"
                );
                continue;
            }
            let visible_outcome = complete_recovery_visible_turn(
                http,
                shared,
                provider,
                &state,
                false,
                "captured_full_response",
            )
            .await;
            if !visible_outcome.should_proceed() {
                // Reserved for future non-proceeding recovery outcomes.
                // A TUI quiescence timeout is not one: terminal delivery
                // evidence is authoritative for mailbox/inflight cleanup.
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    provider = %provider.as_str(),
                    channel_id = channel_id.get(),
                    "[{ts}] ⚠ recovery (captured_full_response) deferred by non-proceeding visible outcome"
                );
                continue;
            }

            let recovered_dispatch_id = parse_dispatch_id(&state.user_text)
                .or(lookup_pending_dispatch_for_thread(shared.api_port, state.channel_id).await);
            let role_binding = resolve_role_binding(channel_id, state.channel_name.as_deref());
            let duration_ms =
                recovered_turn_duration_ms(Some(state.started_at.as_str())).unwrap_or(0);
            let has_completion_evidence = if shared.pg_pool.is_some() {
                // No user message (user_msg_id == 0) → no analytics row to
                // key (`discord:<channel>:0` would be bogus); skip the
                // analytics persist but still write the transcript.
                if let Some(user_msg_id) = user_msg_id {
                    super::turn_bridge::persist_turn_analytics_row_with_handles(
                        shared.pg_pool.as_ref(),
                        provider,
                        channel_id,
                        user_msg_id,
                        role_binding.as_ref(),
                        recovered_dispatch_id
                            .as_deref()
                            .or(state.dispatch_id.as_deref()),
                        state.session_key.as_deref(),
                        state.session_id.as_deref(),
                        &state,
                        TurnTokenUsage::default(),
                        duration_ms,
                    );
                }
                persist_recovered_transcript(
                    shared.pg_pool.as_ref(),
                    provider,
                    &state,
                    recovered_dispatch_id
                        .as_deref()
                        .or(state.dispatch_id.as_deref()),
                    &assistant_response,
                )
                .await
            } else {
                !assistant_response.trim().is_empty()
            };
            let completion_context = has_completion_evidence
                .then(|| serde_json::json!({ "agent_response_present": true }));
            let fallback_result = completion_context
                .clone()
                .map(|mut result| {
                    if let Some(obj) = result.as_object_mut() {
                        obj.insert(
                            "completion_source".to_string(),
                            serde_json::Value::String(
                                "recovery_captured_full_response_db_fallback".to_string(),
                            ),
                        );
                        obj.insert("needs_reconcile".to_string(), serde_json::Value::Bool(true));
                    }
                    result
                })
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "completion_source": "recovery_captured_full_response_db_fallback",
                        "needs_reconcile": true,
                    })
                });
            let mut dispatch_completed = recovered_dispatch_id.is_none();
            if let Some(ref did) = recovered_dispatch_id {
                let dispatch_type = super::internal_api::lookup_dispatch_type(did)
                    .await
                    .ok()
                    .flatten();

                match dispatch_type.as_deref() {
                    Some("implementation") | Some("rework") => {
                        if !has_completion_evidence {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ recovery: refusing to complete work dispatch {did} without assistant response"
                            );
                        } else if let Some(engine) = &shared.policy.engine {
                            for attempt in 1..=3u8 {
                                match crate::dispatch::finalize_dispatch_with_backends(
                                    engine,
                                    did,
                                    "recovery_captured_full_response",
                                    completion_context.as_ref(),
                                ) {
                                    Ok(_) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::info!(
                                            "  [{ts}] ✓ recovery: completed dispatch {did} via finalize_dispatch"
                                        );
                                        let _ =
                                            super::turn_bridge::queue_dispatch_followup_with_handles(
                                                shared.pg_pool.as_ref(),
                                                did,
                                                "recovery_captured_full_response",
                                            )
                                            .await;
                                        dispatch_completed = true;
                                        break;
                                    }
                                    Err(e) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ recovery: finalize_dispatch failed for {did} (attempt {attempt}/3): {e}"
                                        );
                                        if attempt < 3 {
                                            // #2428 H5: exponential backoff + jitter.
                                            tokio::time::sleep(recovery_retry_backoff(u32::from(
                                                attempt,
                                            )))
                                            .await;
                                        }
                                    }
                                }
                            }
                            if !dispatch_completed {
                                dispatch_completed =
                                    super::turn_bridge::runtime_db_fallback_complete_with_result(
                                        did,
                                        &fallback_result,
                                    );
                                if dispatch_completed {
                                    let _ =
                                        super::turn_bridge::queue_dispatch_followup_with_handles(
                                            shared.pg_pool.as_ref(),
                                            did,
                                            "recovery_captured_full_response_fallback",
                                        )
                                        .await;
                                }
                            }
                        } else {
                            dispatch_completed =
                                super::turn_bridge::runtime_db_fallback_complete_with_result(
                                    did,
                                    &fallback_result,
                                );
                            if dispatch_completed {
                                let _ = super::turn_bridge::queue_dispatch_followup_with_handles(
                                    shared.pg_pool.as_ref(),
                                    did,
                                    "recovery_captured_full_response_runtime_fallback",
                                )
                                .await;
                            }
                        }
                        if !dispatch_completed {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ❌ recovery: dispatch {did} completion failed — preserving state for retry"
                            );
                        }
                    }
                    Some(_) => {
                        dispatch_completed = true;
                    }
                    None => {
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ recovery: cannot determine dispatch type for {did} — preserving state"
                        );
                    }
                }
            }

            if dispatch_completed {
                finish_recovered_turn_mailbox(
                    shared,
                    provider,
                    channel_id,
                    "recovery_captured_full_response",
                )
                .await;
                clear_inflight_state(provider, state.channel_id);
            }
            continue;
        }
        if matches!(
            recovery_phase_after_output_scan(terminal_success_drained, false),
            RecoveryPhase::Done
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ✓ recovering completed turn for channel {}: output contains result after offset {}",
                state.channel_id,
                state.last_offset
            );
            let (recovered_session_id, recovered_usage) = output_path
                .as_deref()
                .map(|path| {
                    extract_turn_analytics_from_output(
                        path,
                        state.turn_start_offset.unwrap_or(state.last_offset),
                    )
                })
                .unwrap_or((None, None));
            // Deliver the result to Discord before clearing the inflight state
            let extracted = output_path
                .as_deref()
                .map(|p| extract_response_from_output(p, state.last_offset))
                .unwrap_or_default();
            let assistant_response = if extracted.trim().is_empty() {
                state.full_response.clone()
            } else {
                extracted
            };
            let final_text = if assistant_response.trim().is_empty() {
                "(복구됨 — 응답 텍스트 없음)".to_string()
            } else {
                super::formatting::format_for_discord_with_provider(&assistant_response, provider)
            };
            // #225 P1-1: Track relay success — only clear inflight if Discord delivery succeeds
            let recovery_context = RecoveryDeliveryContext::from_state(
                provider,
                &state,
                terminal_success_end.map(|confirmed_end| (state.last_offset, confirmed_end)),
                shared.restart.current_generation,
            );
            let relay_ok = relay_recovered_terminal_text_to_placeholder(
                http,
                shared,
                channel_id,
                current_msg_id,
                &final_text,
                recovery_context.as_ref(),
            )
            .await
            .delivered();

            if !should_advance_recovery_dispatch_after_relay(relay_ok) {
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    "  [{ts}] ⚠ recovery: Discord relay failed before dispatch completion — preserving inflight for retry"
                );
                continue;
            }
            // Mark user message as completed only after Discord terminal delivery commits.
            let visible_outcome = complete_recovery_visible_turn(
                http,
                shared,
                provider,
                &state,
                false,
                "output_completed",
            )
            .await;
            if !visible_outcome.should_proceed() {
                // Reserved for future non-proceeding recovery outcomes.
                // A TUI quiescence timeout is not one: terminal delivery
                // evidence is authoritative for mailbox/inflight cleanup.
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::warn!(
                    provider = %provider.as_str(),
                    channel_id = channel_id.get(),
                    "[{ts}] ⚠ recovery (output_completed) deferred by non-proceeding visible outcome"
                );
                continue;
            }

            // Complete the dispatch if this was an implementation/rework turn.
            // Review dispatches require the verdict flow (review_verdict.rs)
            // and must not be generically finalized here.
            // #225 P1-3: Use DB lookup for dispatch ID (text parsing fails in unified threads)
            let recovered_dispatch_id = parse_dispatch_id(&state.user_text)
                .or(lookup_pending_dispatch_for_thread(shared.api_port, state.channel_id).await);
            let role_binding = resolve_role_binding(channel_id, state.channel_name.as_deref());
            let duration_ms =
                recovered_turn_duration_ms(Some(state.started_at.as_str())).unwrap_or(0);
            let has_completion_evidence = if shared.pg_pool.is_some() {
                // No user message (user_msg_id == 0) → no analytics row to
                // key (`discord:<channel>:0` would be bogus); skip the
                // analytics persist but still write the transcript.
                if let Some(user_msg_id) = user_msg_id {
                    super::turn_bridge::persist_turn_analytics_row_with_handles(
                        shared.pg_pool.as_ref(),
                        provider,
                        channel_id,
                        user_msg_id,
                        role_binding.as_ref(),
                        recovered_dispatch_id
                            .as_deref()
                            .or(state.dispatch_id.as_deref()),
                        state.session_key.as_deref(),
                        recovered_session_id
                            .as_deref()
                            .or(state.session_id.as_deref()),
                        &state,
                        recovered_usage.unwrap_or_default(),
                        duration_ms,
                    );
                }
                persist_recovered_transcript(
                    shared.pg_pool.as_ref(),
                    provider,
                    &state,
                    recovered_dispatch_id
                        .as_deref()
                        .or(state.dispatch_id.as_deref()),
                    &assistant_response,
                )
                .await
            } else {
                !assistant_response.trim().is_empty()
            };
            let completion_context = has_completion_evidence
                .then(|| serde_json::json!({ "agent_response_present": true }));
            let fallback_result = completion_context
                .clone()
                .map(|mut result| {
                    if let Some(obj) = result.as_object_mut() {
                        obj.insert(
                            "completion_source".to_string(),
                            serde_json::Value::String("recovery_output_db_fallback".to_string()),
                        );
                        obj.insert("needs_reconcile".to_string(), serde_json::Value::Bool(true));
                    }
                    result
                })
                .unwrap_or_else(|| {
                    serde_json::json!({
                        "completion_source": "recovery_output_db_fallback",
                        "needs_reconcile": true,
                    })
                });
            let mut dispatch_completed = recovered_dispatch_id.is_none();
            if let Some(ref did) = recovered_dispatch_id {
                let dispatch_type = super::internal_api::lookup_dispatch_type(did)
                    .await
                    .ok()
                    .flatten();

                match dispatch_type.as_deref() {
                    Some("implementation") | Some("rework") => {
                        if !has_completion_evidence {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ recovery: refusing to complete work dispatch {did} without assistant response"
                            );
                        } else if let Some(engine) = &shared.policy.engine {
                            for attempt in 1..=3u8 {
                                match crate::dispatch::finalize_dispatch_with_backends(
                                    engine,
                                    did,
                                    "recovery_output_completed",
                                    completion_context.as_ref(),
                                ) {
                                    Ok(_) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::info!(
                                            "  [{ts}] ✓ recovery: completed dispatch {did} via finalize_dispatch"
                                        );
                                        let _ =
                                            super::turn_bridge::queue_dispatch_followup_with_handles(
                                                shared.pg_pool.as_ref(),
                                                did,
                                                "recovery_output_completed",
                                            )
                                            .await;
                                        dispatch_completed = true;
                                        break;
                                    }
                                    Err(e) => {
                                        let ts = chrono::Local::now().format("%H:%M:%S");
                                        tracing::warn!(
                                            "  [{ts}] ⚠ recovery: finalize_dispatch failed for {did} (attempt {attempt}/3): {e}"
                                        );
                                        if attempt < 3 {
                                            // #2428 H5: exponential backoff + jitter.
                                            tokio::time::sleep(recovery_retry_backoff(u32::from(
                                                attempt,
                                            )))
                                            .await;
                                        }
                                    }
                                }
                            }
                            if !dispatch_completed {
                                dispatch_completed =
                                    super::turn_bridge::runtime_db_fallback_complete_with_result(
                                        did,
                                        &fallback_result,
                                    );
                                if dispatch_completed {
                                    let _ =
                                        super::turn_bridge::queue_dispatch_followup_with_handles(
                                            shared.pg_pool.as_ref(),
                                            did,
                                            "recovery_output_completed_fallback",
                                        )
                                        .await;
                                }
                            }
                        } else {
                            dispatch_completed =
                                super::turn_bridge::runtime_db_fallback_complete_with_result(
                                    did,
                                    &fallback_result,
                                );
                            if dispatch_completed {
                                let _ = super::turn_bridge::queue_dispatch_followup_with_handles(
                                    shared.pg_pool.as_ref(),
                                    did,
                                    "recovery_output_completed_runtime_fallback",
                                )
                                .await;
                            }
                        }
                        if !dispatch_completed {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ❌ recovery: dispatch {did} completion failed — preserving state for retry"
                            );
                        }
                    }
                    Some(_) => {
                        // Non-work dispatches (review, review-decision) need
                        // their own explicit API completion flow. Clear inflight
                        // but leave dispatch status untouched.
                        dispatch_completed = true;
                    }
                    None => {
                        // DB unavailable — cannot determine dispatch type.
                        // Preserve inflight state so the next recovery pass can retry.
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::warn!(
                            "  [{ts}] ⚠ recovery: cannot determine dispatch type for {did} — preserving state"
                        );
                    }
                }
            }

            // #225 P1-1: Only clear inflight if both dispatch completed AND relay succeeded.
            // If relay failed, preserve inflight for retry on next startup.
            if dispatch_completed {
                finish_recovered_turn_mailbox(
                    shared,
                    provider,
                    channel_id,
                    "recovery_output_completed",
                )
                .await;
                clear_inflight_state(provider, state.channel_id);
            }
            continue;
        }

        let tmux_ready_without_new_output = tmux_session_name.as_deref().map_or(false, |name| {
            !output_has_new_bytes
                && recovery_has_post_work_ready_evidence(&state)
                && inflight_or_legacy_tmux_ready_for_input(provider, &state, name, true)
        });

        if matches!(
            recovery_phase_after_output_scan(false, tmux_ready_without_new_output),
            RecoveryPhase::Done
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            // #2770: ready/idle is not terminal delivery evidence. If recovery
            // has neither captured text nor a recorded relay commit, preserve
            // the inflight so the pane-alive reattach path below can own it.
            if recovery_ready_without_output_already_delivered(&state) {
                tracing::info!(
                    "  [{ts}] ✓ clearing inflight turn for channel {}: tmux is ready for input and terminal delivery was already recorded after offset {}",
                    state.channel_id,
                    state.last_offset
                );
                finish_recovered_turn_mailbox(
                    shared,
                    provider,
                    channel_id,
                    "recovery_ready_without_output_already_delivered",
                )
                .await;
                clear_inflight_state(provider, state.channel_id);
                continue;
            }
            if recovery_ready_without_output_has_captured_response(&state) {
                tracing::info!(
                    "  [{ts}] ✓ clearing inflight turn for channel {}: tmux is ready for input and captured output is idle after offset {}",
                    state.channel_id,
                    state.last_offset
                );
                let final_text = super::formatting::format_for_discord_with_provider(
                    &state.full_response,
                    provider,
                );
                let outcome =
                    relay_recovery_terminal_notice(http, shared, provider, &state, &final_text)
                        .await;
                // #3293: tmux_alive=true — budget force-clear forbidden here
                // (pane-alive invariant); only a permanent verdict clears.
                dispose_recovery_relay_outcome(
                    shared,
                    provider,
                    &state,
                    outcome,
                    true,
                    "recovery_ready_without_output",
                    "ready_without_output",
                    &state.full_response,
                    false,
                )
                .await;
                continue;
            }
            tracing::warn!(
                "  [{ts}] ⚠ recovery: deferring ready-without-output completion for channel {} because no captured assistant response or terminal delivery evidence exists",
                state.channel_id
            );
        }

        let can_recover = tmux_session_name
            .as_deref()
            .map_or(false, |name| tmux_has_session_with_retry(name));

        if matches!(
            recovery_phase_after_tmux_probe(can_recover, None),
            RecoveryPhase::Done
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            // Even without a live tmux session, the output file may contain
            // response data. Try extracting from the full file first, then
            // fall back to saved partial response.
            let extracted_full = output_path
                .as_deref()
                .map(|p| extract_response_from_output(p, 0))
                .unwrap_or_default();
            let best_response = if !extracted_full.trim().is_empty() {
                extracted_full
            } else {
                state.full_response.clone()
            };
            let stale_text = interrupted_recovery_message(&state, &best_response);
            let death_diag = tmux_session_name
                .as_deref()
                .and_then(|name| build_tmux_death_diagnostic(name, output_path.as_deref()));
            if let Some(ref diag) = death_diag {
                tracing::info!(
                    "  [{ts}] ⚠ cannot recover inflight turn for channel {}: tmux session missing (response len: {}, {diag})",
                    state.channel_id,
                    best_response.len()
                );
            } else {
                tracing::info!(
                    "  [{ts}] ⚠ cannot recover inflight turn for channel {}: tmux session missing (response len: {})",
                    state.channel_id,
                    best_response.len()
                );
            }
            let outcome =
                relay_recovery_terminal_notice(http, shared, provider, &state, &stale_text).await;
            if let Some(ref sk) = state.session_key {
                crate::services::termination_audit::record_termination_with_handles(
                    shared.pg_pool.as_ref(),
                    sk,
                    state.dispatch_id.as_deref(),
                    "recovery",
                    "restart_session_missing",
                    Some("tmux session missing after restart"),
                    death_diag.as_deref(),
                    Some(state.last_offset),
                    Some(false),
                );
            }
            save_missing_session_handoff(provider, &state, &best_response);
            // Handoff already saved above for every outcome (last arg).
            dispose_recovery_relay_outcome(
                shared,
                provider,
                &state,
                outcome,
                false,
                "recovery_missing_tmux",
                "missing_tmux",
                &best_response,
                true,
            )
            .await;
            continue;
        }

        let Some(tmux_session_name) = tmux_session_name else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ clearing inflight turn for channel {}: tmux session name missing",
                state.channel_id
            );
            let text = stale_inflight_message("tmux session name missing during recovery");
            let outcome =
                relay_recovery_terminal_notice(http, shared, provider, &state, &text).await;
            // #3297 finding 4: past the can_recover gate tmux absence is NOT
            // established — tmux_alive=true forbids budget force-clear here.
            dispose_recovery_relay_outcome(
                shared,
                provider,
                &state,
                outcome,
                true,
                "recovery_missing_tmux_name",
                "missing_tmux_name",
                &state.full_response,
                false,
            )
            .await;
            continue;
        };
        let Some(output_path) = output_path else {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ⚠ clearing inflight turn for channel {}: output path missing",
                state.channel_id
            );
            let text = stale_inflight_message("output path missing during recovery");
            let outcome =
                relay_recovery_terminal_notice(http, shared, provider, &state, &text).await;
            // #3297 finding 4: tmux session existence was confirmed above
            // (can_recover consumed the missing-tmux rows) — tmux_alive=true,
            // so the budget can never clear a possibly-live pane here.
            dispose_recovery_relay_outcome(
                shared,
                provider,
                &state,
                outcome,
                true,
                "recovery_missing_output_path",
                "missing_output_path",
                &state.full_response,
                false,
            )
            .await;
            continue;
        };
        let input_fifo_path = match recovery_input_fifo_for_runtime(runtime_kind, input_fifo_path) {
            Ok(path) => path,
            Err(reason) => {
                // #2235: when the inflight row was written without a stamped
                // `runtime_kind` (legacy pre-v8 row, hook-endpoint race, or a
                // future variant this binary doesn't recognize),
                // `runtime_kind_for_recovery` had to guess. If the guess
                // requires a FIFO that the row never carried, surfacing a
                // user-visible "input fifo path missing" notice misleads the
                // operator — the right thing is to skip recovery silently and
                // let the next turn re-establish state from scratch.
                let runtime_kind_was_inferred = state.runtime_kind.is_none();
                let ts = chrono::Local::now().format("%H:%M:%S");
                if runtime_kind_was_inferred {
                    tracing::debug!(
                        "  [{ts}] ↩ inflight recovery silent-skip for channel {}: runtime_kind unknown/missing on-disk, inferred {} requires FIFO but row carries none",
                        state.channel_id,
                        runtime_kind.as_str()
                    );
                    finish_recovered_turn_mailbox(
                        shared,
                        provider,
                        channel_id,
                        "recovery_runtime_kind_missing_skip",
                    )
                    .await;
                    clear_inflight_state(provider, state.channel_id);
                    continue;
                }
                tracing::info!(
                    "  [{ts}] ⚠ clearing inflight turn for channel {}: input fifo path missing (runtime={})",
                    state.channel_id,
                    runtime_kind.as_str()
                );
                let text = stale_inflight_message(reason);
                let outcome =
                    relay_recovery_terminal_notice(http, shared, provider, &state, &text).await;
                // #3297 finding 4: tmux existence already confirmed —
                // tmux_alive=true (budget clear forbidden; permanent only).
                dispose_recovery_relay_outcome(
                    shared,
                    provider,
                    &state,
                    outcome,
                    true,
                    "recovery_missing_input_fifo",
                    "missing_input_fifo",
                    &state.full_response,
                    false,
                )
                .await;
                continue;
            }
        };

        if recovery_terminal_delivery_already_committed(&state) {
            // #3610 PR-2: the #3607 backstop — this row's terminal answer WAS
            // committed, but it may have since vanished from Discord. Behind a
            // default-OFF flag, probe the recorded anchor and repost (send-new) iff
            // it is permanently gone. Flag OFF → `enabled()` is false so the whole
            // block is skipped and the legacy finish+clear below runs byte-identically.
            // #3918: the committed answer's anchor may have vanished — run the
            // anchor-repost send-new fallback AND its on-disk row disposition in
            // `recovery_paths::restart`. `true` ⇒ the row is fully handled
            // (relayed + disposed, OR the pre-send bump did not durably persist so
            // the send was REFUSED and the row deliberately PRESERVED for a later
            // boot) → `continue` WITHOUT the legacy committed clear, which would
            // otherwise drop an IoError-deferred answer or delete a newer turn's
            // row. `false` ⇒ no repost needed/possible → fall through to the clear.
            // Flag OFF short-circuits before the call, so the dark deploy stays a
            // byte-for-byte no-op.
            if recovery_paths::shared::recovery_anchor_repost_enabled()
                && recovery_paths::restart::recover_committed_anchor_repost(
                    http,
                    shared,
                    provider,
                    &state,
                    &state.full_response,
                )
                .await
            {
                continue;
            }
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = state.channel_id,
                user_msg_id = state.user_msg_id,
                "  [{ts}] ✓ recovery: clearing delivered inflight before watcher re-register; terminal response already reached Discord"
            );
            finish_recovered_turn_mailbox(
                shared,
                provider,
                channel_id,
                "recovery_terminal_delivery_already_committed",
            )
            .await;
            clear_inflight_state(provider, state.channel_id);
            continue;
        }

        // If the tmux pane is alive, skip the recovery reader entirely. The idle
        // session gets a watcher immediately rather than deferring to
        // restore_tmux_watchers() — that ~50s gap raced and lost the response.
        let pane_alive = tmux_session_alive_with_retry(&tmux_session_name);
        if matches!(
            recovery_phase_after_tmux_probe(true, Some(pane_alive)),
            RecoveryPhase::WatcherReattach
        ) {
            let ts = chrono::Local::now().format("%H:%M:%S");
            tracing::info!(
                "  [{ts}] ↻ inflight recovery: pane alive for channel {}, spawning watcher immediately",
                state.channel_id
            );
            // Register session in-memory so handlers can find it.
            let effective_channel_name = channel_name.clone().or_else(|| {
                crate::services::provider::parse_provider_and_channel_from_tmux_name(
                    &tmux_session_name,
                )
                .map(|(_, ch)| ch)
            });
            {
                let persisted_session_path = load_last_session_path(
                    shared.pg_pool.as_ref(),
                    &shared.token_hash,
                    channel_id.get(),
                );
                let recovery_adk_cwd = match recovery_spawn_adk_cwd(&state, persisted_session_path)
                {
                    Ok(path) => path,
                    Err(error) => {
                        let dispatch_id = state
                            .dispatch_id
                            .clone()
                            .or_else(|| parse_dispatch_id(&state.user_text));
                        let ts = chrono::Local::now().format("%H:%M:%S");
                        tracing::error!("  [{ts}] {error}; main-workspace fallback blocked");
                        // #3562: back-trace the recovery to a specific agent/turn.
                        let recovery_turn_id =
                            super::analytics_transcript::recovered_transcript_turn_id(
                                state.channel_id,
                                state.user_msg_id,
                                state.session_key.as_deref(),
                                state.turn_start_offset,
                                &state.started_at,
                            );
                        let recovery_agent_id =
                            resolve_role_binding(channel_id, state.channel_name.as_deref())
                                .map(|b| b.role_id);
                        crate::services::observability::emit_recovery_fired(
                            provider.as_str(),
                            state.channel_id,
                            dispatch_id.as_deref(),
                            state.session_key.as_deref(),
                            Some(recovery_turn_id.as_str()),
                            recovery_agent_id.as_deref(),
                            "worktree_missing_main_fallback_blocked",
                        );
                        emit_recovery_quality_event(
                            provider,
                            state.channel_id,
                            dispatch_id.as_deref(),
                            state.session_key.as_deref(),
                            Some(recovery_turn_id.as_str()),
                            recovery_agent_id.as_deref(),
                            "worktree_missing_main_fallback_blocked",
                        );
                        let recovery_context = RecoveryDeliveryContext::from_state(
                            provider,
                            &state,
                            None,
                            shared.restart.current_generation,
                        );
                        let relay_ok = relay_recovered_terminal_text_to_placeholder(
                            http,
                            shared,
                            channel_id,
                            current_msg_id,
                            &format!("❌ {error}\nmain workspace fallback blocked."),
                            recovery_context.as_ref(),
                        )
                        .await
                        .delivered();
                        if should_advance_recovery_dispatch_after_relay(relay_ok) {
                            super::turn_bridge::fail_dispatch_with_retry(
                                shared.api_port,
                                dispatch_id.as_deref(),
                                &error,
                            )
                            .await;
                            super::restart_report::clear_restart_report(provider, state.channel_id);
                            clear_inflight_state(provider, state.channel_id);
                        } else {
                            let ts = chrono::Local::now().format("%H:%M:%S");
                            tracing::warn!(
                                "  [{ts}] ⚠ recovery: worktree error relay failed before dispatch failure — preserving inflight for retry"
                            );
                        }
                        continue;
                    }
                };
                let mut data = shared.core.lock().await;
                let session = data
                    .sessions
                    .entry(channel_id)
                    .or_insert_with(|| DiscordSession {
                        session_id: state.session_id.clone(),
                        memento_context_loaded: false,
                        memento_reflected: false,
                        current_path: None,
                        history: Vec::new(),
                        pending_uploads: Vec::new(),
                        cleared: false,
                        remote_profile_name: None,
                        channel_id: Some(channel_id.get()),
                        channel_name: effective_channel_name.clone(),
                        category_name: None,
                        last_active: tokio::time::Instant::now(),
                        worktree: None,
                        born_generation: super::runtime_store::load_generation(),
                    });
                session.channel_id = Some(channel_id.get());
                session.last_active = tokio::time::Instant::now();
                if session.current_path.is_none() {
                    session.current_path = recovery_adk_cwd;
                }
                if session.channel_name.is_none() {
                    session.channel_name = effective_channel_name;
                }
                restore_recovered_session_worktree(session, &state);
            }

            let finish_mailbox_on_completion =
                reregister_active_turn_from_inflight(shared, &state).await;

            // #4380 backstop: `reregister_active_turn_from_inflight` stamps
            // `readopted_from_inflight`, which the watcher-yield escape hatch honours
            // to resume relay for this re-adopted live turn. If that marker did NOT
            // durably persist (IoError), the recovered watcher will still yield to
            // the dead bridge and drop the remaining output silently — dead-letter it
            // so the loss is observable/recoverable instead of a silent wedge. No-op
            // on the normal path (marker present).
            #[cfg(unix)]
            super::guard_readopt_relay_resume_or_dead_letter(shared, provider, channel_id);

            let output_path = match restore_codex_rollout_output_path(provider, &state, output_path)
            {
                RestorePersistOutcome::UseOutputPath(output_path) => output_path,
                RestorePersistOutcome::SkipWatcher => continue,
            };

            // Immediately spawn watcher to avoid race condition.
            if std::fs::metadata(&output_path).is_ok() {
                let (initial_offset, current_len, truncated) =
                    recovery_watcher_start_offset_for_state(&output_path, &state);
                let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let paused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let resume_offset = std::sync::Arc::new(std::sync::Mutex::new(None::<u64>));
                let pause_epoch = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
                let turn_delivered = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
                let last_heartbeat_ts_ms = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(
                    super::tmux_watcher_now_ms(),
                ));
                let handle = TmuxWatcherHandle {
                    tmux_session_name: tmux_session_name.clone(),
                    output_path: output_path.clone(),
                    paused: paused.clone(),
                    resume_offset: resume_offset.clone(),
                    cancel: cancel.clone(),
                    pause_epoch: pause_epoch.clone(),
                    turn_delivered: turn_delivered.clone(),
                    last_heartbeat_ts_ms: last_heartbeat_ts_ms.clone(),
                };
                let watcher_claimed = {
                    #[cfg(unix)]
                    {
                        let claim = super::tmux::claim_or_reuse_watcher(
                            &shared.tmux_watchers,
                            channel_id,
                            handle,
                            provider,
                            "inflight_recovery",
                        );
                        claim.should_spawn()
                    }
                    #[cfg(not(unix))]
                    {
                        let _ = handle;
                        false
                    }
                };
                if watcher_claimed {
                    let ts2 = chrono::Local::now().format("%H:%M:%S");
                    if truncated {
                        tracing::info!(
                            "  [{ts2}] ↻ recovery: output truncated for #{} (saved offset {}, file len {}), restarting watcher from 0",
                            tmux_session_name,
                            state.last_offset,
                            current_len
                        );
                    }
                    tracing::info!(
                        "  [{ts2}] 👁 recovery: spawned watcher for #{} at offset {}",
                        tmux_session_name,
                        initial_offset
                    );
                    #[cfg(unix)]
                    {
                        let restored_turn = super::tmux::restored_watcher_turn_from_inflight(
                            &state,
                            &tmux_session_name,
                            finish_mailbox_on_completion,
                        );
                        shared.record_tmux_watcher_reconnect(channel_id);
                        super::task_supervisor::spawn_observed_tmux_watcher(
                            "recovery_restore_inflight_tmux_output_watcher_with_restore",
                            shared.clone(),
                            tmux_session_name.clone(),
                            cancel.clone(),
                            super::tmux::tmux_output_watcher_with_restore(
                                channel_id,
                                http.clone(),
                                shared.clone(),
                                output_path.clone(),
                                tmux_session_name.clone(),
                                initial_offset,
                                cancel,
                                paused,
                                resume_offset,
                                pause_epoch,
                                turn_delivered,
                                last_heartbeat_ts_ms,
                                restored_turn,
                            ),
                        );
                    }
                }
            }

            // Keep the inflight state until the watcher either relays the final response or
            // triggers watcher-death handoff. Clearing it here breaks the handoff path if the
            // recovered tmux session dies before producing a result.
            continue;
        }

        shared
            .restart
            .recovering_channels
            .insert(channel_id, std::time::Instant::now());

        let persisted_session_path = load_last_session_path(
            shared.pg_pool.as_ref(),
            &shared.token_hash,
            channel_id.get(),
        );
        let recovery_adk_cwd = match recovery_spawn_adk_cwd(&state, persisted_session_path) {
            Ok(path) => path,
            Err(error) => {
                let dispatch_id = state
                    .dispatch_id
                    .clone()
                    .or_else(|| parse_dispatch_id(&state.user_text));
                let ts = chrono::Local::now().format("%H:%M:%S");
                tracing::error!("  [{ts}] {error}; main-workspace fallback blocked");
                // #3562: back-trace the recovery to a specific agent/turn.
                let recovery_turn_id = super::analytics_transcript::recovered_transcript_turn_id(
                    state.channel_id,
                    state.user_msg_id,
                    state.session_key.as_deref(),
                    state.turn_start_offset,
                    &state.started_at,
                );
                let recovery_agent_id =
                    resolve_role_binding(channel_id, state.channel_name.as_deref())
                        .map(|b| b.role_id);
                crate::services::observability::emit_recovery_fired(
                    provider.as_str(),
                    state.channel_id,
                    dispatch_id.as_deref(),
                    state.session_key.as_deref(),
                    Some(recovery_turn_id.as_str()),
                    recovery_agent_id.as_deref(),
                    "worktree_missing_main_fallback_blocked",
                );
                emit_recovery_quality_event(
                    provider,
                    state.channel_id,
                    dispatch_id.as_deref(),
                    state.session_key.as_deref(),
                    Some(recovery_turn_id.as_str()),
                    recovery_agent_id.as_deref(),
                    "worktree_missing_main_fallback_blocked",
                );
                let recovery_context = RecoveryDeliveryContext::from_state(
                    provider,
                    &state,
                    None,
                    shared.restart.current_generation,
                );
                let relay_ok = relay_recovered_terminal_text_to_placeholder(
                    http,
                    shared,
                    channel_id,
                    current_msg_id,
                    &format!("❌ {error}\nmain workspace fallback blocked."),
                    recovery_context.as_ref(),
                )
                .await
                .delivered();
                if should_advance_recovery_dispatch_after_relay(relay_ok) {
                    super::turn_bridge::fail_dispatch_with_retry(
                        shared.api_port,
                        dispatch_id.as_deref(),
                        &error,
                    )
                    .await;
                    super::restart_report::clear_restart_report(provider, state.channel_id);
                    clear_inflight_state(provider, state.channel_id);
                } else {
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    tracing::warn!(
                        "  [{ts}] ⚠ recovery: worktree error relay failed before dispatch failure — preserving inflight for retry"
                    );
                }
                continue;
            }
        };
        let cancel_token = Arc::new(CancelToken::new());
        super::turn_bridge::bind_cancel_token_tmux_runtime(
            provider,
            &cancel_token,
            &tmux_session_name,
            "recovery kickoff",
        );

        {
            let mut data = shared.core.lock().await;
            let session = data
                .sessions
                .entry(channel_id)
                .or_insert_with(|| DiscordSession {
                    session_id: state.session_id.clone(),
                    memento_context_loaded: false,
                    memento_reflected: false,
                    current_path: None,
                    history: Vec::new(),
                    pending_uploads: Vec::new(),
                    cleared: false,
                    remote_profile_name: None,
                    channel_id: Some(channel_id.get()),
                    channel_name: channel_name.clone(),
                    category_name: None,
                    last_active: tokio::time::Instant::now(),
                    worktree: None,

                    born_generation: super::runtime_store::load_generation(),
                });
            session.channel_id = Some(channel_id.get());
            session.last_active = tokio::time::Instant::now();
            if session.current_path.is_none() {
                session.current_path = recovery_adk_cwd.clone();
            }
            if session.channel_name.is_none() {
                session.channel_name = channel_name.clone();
            }
            session.remote_profile_name = None;
            restore_recovered_session_worktree(session, &state);
        }

        mailbox_recovery_kickoff(
            shared,
            channel_id,
            cancel_token.clone(),
            UserId::new(state.request_owner_user_id),
            // user_msg_id == 0 (TUI-direct turn) → no active user message to
            // bind; `optional_message_id` yields None instead of panicking.
            user_msg_id,
        )
        .await;

        let adk_session_key = build_adk_session_key(shared, channel_id, provider).await;
        let adk_session_name = channel_name.clone();
        let adk_session_info = derive_adk_session_info(
            Some(&state.user_text),
            channel_name.as_deref(),
            recovery_adk_cwd.as_deref(),
        );
        let role_binding = resolve_role_binding(channel_id, channel_name.as_deref());
        let adk_thread_channel_id = adk_session_name
            .as_deref()
            .and_then(crate::services::discord::adk_session::parse_thread_channel_id_from_name);
        post_adk_session_status(
            adk_session_key.as_deref(),
            adk_session_name.as_deref(),
            Some(provider.as_str()),
            "working",
            provider,
            Some(&adk_session_info),
            None,
            recovery_adk_cwd.as_deref(),
            parse_dispatch_id(&state.user_text)
                .or(lookup_pending_dispatch_for_thread(shared.api_port, channel_id.get()).await)
                .as_deref(),
            adk_thread_channel_id,
            Some(channel_id),
            role_binding
                .as_ref()
                .map(|binding| binding.role_id.as_str()),
            shared.api_port,
        )
        .await;

        let (tx, rx) = mpsc::channel();
        let cancel_for_reader = cancel_token.clone();
        let output_for_reader = output_path.clone();
        let input_for_reader = input_fifo_path.clone();
        let tmux_for_reader = tmux_session_name.clone();
        let start_offset = state.last_offset;
        let recovery_session_id = state.session_id.clone();
        let runtime_kind_for_reader = runtime_kind;
        let retry_channel_id = channel_id.get();
        let provider_for_reader = provider.clone();
        std::thread::spawn(move || {
            match crate::services::session_backend::read_output_file_until_result(
                &output_for_reader,
                start_offset,
                tx.clone(),
                Some(cancel_for_reader),
                crate::services::provider::SessionProbe::tmux_with_structured_output(
                    tmux_for_reader.clone(),
                    provider_for_reader,
                    Some(runtime_kind_for_reader),
                    output_for_reader.clone(),
                ),
            ) {
                Ok(ReadOutputResult::Completed { offset })
                | Ok(ReadOutputResult::Cancelled { offset }) => {
                    let _ = tx.send(StreamMessage::RuntimeReady {
                        handoff: runtime_handoff_for_recovery(
                            runtime_kind_for_reader,
                            output_for_reader,
                            input_for_reader,
                            tmux_for_reader,
                            recovery_session_id,
                            offset,
                        ),
                    });
                }
                Ok(ReadOutputResult::SessionDied { offset }) => {
                    // Check if tmux pane is actually alive — dcserver restart
                    // may cause SessionDied because no new output arrived, but
                    // the Claude CLI process could still be idle (waiting for input).
                    let pane_alive = tmux_session_alive_with_retry(&tmux_for_reader);
                    let ts = chrono::Local::now().format("%H:%M:%S");
                    if pane_alive {
                        // Session is alive but idle — hand off to watcher instead of retrying
                        tracing::warn!(
                            "  [{ts}] ↻ Recovery: session idle but pane alive — handing off to watcher (channel {})",
                            retry_channel_id
                        );
                        let _ = tx.send(StreamMessage::RuntimeReady {
                            handoff: runtime_handoff_for_recovery(
                                runtime_kind_for_reader,
                                output_for_reader,
                                input_for_reader,
                                tmux_for_reader,
                                recovery_session_id,
                                offset,
                            ),
                        });
                    } else {
                        // Session truly died during restart recovery. Fall back
                        // to the generic auto-retry path so restart handling
                        // does not get a special handoff-only branch.
                        tracing::warn!(
                            "  [{ts}] ↻ Recovery: session died, signaling generic auto-retry (channel {})",
                            retry_channel_id
                        );
                        let _ = tx.send(StreamMessage::Done {
                            result: "__session_died_retry__".to_string(),
                            session_id: recovery_session_id,
                        });
                    }
                }
                Err(e) => {
                    let _ = tx.send(StreamMessage::Error {
                        message: e,
                        stdout: String::new(),
                        stderr: String::new(),
                        exit_code: None,
                    });
                }
            }
        });

        let recovery_dispatch_id = parse_dispatch_id(&state.user_text)
            .or(lookup_pending_dispatch_for_thread(shared.api_port, channel_id.get()).await);
        let recovery_dispatch_kind =
            lookup_turn_finished_dispatch_kind(recovery_dispatch_id.as_deref()).await;
        // Backfill session_key/dispatch_id on inflight state for long-turn detection ([L]).
        let mut state = state;
        state.session_key = state.session_key.or_else(|| adk_session_key.clone());
        state.dispatch_id = state.dispatch_id.or_else(|| recovery_dispatch_id.clone());
        // #3166: read the real configured thresholds (e.g.
        // `context_compact_percent_claude`) instead of `ContextThresholds::default()`
        // so the recovered turn's status panel reflects the user-set auto-compact
        // percent, matching the live launch paths (intake_turn/headless_turn).
        // This is the display value; the spawn-side `CLAUDE_AUTOCOMPACT_PCT_OVERRIDE`
        // env is exported by the launch script (claude_tui/session.rs, #3166).
        let recovery_compact_percent =
            super::adk_session::fetch_context_thresholds(shared.api_port)
                .await
                .compact_pct_for(&provider);
        spawn_turn_bridge(
            shared.clone(),
            cancel_token,
            rx,
            TurnBridgeContext {
                provider: provider.clone(),
                gateway: Arc::new(DiscordGateway::new(
                    http.clone(),
                    shared.clone(),
                    provider.clone(),
                    None,
                )),
                channel_id,
                user_msg_id,
                user_text_owned: state.user_text.clone(),
                request_owner_name: String::new(),
                role_binding,
                adk_session_key,
                adk_session_name,
                adk_session_info: Some(adk_session_info),
                adk_cwd: recovery_adk_cwd.clone(),
                dispatch_id: recovery_dispatch_id,
                dispatch_kind: recovery_dispatch_kind,
                memory_recall_usage: crate::services::memory::TokenUsage::default(),
                context_window_tokens: provider.default_context_window(),
                context_compact_percent: recovery_compact_percent,
                current_msg_id,
                response_sent_offset: state.response_sent_offset,
                full_response: state.full_response.clone(),
                tmux_last_offset: Some(state.last_offset),
                new_session_id: state.session_id.clone(),
                defer_watcher_resume: false,
                reuse_status_panel_message: true,
                completion_tx: None,
                is_external_input_tui_direct: false, // #3089 A6b: recovery is not external-input
                inflight_state: state,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::services::discord::inflight::{
        self, GuardedSaveOutcome, InflightTurnIdentity, InflightTurnState, RelayOwnerKind,
    };
    use crate::services::provider::ProviderKind;

    #[test]
    fn restore_rollout_output_path_patch_preserves_concurrent_relay_fields() {
        let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let tempdir = tempfile::tempdir().expect("temp runtime root");
        let _env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_ROOT_DIR",
            tempdir.path(),
        );

        let provider = ProviderKind::Codex;
        let channel_id = 4_111_002;
        let snapshot = InflightTurnState::new(
            provider.clone(),
            channel_id,
            Some("adk-cc".to_string()),
            7,
            4_111_102,
            4_111_202,
            "restore fallback rollout".to_string(),
            Some("session-4111-restore".to_string()),
            Some("AgentDesk-codex-4111-restore".to_string()),
            Some("/tmp/agentdesk-4111-old.jsonl".to_string()),
            None,
            128,
        );
        inflight::save_inflight_state(&snapshot).expect("seed restore snapshot row");
        let identity = InflightTurnIdentity::from_state(&snapshot);

        let mut concurrent = inflight::load_inflight_state(&provider, channel_id)
            .expect("seeded row for concurrent update");
        concurrent.last_watcher_relayed_offset = Some(4_096);
        concurrent.last_watcher_relayed_generation_mtime_ns = Some(12_345);
        concurrent.session_bound_delivered = true;
        concurrent.set_relay_owner_kind(RelayOwnerKind::SessionBoundRelay);
        inflight::save_inflight_state(&concurrent).expect("save concurrent relay fields");

        let outcome = inflight::persist_recovery_output_path_if_matches_identity_locked(
            &provider,
            channel_id,
            &identity,
            "/tmp/agentdesk-4111-rollout.jsonl".to_string(),
        );

        assert_eq!(outcome, GuardedSaveOutcome::Saved);
        let persisted =
            inflight::load_inflight_state(&provider, channel_id).expect("patched row must survive");
        assert_eq!(
            persisted.output_path.as_deref(),
            Some("/tmp/agentdesk-4111-rollout.jsonl")
        );
        assert_eq!(persisted.last_watcher_relayed_offset, Some(4_096));
        assert_eq!(
            persisted.last_watcher_relayed_generation_mtime_ns,
            Some(12_345)
        );
        assert!(persisted.session_bound_delivered);
        assert_eq!(
            persisted.effective_relay_owner_kind(),
            RelayOwnerKind::SessionBoundRelay
        );
    }
}
