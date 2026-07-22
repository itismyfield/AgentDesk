use super::super::{inflight, task_supervisor};
use super::claude_idle_bridge::compose_tui_idle_response;
use super::*;

#[cfg(unix)]
fn advance_codex_tui_runtime_binding_and_marker_offset(
    tmux_session_name: &str,
    rollout_path: &std::path::Path,
    offset: u64,
) {
    let rollout_path_str = rollout_path.to_str().unwrap_or_default();
    if !crate::services::tui_prompt_dedupe::advance_tmux_runtime_binding_offset(
        tmux_session_name,
        rollout_path_str,
        offset,
    ) {
        return;
    }
    if let Err(error) =
        crate::services::codex_tui::session::advance_codex_tui_rollout_marker_start_offset(
            tmux_session_name,
            rollout_path,
            offset,
        )
    {
        tracing::warn!(
            tmux_session_name,
            rollout_path = %rollout_path.display(),
            offset,
            error,
            "failed to advance Codex TUI rollout marker cursor"
        );
    }
}

#[cfg(unix)]
pub(super) fn spawn_codex_idle_rollout_relay(shared: Arc<SharedData>) {
    if CODEX_IDLE_ROLLOUT_RELAY_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    task_supervisor::spawn_observed("codex_idle_rollout_relay", async move {
        let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let mut active_tails: HashSet<String> = HashSet::new();
        let mut next_rehydrate = tokio::time::Instant::now();

        loop {
            // #3711: direct Codex TUI tmux sessions survive dcserver restarts,
            // but their in-memory rollout binding does not.
            let now = tokio::time::Instant::now();
            if now >= next_rehydrate {
                let shared_for_rehydrate = shared.clone();
                let rehydrate_result = tokio::task::spawn_blocking(move || {
                    rehydrate_existing_codex_tui_bindings(&shared_for_rehydrate);
                })
                .await;
                if let Err(error) = rehydrate_result {
                    tracing::warn!(
                        error = %error,
                        "Codex TUI binding rehydrate task panicked or was cancelled"
                    );
                }
                next_rehydrate = now + CLAUDE_IDLE_REHYDRATE_POLL_INTERVAL;
            }

            while let Ok(tmux_session_name) = done_rx.try_recv() {
                active_tails.remove(&tmux_session_name);
            }

            for (tmux_session_name, binding) in
                crate::services::tui_prompt_dedupe::runtime_bindings_for_kind(
                    RuntimeHandoffKind::CodexTui,
                )
            {
                if active_tails.contains(&tmux_session_name) {
                    continue;
                }
                let Some(channel_id) =
                    owner_channel_for_tmux_session(&shared, &ProviderKind::Codex, &tmux_session_name)
                else {
                    // #3018/#3306/#3656: registry miss ⇒ drop; Codex repair-ineligible.
                    continue;
                };
                let binding = resolved_codex_idle_relay_binding(
                    &tmux_session_name,
                    channel_id,
                    &binding,
                )
                .unwrap_or(binding);
                let rollout_path = PathBuf::from(&binding.output_path);
                if !rollout_path.exists() {
                    tracing::debug!(
                        tmux_session_name = %tmux_session_name,
                        rollout_path = %rollout_path.display(),
                        "codex idle rollout relay skipped missing rollout path"
                    );
                    continue;
                }
                if let Some(inflight) =
                    inflight::load_inflight_state(&ProviderKind::Codex, channel_id.get())
                {
                    if codex_ownerless_external_input_inflight_needs_rollout_recovery(
                        &inflight,
                        &tmux_session_name,
                    ) {
                        match scan_codex_idle_rollout_for_latest_prompt_matching(
                            &rollout_path,
                            &inflight.user_text,
                        ) {
                            Ok(Some(CodexIdleRolloutScan::Prompt {
                                prompt,
                                line_end_offset,
                                ..
                            })) => {
                                if let Some(anchor_id) = inflight.injected_prompt_message_id {
                                    crate::services::tui_prompt_dedupe::record_prompt_anchor(
                                        ProviderKind::Codex.as_str(),
                                        &tmux_session_name,
                                        channel_id.get(),
                                        anchor_id,
                                    );
                                }
                                let observed_at = chrono::Utc::now();
                                let lease = record_external_turn_lease_for_output(
                                    &shared,
                                    &ProviderKind::Codex,
                                    channel_id,
                                    &tmux_session_name,
                                    binding.runtime_kind,
                                    &rollout_path,
                                    observed_at,
                                );
                                let expected = inflight::InflightTurnIdentity::from_state(&inflight);
                                let mut repaired = inflight;
                                repaired.output_path =
                                    rollout_path.to_str().map(ToString::to_string);
                                repaired.last_offset = line_end_offset;
                                repaired.turn_start_offset = Some(line_end_offset);
                                repaired.session_key = lease.session_key.clone();
                                repaired.runtime_kind = lease.runtime_kind;
                                repaired.set_relay_owner_kind(RelayOwnerKind::None);
                                let outcome = inflight::save_inflight_state_if_identity_matches_allow_output_restamp(
                                    &repaired,
                                    &expected,
                                    "codex_idle_rollout_repair",
                                );
                                if !matches!(outcome, inflight::GuardedSaveOutcome::Saved) {
                                    tracing::warn!(
                                        tmux_session_name = %tmux_session_name,
                                        channel_id = channel_id.get(),
                                        rollout_path = %rollout_path.display(),
                                        ?outcome,
                                        "skipped Codex ownerless external-input inflight repair"
                                    );
                                    continue;
                                }
                                advance_codex_tui_runtime_binding_and_marker_offset(
                                    &tmux_session_name,
                                    &rollout_path,
                                    line_end_offset,
                                );
                                active_tails.insert(tmux_session_name.clone());
                                let shared_for_tail = shared.clone();
                                let done_tx_for_tail = done_tx.clone();
                                let tail_tmux_session_name = tmux_session_name.clone();
                                let tail_rollout_path = rollout_path.clone();
                                let tail_lease = lease.clone();
                                let tail_span = tracing::info_span!(
                                    "codex_idle_response_tail_repair",
                                    provider = ProviderKind::Codex.as_str(),
                                    channel_id = channel_id.get(),
                                    tmux_session_name = %tmux_session_name,
                                    turn_id = lease.turn_id.as_deref().unwrap_or(""),
                                    session_key = lease.session_key.as_deref().unwrap_or(""),
                                    relay_owner = lease.relay_owner.as_str(),
                                    runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                                );
                                tracing::warn!(
                                    tmux_session_name = %tmux_session_name,
                                    channel_id = channel_id.get(),
                                    rollout_path = %rollout_path.display(),
                                    line_end_offset,
                                    "repaired Codex ownerless external-input inflight and resumed response tail from live rollout"
                                );
                                task_supervisor::spawn_observed(
                                    "codex_idle_response_tail_repair",
                                    async move {
                                        let _done_guard = CodexIdleTailDoneGuard {
                                            tmux_session_name: Some(tail_tmux_session_name.clone()),
                                            done_tx: done_tx_for_tail,
                                        };
                                        run_codex_idle_response_tail(
                                            shared_for_tail,
                                            tail_tmux_session_name,
                                            channel_id,
                                            tail_rollout_path,
                                            line_end_offset,
                                            prompt,
                                            tail_lease,
                                        )
                                        .await;
                                    }
                                    .instrument(tail_span),
                                );
                                continue;
                            }
                            Ok(Some(CodexIdleRolloutScan::NoPrompt { .. })) => {}
                            Ok(None) => {
                                tracing::debug!(
                                    tmux_session_name = %tmux_session_name,
                                    channel_id = channel_id.get(),
                                    rollout_path = %rollout_path.display(),
                                    "Codex ownerless external-input inflight repair skipped; prompt not found in rollout"
                                );
                            }
                            Err(error) => {
                                tracing::warn!(
                                    tmux_session_name = %tmux_session_name,
                                    channel_id = channel_id.get(),
                                    rollout_path = %rollout_path.display(),
                                    error = %error,
                                    "Codex ownerless external-input inflight repair scan failed"
                                );
                            }
                        }
                    }
                    if !inflight::ownerless_external_input_inflight_is_stale(&inflight) {
                        continue;
                    }
                    tracing::debug!(
                        channel_id = channel_id.get(),
                        tmux_session_name = %tmux_session_name,
                        user_msg_id = inflight.user_msg_id,
                        updated_at = %inflight.updated_at,
                        "codex idle rollout relay ignored stale ownerless TUI-direct inflight blocker"
                    );
                }

                let scan =
                    match scan_codex_idle_rollout_for_prompt(&rollout_path, binding.last_offset) {
                        Ok(scan) => scan,
                        Err(error) => {
                            tracing::debug!(
                                tmux_session_name = %tmux_session_name,
                                rollout_path = %rollout_path.display(),
                                error = %error,
                                "codex idle rollout relay scan skipped"
                            );
                            continue;
                        }
                    };

                match scan {
                    CodexIdleRolloutScan::NoPrompt { offset } => {
                        if offset != binding.last_offset {
                            advance_codex_tui_runtime_binding_and_marker_offset(
                                &tmux_session_name,
                                &rollout_path,
                                offset,
                            );
                        }
                    }
                    CodexIdleRolloutScan::Prompt {
                        prompt,
                        line_end_offset,
                        entry_id,
                    } => {
                        let observed_at = chrono::Utc::now();
                        let observation =
                            crate::services::tui_prompt_dedupe::observe_prompt_by_tmux_with_entry_id_at(
                                ProviderKind::Codex.as_str(),
                                &tmux_session_name,
                                &prompt,
                                entry_id.as_deref(),
                                observed_at,
                            );
                        tracing::info!(
                            tmux_session_name = %tmux_session_name,
                            channel_id = channel_id.get(),
                            observation = ?observation,
                            entry_id = entry_id.as_deref().unwrap_or(""),
                            "codex idle rollout relay observed prompt"
                        );
                        if !codex_idle_prompt_observation_should_tail_response(observation) {
                            advance_codex_tui_runtime_binding_and_marker_offset(
                                &tmux_session_name,
                                &rollout_path,
                                line_end_offset,
                            );
                            continue;
                        }
                        let lease = record_external_turn_lease_for_output(
                            &shared,
                            &ProviderKind::Codex,
                            channel_id,
                            &tmux_session_name,
                            binding.runtime_kind,
                            &rollout_path,
                            observed_at,
                        );
                        tracing::info!(
                            tmux_session_name = %tmux_session_name,
                            channel_id = channel_id.get(),
                            turn_id = lease.turn_id.as_deref().unwrap_or(""),
                            session_key = lease.session_key.as_deref().unwrap_or(""),
                            relay_owner = lease.relay_owner.as_str(),
                            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                            "codex idle rollout relay selected external turn owner"
                        );
                        if wait_for_tui_direct_synthetic_non_bridge_claim(
                            &ProviderKind::Codex,
                            channel_id,
                            &tmux_session_name,
                        )
                        .await
                        {
                            advance_codex_tui_runtime_binding_and_marker_offset(
                                &tmux_session_name,
                                &rollout_path,
                                line_end_offset,
                            );
                            tracing::info!(
                                tmux_session_name = %tmux_session_name,
                                channel_id = channel_id.get(),
                                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                                session_key = lease.session_key.as_deref().unwrap_or(""),
                                "codex idle rollout relay yielded to resolved TUI-direct synthetic non-bridge owner"
                            );
                            continue;
                        }
                        if !bridge_adapter_owns_external_turn(lease.relay_owner) {
                            advance_codex_tui_runtime_binding_and_marker_offset(
                                &tmux_session_name,
                                &rollout_path,
                                line_end_offset,
                            );
                            tracing::debug!(
                                tmux_session_name = %tmux_session_name,
                                channel_id = channel_id.get(),
                                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                                session_key = lease.session_key.as_deref().unwrap_or(""),
                                relay_owner = lease.relay_owner.as_str(),
                                runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                                "codex idle rollout relay yielded response tail to selected owner"
                            );
                            continue;
                        }

                        advance_codex_tui_runtime_binding_and_marker_offset(
                            &tmux_session_name,
                            &rollout_path,
                            line_end_offset,
                        );
                        active_tails.insert(tmux_session_name.clone());
                        let shared_for_tail = shared.clone();
                        let done_tx_for_tail = done_tx.clone();
                        let tail_tmux_session_name = tmux_session_name.clone();
                        let tail_rollout_path = rollout_path.clone();
                        let tail_lease = lease.clone();
                        let tail_span = tracing::info_span!(
                            "codex_idle_response_tail",
                            provider = ProviderKind::Codex.as_str(),
                            channel_id = channel_id.get(),
                            tmux_session_name = %tmux_session_name,
                            turn_id = lease.turn_id.as_deref().unwrap_or(""),
                            session_key = lease.session_key.as_deref().unwrap_or(""),
                            relay_owner = lease.relay_owner.as_str(),
                            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                        );
                        task_supervisor::spawn_observed(
                            "codex_idle_response_tail",
                            async move {
                                let _done_guard = CodexIdleTailDoneGuard {
                                    tmux_session_name: Some(tail_tmux_session_name.clone()),
                                    done_tx: done_tx_for_tail,
                                };
                                run_codex_idle_response_tail(
                                    shared_for_tail,
                                    tail_tmux_session_name,
                                    channel_id,
                                    tail_rollout_path,
                                    line_end_offset,
                                    prompt,
                                    tail_lease,
                                )
                                .await;
                            }
                            .instrument(tail_span),
                        );
                    }
                }
            }

            tokio::time::sleep(CODEX_IDLE_ROLLOUT_POLL_INTERVAL).await;
        }
    }
    .instrument(tracing::info_span!(
        "codex_idle_rollout_relay",
        provider = ProviderKind::Codex.as_str(),
        runtime_kind = RuntimeHandoffKind::CodexTui.as_str(),
    )));
}

#[cfg(unix)]
async fn run_codex_idle_response_tail(
    shared: Arc<SharedData>,
    tmux_session_name: String,
    channel_id: ChannelId,
    rollout_path: PathBuf,
    start_offset: u64,
    prompt_text: String,
    lease: ExternalInputRelayLease,
) {
    let _lease_guard = TuiDirectExternalInputLeaseGuard::new(
        ProviderKind::Codex,
        &tmux_session_name,
        channel_id,
        &lease,
    );
    let (reader_tx, reader_rx) = mpsc::channel::<StreamMessage>();
    let (offset_tx, offset_rx) = tokio::sync::oneshot::channel::<Result<u64, String>>();
    let rollout_for_reader = rollout_path.clone();
    let tmux_for_reader = tmux_session_name.clone();
    std::thread::Builder::new()
        .name("codex_idle_response_tail_reader".to_string())
        .spawn(move || {
            let read_result =
                crate::services::codex_tui::rollout_tail::tail_rollout_file_from_offset(
                    &rollout_for_reader,
                    start_offset,
                    None,
                    reader_tx,
                    None,
                    || {
                        crate::services::tmux_diagnostics::tmux_session_has_live_pane(
                            &tmux_for_reader,
                        )
                    },
                )
                .map(|result| match result {
                    ReadOutputResult::Completed { offset }
                    | ReadOutputResult::Cancelled { offset }
                    | ReadOutputResult::SessionDied { offset } => offset,
                });
            let _ = offset_tx.send(read_result);
        })
        .expect("spawn codex idle response tail reader thread");

    let buffered = tokio::task::spawn_blocking(move || {
        let mut prefix: Vec<StreamMessage> = Vec::new();
        let mut has_content = false;
        while let Ok(message) = reader_rx.recv() {
            let is_content = idle_stream_message_is_content(&message);
            let is_terminal = matches!(message, StreamMessage::Done { .. });
            prefix.push(message);
            if is_content {
                has_content = true;
                break;
            }
            if is_terminal {
                break;
            }
        }
        (prefix, has_content, reader_rx)
    })
    .await;

    let (prefix, has_content, reader_rx) = match buffered {
        Ok(buffered) => buffered,
        Err(error) => {
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                rollout_path = %rollout_path.display(),
                error = %error,
                "codex idle rollout response tail buffering panicked"
            );
            finish_tui_direct_synthetic_turn_if_current(
                &shared,
                &ProviderKind::Codex,
                channel_id,
                &tmux_session_name,
                lease.session_key.as_deref(),
                "codex_tui_direct_tail_panicked",
            )
            .await;
            return;
        }
    };

    if !has_content {
        let _ = tokio::task::spawn_blocking(move || while reader_rx.recv().is_ok() {}).await;
        if let Ok(Ok(final_offset)) = offset_rx.await {
            advance_codex_tui_runtime_binding_and_marker_offset(
                &tmux_session_name,
                &rollout_path,
                final_offset,
            );
        }
        finish_tui_direct_synthetic_turn_if_current(
            &shared,
            &ProviderKind::Codex,
            channel_id,
            &tmux_session_name,
            lease.session_key.as_deref(),
            "codex_tui_direct_empty_response",
        )
        .await;
        return;
    }
    let delivery_result = stream_tui_idle_response_through_bridge(
        &shared,
        ProviderKind::Codex,
        channel_id,
        &tmux_session_name,
        &rollout_path,
        start_offset,
        &prompt_text,
        prefix,
        reader_rx,
        &lease,
    )
    .await;
    if delivery_result.is_err() {
        finish_tui_direct_synthetic_turn_if_current(
            &shared,
            &ProviderKind::Codex,
            channel_id,
            &tmux_session_name,
            lease.session_key.as_deref(),
            "codex_tui_direct_delivery_failed",
        )
        .await;
    }
    match offset_rx.await {
        Ok(Ok(final_offset))
            if tui_idle_tail_stream_should_commit_runtime_binding_offset(
                delivery_result.is_ok(),
            ) =>
        {
            advance_codex_tui_runtime_binding_and_marker_offset(
                &tmux_session_name,
                &rollout_path,
                final_offset,
            );
        }
        Ok(Err(error)) => {
            tracing::warn!(
                tmux_session_name = %tmux_session_name,
                rollout_path = %rollout_path.display(),
                error = %error,
                "codex idle rollout response tail failed"
            );
            finish_tui_direct_synthetic_turn_if_current(
                &shared,
                &ProviderKind::Codex,
                channel_id,
                &tmux_session_name,
                lease.session_key.as_deref(),
                "codex_tui_direct_tail_failed",
            )
            .await;
        }
        _ => {}
    }
}

#[cfg(unix)]
#[allow(dead_code)]
fn collect_codex_idle_response(
    rollout_path: PathBuf,
    start_offset: u64,
    tmux_session_name: String,
) -> Result<(String, u64), String> {
    let (tx, rx) = mpsc::channel();
    let read_result = crate::services::codex_tui::rollout_tail::tail_rollout_file_from_offset(
        &rollout_path,
        start_offset,
        None,
        tx,
        None,
        || crate::services::tmux_diagnostics::tmux_session_has_live_pane(&tmux_session_name),
    )?;

    let mut streamed = String::new();
    let mut done_result: Option<String> = None;
    let mut error_result: Option<String> = None;
    let mut sideband = Vec::new();
    for message in rx.try_iter() {
        match message {
            StreamMessage::Text { content } => streamed.push_str(&content),
            StreamMessage::Done { result, .. } => done_result = Some(result),
            StreamMessage::Error {
                message, stderr, ..
            } => {
                let mut combined = message;
                if !stderr.trim().is_empty() {
                    combined.push_str("\n");
                    combined.push_str(stderr.trim());
                }
                error_result = Some(combined);
            }
            StreamMessage::TaskNotification {
                status, summary, ..
            } => {
                if !summary.trim().is_empty() {
                    sideband.push(format!("[{status}] {summary}"));
                }
            }
            _ => {}
        }
    }

    let offset = match read_result {
        ReadOutputResult::Completed { offset }
        | ReadOutputResult::Cancelled { offset }
        | ReadOutputResult::SessionDied { offset } => offset,
    };
    let response = compose_tui_idle_response(done_result, error_result, streamed, sideband);
    Ok((response, offset))
}
