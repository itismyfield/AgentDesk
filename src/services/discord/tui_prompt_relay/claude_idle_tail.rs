use super::*;

#[cfg(unix)]
pub(super) async fn maybe_spawn_claude_idle_response_tail(
    shared: Arc<SharedData>,
    channel_id: ChannelId,
    prompt: &ObservedTuiPrompt,
    lease: &ExternalInputRelayLease,
    current_turn_anchor_id: Option<u64>,
    // #3154 P1 (timestamp-anchor output loss): when `Some`, anchor the tail's
    // start to THIS explicit transcript byte offset (the deferred claim's
    // post-drain EOF `turn_start_offset`) and SKIP the `observed_at` timestamp
    // scan. The timestamp scan picks the first transcript line at/after
    // `prompt.observed_at`; for the worker-spawned deferred-BridgeAdapter path
    // that timestamp is a `Utc::now()` synthesized AFTER the claim wait, so the
    // scan skips every byte written during the wait window — those bytes belong
    // to this synthetic turn and would be lost. The post-drain EOF offset is the
    // exact turn boundary (no skip, no re-relay of prior-turn bytes). `None`
    // preserves the original timestamp-scan behaviour for the inline /
    // non-deferred path.
    explicit_start_offset: Option<u64>,
) -> bool {
    if !prompt
        .provider
        .trim()
        .eq_ignore_ascii_case(ProviderKind::Claude.as_str())
    {
        return false;
    }
    if !bridge_adapter_owns_external_turn(lease.relay_owner) {
        tracing::debug!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            tmux_session_name = %prompt.tmux_session_name,
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
            "skipping Claude idle response tail; external turn has another relay owner"
        );
        return false;
    }
    if !wait_for_claude_inflight_to_clear(
        channel_id,
        &prompt.tmux_session_name,
        current_turn_anchor_id,
    )
    .await
    {
        tracing::warn!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            tmux_session_name = %prompt.tmux_session_name,
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
            wait_ms = CLAUDE_IDLE_INFLIGHT_DRAIN_WAIT.as_millis(),
            "skipping Claude idle response tail; previous inflight did not drain"
        );
        return false;
    }
    let Some(binding) = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
        &prompt.tmux_session_name,
    ) else {
        tracing::debug!(
            tmux_session_name = %prompt.tmux_session_name,
            "skipping Claude idle response tail; no runtime binding"
        );
        return false;
    };
    if binding.runtime_kind != RuntimeHandoffKind::ClaudeTui {
        return false;
    }

    // #2843: resolve the freshest active transcript (the bound output_path can be
    // stale) and only let a non-stale tmux watcher suppress the tail when it
    // actually covers that transcript. Re-registers the binding if it changed.
    let Some(transcript_path) = resolve_idle_relay_transcript(
        &shared,
        &prompt.tmux_session_name,
        channel_id,
        &binding,
        false,
    ) else {
        return false;
    };

    // #2843: if the path changed, don't trust the old binding offset (it indexes
    // a different transcript and would replay old output); the timestamp-based
    // resolution still takes precedence, falling back to the fresh EOF.
    let fallback_offset = if Path::new(&binding.output_path) == transcript_path {
        binding.last_offset
    } else {
        claude_tui_rehydrate_start_offset(&transcript_path)
    };
    let start_offset = resolve_idle_tail_start_offset(
        &transcript_path,
        explicit_start_offset,
        prompt.observed_at,
        fallback_offset,
    );
    spawn_claude_idle_response_tail_once(
        shared,
        prompt.tmux_session_name.clone(),
        channel_id,
        transcript_path,
        start_offset,
        prompt.prompt.clone(),
        lease.clone(),
    )
}

#[cfg(unix)]
/// #3176: is the present inflight THIS turn's own TUI-direct synthetic row?
///
/// The drain-wait must only skip waiting on the inflight WE just created for the
/// current turn. The discriminator is the precise current-turn identity —
/// `ExternalInput` + same tmux session + `user_msg_id == this turn's anchor
/// message id` — NOT merely same-session `ExternalInput` (which would also match a
/// still-draining PREVIOUS same-session TUI turn and wrongly skip it, risking
/// interleaved or lost delivery). When the current turn created no synthetic
/// (`current_turn_anchor_id == None` — e.g. system-continuation / slash-control
/// paths), nothing here is "ours", so any present inflight remains a previous turn
/// and still blocks.
#[cfg(unix)]
pub(super) fn inflight_is_current_turn_synthetic(
    state: Option<&InflightTurnState>,
    tmux_session_name: &str,
    current_turn_anchor_id: Option<u64>,
) -> bool {
    match (state, current_turn_anchor_id) {
        (Some(state), Some(anchor_id)) => {
            state.turn_source == TurnSource::ExternalInput
                && state.tmux_session_name.as_deref() == Some(tmux_session_name)
                && state.user_msg_id == anchor_id
        }
        _ => false,
    }
}

#[cfg(unix)]
pub(super) async fn wait_for_claude_inflight_to_clear(
    channel_id: ChannelId,
    tmux_session_name: &str,
    current_turn_anchor_id: Option<u64>,
) -> bool {
    let mut observed_inflight = false;
    let cleared = wait_for_transient_state_to_clear(
        CLAUDE_IDLE_INFLIGHT_DRAIN_WAIT,
        CLAUDE_IDLE_INFLIGHT_DRAIN_POLL,
        || {
            // #3176: a present inflight only BLOCKS this turn's idle tail when it
            // belongs to a DIFFERENT (previous) turn. Our OWN synthetic for THIS turn
            // (created upstream in the notify/anchor block) must not be waited on —
            // doing so self-deadlocks (we created it; it never "drains" within the
            // window), permanently skipping the relay and silently dropping every
            // subsequent response. Identity-pinned via `inflight_is_current_turn_synthetic`.
            let state = super::super::inflight::load_inflight_state(
                &ProviderKind::Claude,
                channel_id.get(),
            );
            let blocking = state.is_some()
                && !inflight_is_current_turn_synthetic(
                    state.as_ref(),
                    tmux_session_name,
                    current_turn_anchor_id,
                );
            observed_inflight |= blocking;
            blocking
        },
    )
    .await;
    if observed_inflight && cleared {
        tracing::info!(
            provider = ProviderKind::Claude.as_str(),
            channel_id = channel_id.get(),
            wait_ms = CLAUDE_IDLE_INFLIGHT_DRAIN_WAIT.as_millis(),
            "Claude idle response tail waited for previous inflight to drain"
        );
    }
    cleared
}

#[cfg(unix)]
pub(super) async fn wait_for_transient_state_to_clear<F>(
    timeout: Duration,
    poll_interval: Duration,
    mut is_present: F,
) -> bool
where
    F: FnMut() -> bool,
{
    if !is_present() {
        return true;
    }

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return !is_present();
        }
        tokio::time::sleep(poll_interval.min(deadline - now)).await;
        if !is_present() {
            return true;
        }
    }
}

#[cfg(unix)]
pub(super) fn spawn_claude_idle_response_tail_once(
    shared: Arc<SharedData>,
    tmux_session_name: String,
    channel_id: ChannelId,
    transcript_path: PathBuf,
    start_offset: u64,
    prompt_text: String,
    lease: ExternalInputRelayLease,
) -> bool {
    // #3183: never re-relay output the watcher already committed delivery for. Both spawn paths
    // (observed-prompt + background poll loop) funnel through here, so clamping once covers both.
    // #3183 codex (CRITICAL outage-safety): `committed_relay_offset` is a
    // PER-CHANNEL watermark, not per-transcript. A stale-high watermark left by a
    // PREVIOUS wrapper (e.g. 5000) would, after a respawn whose fresh transcript
    // starts near 0, clamp this tail forward and SKIP the new turn's response —
    // exactly the relay-loss the idle tail exists to prevent. Run the SAME
    // generation-aware regression resets the watcher / idle-JSONL sink run BEFORE
    // consulting the watermark (session_relay_sink.rs): a truncated/respawned
    // transcript (EOF below the watermark) or a wrapper-generation change resets the
    // watermark to 0, so the fresh range is relayed; only a watermark that genuinely covers THIS transcript clamps (dedupe).
    let transcript_len = std::fs::metadata(&transcript_path)
        .map(|meta| meta.len())
        .unwrap_or(0);
    super::super::tmux::reset_stale_relay_watermark_if_output_regressed(
        shared.as_ref(),
        channel_id,
        &tmux_session_name,
        transcript_len,
        "idle_response_tail",
    );
    super::super::tmux::reset_relay_watermark_on_generation_change(
        shared.as_ref(),
        channel_id,
        &tmux_session_name,
        "idle_response_tail",
    );
    // #3089 B2c (#3235): durable-frontier dedup clamp (flag OFF → in-memory) survives restart.
    let committed_offset = dr::effective_committed_offset(
        &shared,
        &ProviderKind::Claude,
        channel_id,
        &tmux_session_name,
    );
    let start_offset = clamp_idle_tail_start_offset_to_committed(start_offset, committed_offset);
    if committed_offset > 0 {
        tracing::debug!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            committed_offset,
            start_offset,
            "Claude idle response tail start offset clamped to watcher committed delivery offset"
        );
    }
    {
        let mut active = CLAUDE_IDLE_RESPONSE_TAILS
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !active.insert(tmux_session_name.clone()) {
            return false;
        }
    }

    let span = tracing::info_span!(
        "claude_idle_response_tail",
        provider = ProviderKind::Claude.as_str(),
        channel_id = channel_id.get(),
        tmux_session_name = %tmux_session_name,
        turn_id = lease.turn_id.as_deref().unwrap_or(""),
        session_key = lease.session_key.as_deref().unwrap_or(""),
        relay_owner = lease.relay_owner.as_str(),
        runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
    );
    super::super::task_supervisor::spawn_observed(
        "claude_idle_response_tail",
        async move {
            let _tail_guard = ClaudeIdleTailGuard {
                tmux_session_name: tmux_session_name.clone(),
            };
            run_claude_idle_response_tail(
                shared,
                tmux_session_name.clone(),
                channel_id,
                transcript_path,
                start_offset,
                prompt_text,
                lease,
            )
            .await;
        }
        .instrument(span),
    );
    true
}

#[cfg(unix)]
pub(super) async fn run_claude_idle_response_tail(
    shared: Arc<SharedData>,
    tmux_session_name: String,
    channel_id: ChannelId,
    transcript_path: PathBuf,
    start_offset: u64,
    prompt_text: String,
    lease: ExternalInputRelayLease,
) {
    let _lease_guard = TuiDirectExternalInputLeaseGuard::new(
        ProviderKind::Claude,
        &tmux_session_name,
        channel_id,
        &lease,
    );

    // #3256: STREAM the operator's external-input prose THROUGH a single bridge
    // turn instead of pre-collecting the whole response and posting it as one
    // batched `[Text{full}, Done]` at turn end. The transcript reader
    // (`read_output_file_until_result`) already emits each `StreamMessage`
    // (Text / ToolUse / ToolResult / Done / OutputOffset) in real time; we feed
    // those frames into the SAME bridge `tx` a normal Discord turn uses, so a
    // LONG continuous autonomous turn relays prose progressively within ONE
    // intake card / ONE `spawn_turn_bridge` instead of accumulating until the
    // next user-message turn boundary.
    //
    // The reader runs on a blocking OS thread; it forwards every frame onto an
    // intermediate channel (`reader_rx`) and reports the final transcript byte
    // offset over `offset_tx` after it returns (done / idle / dead). We BUFFER
    // the leading frames until the first content frame arrives so that a turn
    // that produces NO prose still takes the original empty-response path (no
    // intake card, just advance the binding offset + finish the synthetic turn)
    // — preserving today's behavior for the common no-op case.
    let (reader_tx, reader_rx) = mpsc::channel::<StreamMessage>();
    let (offset_tx, offset_rx) = tokio::sync::oneshot::channel::<Result<u64, String>>();
    let transcript_for_reader = transcript_path.clone();
    let tmux_for_reader = tmux_session_name.clone();
    std::thread::Builder::new()
        .name("claude_idle_response_tail_reader".to_string())
        .spawn(move || {
            let transcript_string = transcript_for_reader.display().to_string();
            let read_result = crate::services::session_backend::read_output_file_until_result(
                &transcript_string,
                start_offset,
                reader_tx,
                None,
                crate::services::provider::SessionProbe::tmux(
                    tmux_for_reader,
                    ProviderKind::Claude,
                ),
            );
            let offset_result = read_result.map(|result| match result {
                ReadOutputResult::Completed { offset }
                | ReadOutputResult::Cancelled { offset }
                | ReadOutputResult::SessionDied { offset } => offset,
            });
            let _ = offset_tx.send(offset_result);
        })
        .expect("spawn claude idle response tail reader thread");

    // Buffer leading frames on the blocking pool until the first content frame
    // (or the reader closes). `prefix` carries the frames already pulled,
    // `has_content` tells us whether the bridge should run, and we hand the live
    // `reader_rx` back to drain the remainder into the bridge.
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
                transcript_path = %transcript_path.display(),
                error = %error,
                "Claude idle transcript response tail buffering panicked"
            );
            finish_tui_direct_synthetic_turn_if_current(
                &shared,
                &ProviderKind::Claude,
                channel_id,
                &tmux_session_name,
                "claude_tui_direct_tail_panicked",
            )
            .await;
            return;
        }
    };

    if !has_content {
        // No prose / no terminal body for this turn: keep today's no-card empty
        // path. Drain any residual frames so the reader thread can finish, then
        // commit the binding offset.
        let _ = tokio::task::spawn_blocking(move || while reader_rx.recv().is_ok() {}).await;
        if let Ok(Ok(final_offset)) = offset_rx.await {
            advance_claude_tmux_runtime_binding_offset(
                &tmux_session_name,
                &transcript_path,
                final_offset,
            );
        }
        finish_tui_direct_synthetic_turn_if_current(
            &shared,
            &ProviderKind::Claude,
            channel_id,
            &tmux_session_name,
            "claude_tui_direct_empty_response",
        )
        .await;
        return;
    }

    let delivery_result = stream_tui_idle_response_through_bridge(
        &shared,
        ProviderKind::Claude,
        channel_id,
        &tmux_session_name,
        &transcript_path,
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
            &ProviderKind::Claude,
            channel_id,
            &tmux_session_name,
            "claude_tui_direct_delivery_failed",
        )
        .await;
    }
    // #3041 / #3256: advance the runtime-binding offset on successful delivery so
    // the watcher / idle paths never double-send this turn's bytes. The reader
    // reports the authoritative final offset over `offset_rx`.
    let final_offset = match offset_rx.await {
        Ok(Ok(offset)) => Some(offset),
        _ => None,
    };
    if let Some(final_offset) = final_offset
        && tui_idle_tail_stream_should_commit_runtime_binding_offset(delivery_result.is_ok())
    {
        advance_claude_tmux_runtime_binding_offset(
            &tmux_session_name,
            &transcript_path,
            final_offset,
        );
    }
}
