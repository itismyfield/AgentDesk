use super::*;

#[cfg(unix)]
#[derive(Clone)]
struct IdleStreamFrameLogContext {
    provider: String,
    channel_id: u64,
    tmux_session_name: String,
    mailbox_owner_user_msg_id: u64,
    inflight_user_msg_id: u64,
    inflight_current_msg_id: u64,
}

#[cfg(unix)]
fn log_idle_stream_text_decision(
    ctx: Option<&IdleStreamFrameLogContext>,
    decision: &'static str,
    content_len: usize,
) {
    if let Some(ctx) = ctx {
        tracing::debug!(
            provider = %ctx.provider,
            channel_id = ctx.channel_id,
            tmux_session_name = %ctx.tmux_session_name,
            mailbox_owner_user_msg_id = ctx.mailbox_owner_user_msg_id,
            inflight_user_msg_id = ctx.inflight_user_msg_id,
            inflight_current_msg_id = ctx.inflight_current_msg_id,
            text_len = content_len,
            decision,
            "idle-tail text frame relay decision"
        );
    }
}

/// #3256: a transcript-reader frame counts as "content" for the idle-tail
/// stream-through when it carries body the operator actually produced — prose
/// (`Text`), an authoritative terminal body (`Done` with a non-empty result),
/// or a transport error. A bare terminal `Done` with an empty result (the
/// synthetic completion frame the reader emits at turn end) or pure control /
/// offset frames are NOT content; if the whole turn yields only those, the
/// idle tail takes the no-card empty path (preserving today's behavior).
#[cfg(unix)]
pub(super) fn idle_stream_message_is_content(message: &StreamMessage) -> bool {
    match message {
        // #3256: a `Text`/`Done` body that is ONLY leading TUI chrome (e.g.
        // `No response requested.` / `Continue from where you left off.`) is NOT
        // real content — the old path stripped that chrome with
        // `strip_leading_tui_response_chrome` and produced an empty response, i.e.
        // the no-card empty path. Strip BEFORE the emptiness test so a chrome-only
        // turn keeps spawning no placeholder card (parity with prior behavior).
        StreamMessage::Text { content } => {
            !super::super::response_sanitizer::strip_leading_tui_response_chrome(content)
                .trim()
                .is_empty()
        }
        StreamMessage::Done { result, .. } => {
            !super::super::response_sanitizer::strip_leading_tui_response_chrome(result)
                .trim()
                .is_empty()
        }
        StreamMessage::Error { message, .. } => !message.trim().is_empty(),
        _ => false,
    }
}

/// #3256: the stream-through path commits the runtime-binding offset whenever
/// the single bridge turn delivered successfully. (The empty-response branch
/// commits independently before finishing the synthetic turn.)
#[cfg(unix)]
pub(super) fn tui_idle_tail_stream_should_commit_runtime_binding_offset(
    discord_delivery_succeeded: bool,
) -> bool {
    discord_delivery_succeeded
}

#[cfg(unix)]
pub(super) fn compose_tui_idle_response(
    done_result: Option<String>,
    error_result: Option<String>,
    streamed: String,
    sideband: Vec<String>,
) -> String {
    let body = done_result
        .or(error_result)
        .filter(|text| !text.trim().is_empty())
        .unwrap_or(streamed);
    let body = super::super::response_sanitizer::strip_leading_tui_response_chrome(&body);
    let sideband = sideband
        .into_iter()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    if sideband.is_empty() {
        body
    } else if body.trim().is_empty() {
        sideband.join("\n")
    } else {
        format!("{}\n\n{}", sideband.join("\n"), body)
    }
}

#[cfg(unix)]
fn codex_external_input_bridge_stream_messages(
    response: &str,
    final_offset: u64,
) -> Vec<StreamMessage> {
    let mut messages = Vec::new();
    if !response.trim().is_empty() {
        messages.push(StreamMessage::Text {
            content: response.to_string(),
        });
    }
    messages.push(StreamMessage::OutputOffset {
        offset: final_offset,
    });
    messages.push(StreamMessage::Done {
        result: response.to_string(),
        session_id: None,
    });
    messages
}

#[cfg(unix)]
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn relay_tui_idle_response_through_bridge(
    shared: &Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &Path,
    start_offset: u64,
    // #3089 A6b r2 [High]/#3998 S1-f2: the tail's authoritative end offset.
    // Plumbed into the bridge stream as `OutputOffset` so codex external-input's
    // `ordered_range` becomes true and the unconditional A5 controller route is
    // structurally eligible.
    final_offset: u64,
    prompt_text: &str,
    response: &str,
    lease: &ExternalInputRelayLease,
) -> Result<(), String> {
    let _lease_guard = TuiDirectExternalInputLeaseGuard::new(
        provider.clone(),
        tmux_session_name,
        channel_id,
        lease,
    );
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        tracing::warn!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            provider = %provider.as_str(),
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
            "skipping TUI idle response relay; Discord HTTP unavailable"
        );
        return Err(format!(
            "discord http unavailable for provider {}",
            provider.as_str()
        ));
    };
    // #3097: resolve the provider-specific compact threshold so the status panel reflects the configured value (e.g. `context_compact_percent_claude`) instead of the hardcoded 0 it used previously.
    let context_compact_percent =
        super::super::adk_session::fetch_context_thresholds(shared.api_port)
            .await
            .compact_pct_for(&provider);
    let anchor = prompt_anchor_for_response_after_wait(
        provider.as_str(),
        tmux_session_name,
        channel_id.get(),
    )
    .await;
    let reference = anchor.map(|anchor| {
        (
            ChannelId::new(anchor.channel_id),
            MessageId::new(anchor.message_id),
        )
    });
    let current_msg_id = super::super::gateway::send_intake_placeholder(
        http.clone(),
        shared.clone(),
        channel_id,
        reference,
        // #3082 P2-3: a TUI idle-response placeholder is an ACTIVE-turn card, not a queued "📬" notice — it must not wait on the answer-flush barrier.
        false,
    )
    .await?;
    let user_msg_id = anchor
        .map(|anchor| MessageId::new(anchor.message_id))
        .unwrap_or(current_msg_id);
    let (tx, rx) = mpsc::channel();
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    let inflight_state = build_tui_direct_bridge_inflight_state(
        provider.clone(),
        channel_id,
        user_msg_id,
        current_msg_id,
        prompt_text,
        tmux_session_name,
        output_path,
        start_offset,
        lease,
    );
    let bridge = TurnBridgeContext {
        provider: provider.clone(),
        gateway: Arc::new(TuiDirectBridgeGateway {
            http,
            shared: shared.clone(),
            provider: provider.clone(),
        }),
        channel_id,
        user_msg_id: Some(user_msg_id),
        user_text_owned: prompt_text.to_string(),
        request_owner_name: "TUI direct".to_string(),
        role_binding: None,
        adk_session_key: lease.session_key.clone(),
        adk_session_name: Some(tmux_session_name.to_string()),
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        memory_recall_usage: TokenUsage::default(),
        context_window_tokens: 0,
        context_compact_percent,
        current_msg_id: Some(current_msg_id),
        response_sent_offset: 0,
        full_response: String::new(),
        tmux_last_offset: Some(start_offset),
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: Some(completion_tx),
        is_external_input_tui_direct: true, // #3959: suppress mirror chrome footer
        inflight_state,
    };

    spawn_turn_bridge(shared.clone(), Arc::new(CancelToken::new()), rx, bridge);
    // #3089 A6b r2 [High]/#3998 S1-f2: feed the bridge
    // `[Text?, OutputOffset, Done]`. `OutputOffset` advances `tmux_last_offset`
    // to `final_offset` so codex external-input's `ordered_range` is true and
    // the A5 controller route is structurally eligible.
    for message in codex_external_input_bridge_stream_messages(response, final_offset) {
        tx.send(message)
            .map_err(|error| format!("send TUI-direct bridge stream event: {error}"))?;
    }
    drop(tx);

    match tokio::time::timeout(Duration::from_secs(180), completion_rx).await {
        Ok(_) => {
            ensure_tui_direct_bridge_delivery_committed(
                &provider,
                channel_id,
                user_msg_id,
                current_msg_id,
                tmux_session_name,
                lease,
                anchor.map(|anchor| anchor.message_id),
                false,
            )?;
            if let Some(anchor) = anchor {
                crate::services::tui_prompt_dedupe::clear_prompt_anchor_for_response(
                    provider.as_str(),
                    tmux_session_name,
                    anchor,
                );
            }
            tracing::info!(
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                provider = %provider.as_str(),
                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                session_key = lease.session_key.as_deref().unwrap_or(""),
                relay_owner = lease.relay_owner.as_str(),
                runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                current_msg_id = current_msg_id.get(),
                prompt_anchor_message_id = anchor.map(|anchor| anchor.message_id),
                "TUI-direct bridge adapter completed response relay"
            );
            Ok(())
        }
        Err(_) => Err(format!(
            "TUI-direct bridge adapter timed out waiting for completion for provider {}",
            provider.as_str()
        )),
    }
}

/// #3256: STREAM-THROUGH variant of `relay_tui_idle_response_through_bridge`
/// for the Claude external-input idle path.
///
/// Identical bridge setup — EXACTLY ONE intake placeholder card and EXACTLY ONE
/// `spawn_turn_bridge` per external turn — but instead of pre-collecting the
/// whole response and feeding the bridge one synthetic `[Text{full}, Done]`,
/// it forwards the transcript reader's LIVE `StreamMessage`s into the same
/// bridge `tx` AS THEY ARRIVE (`prefix` = the frames already buffered upstream,
/// including the first content frame; `reader_rx` = the remaining live stream).
/// The bridge consumes them exactly as it does for a normal Discord turn:
/// `Text` chunks edit the one card progressively, the terminal `Done`
/// finalizes the turn EXACTLY ONCE.
///
/// Behavior-preservation: for a SHORT turn the prefix + a quick `Done` arrive
/// back-to-back, so the bridge still posts one card with the full prose and
/// finalizes once — observably identical to the old collect-then-send path. The
/// only change is that a LONG turn now relays prose incrementally within that
/// one card instead of all-at-once at turn end.
#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub(super) async fn stream_tui_idle_response_through_bridge(
    shared: &Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &Path,
    start_offset: u64,
    prompt_text: &str,
    prefix: Vec<StreamMessage>,
    reader_rx: mpsc::Receiver<StreamMessage>,
    lease: &ExternalInputRelayLease,
) -> Result<(), String> {
    let _lease_guard = TuiDirectExternalInputLeaseGuard::new(
        provider.clone(),
        tmux_session_name,
        channel_id,
        lease,
    );
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        tracing::warn!(
            channel_id = channel_id.get(),
            tmux_session_name = %tmux_session_name,
            provider = %provider.as_str(),
            turn_id = lease.turn_id.as_deref().unwrap_or(""),
            session_key = lease.session_key.as_deref().unwrap_or(""),
            relay_owner = lease.relay_owner.as_str(),
            runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
            "skipping TUI idle response relay; Discord HTTP unavailable"
        );
        return Err(format!(
            "discord http unavailable for provider {}",
            provider.as_str()
        ));
    };
    // #3097: resolve the provider-specific compact threshold so the status
    // panel reflects the configured value.
    let context_compact_percent =
        super::super::adk_session::fetch_context_thresholds(shared.api_port)
            .await
            .compact_pct_for(&provider);
    let anchor = prompt_anchor_for_response_after_wait(
        provider.as_str(),
        tmux_session_name,
        channel_id.get(),
    )
    .await;
    let reference = anchor.map(|anchor| {
        (
            ChannelId::new(anchor.channel_id),
            MessageId::new(anchor.message_id),
        )
    });
    // EXACTLY ONE intake placeholder card per external turn.
    let current_msg_id = super::super::gateway::send_intake_placeholder(
        http.clone(),
        shared.clone(),
        channel_id,
        reference,
        false,
    )
    .await?;
    let user_msg_id = anchor
        .map(|anchor| MessageId::new(anchor.message_id))
        .unwrap_or(current_msg_id);
    let (tx, rx) = mpsc::channel();
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    let inflight_state = build_tui_direct_bridge_inflight_state(
        provider.clone(),
        channel_id,
        user_msg_id,
        current_msg_id,
        prompt_text,
        tmux_session_name,
        output_path,
        start_offset,
        lease,
    );
    let bridge = TurnBridgeContext {
        provider: provider.clone(),
        gateway: Arc::new(TuiDirectBridgeGateway {
            http,
            shared: shared.clone(),
            provider: provider.clone(),
        }),
        channel_id,
        user_msg_id: Some(user_msg_id),
        user_text_owned: prompt_text.to_string(),
        request_owner_name: "TUI direct".to_string(),
        role_binding: None,
        adk_session_key: lease.session_key.clone(),
        adk_session_name: Some(tmux_session_name.to_string()),
        adk_session_info: None,
        adk_cwd: None,
        dispatch_id: None,
        dispatch_kind: None,
        memory_recall_usage: TokenUsage::default(),
        context_window_tokens: 0,
        context_compact_percent,
        current_msg_id: Some(current_msg_id),
        response_sent_offset: 0,
        full_response: String::new(),
        tmux_last_offset: Some(start_offset),
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: Some(completion_tx),
        is_external_input_tui_direct: true, // #3959: suppress mirror chrome footer
        inflight_state,
    };

    // EXACTLY ONE spawn_turn_bridge per external turn.
    spawn_turn_bridge(shared.clone(), Arc::new(CancelToken::new()), rx, bridge);
    let frame_log_context = IdleStreamFrameLogContext {
        provider: provider.as_str().to_string(),
        channel_id: channel_id.get(),
        tmux_session_name: tmux_session_name.to_string(),
        mailbox_owner_user_msg_id: super::super::mailbox_snapshot(shared, channel_id)
            .await
            .active_user_message_id
            .map(|id| id.get())
            .unwrap_or(0),
        inflight_user_msg_id: user_msg_id.get(),
        inflight_current_msg_id: current_msg_id.get(),
    };

    // Forward the buffered prefix + the live reader stream into the SINGLE
    // bridge `tx` on a blocking thread (the reader receiver and the bridge
    // sender are both sync `mpsc`). The bridge finalizes on the first terminal
    // `Done`; we send a fallback `Done` only if the reader closed without one
    // so the bridge always finalizes EXACTLY ONCE.
    let forward_handle = tokio::task::spawn_blocking(move || {
        forward_idle_stream_into_bridge_with_logging(prefix, reader_rx, tx, Some(frame_log_context))
    });

    // #3256: the forward thread runs for the WHOLE turn — it only returns once the
    // transcript reader closes (turn done / idle / dead), having forwarded every
    // prose frame plus the terminal `Done` into the bridge. Join it FIRST so the
    // completion wait does not race the turn's real duration. A long autonomous
    // turn (many minutes, well past any fixed wall-clock) therefore streams in
    // full and still reports success — the previous `timeout(180s, completion_rx)`
    // placed before this join made >180s turns return `Err` despite a normal
    // delivery, which skipped the runtime-binding offset commit and risked a
    // duplicate re-relay on the next idle poll.
    let _ = forward_handle.await;

    // Only NOW bound the post-`Done` bridge finalization (Discord edit/flush),
    // which should land within seconds of the terminal frame being forwarded.
    let completion = tokio::time::timeout(Duration::from_secs(180), completion_rx).await;

    match completion {
        Ok(_) => {
            ensure_tui_direct_bridge_delivery_committed(
                &provider,
                channel_id,
                user_msg_id,
                current_msg_id,
                tmux_session_name,
                lease,
                anchor.map(|anchor| anchor.message_id),
                true,
            )?;
            if let Some(anchor) = anchor {
                crate::services::tui_prompt_dedupe::clear_prompt_anchor_for_response(
                    provider.as_str(),
                    tmux_session_name,
                    anchor,
                );
            }
            tracing::info!(
                channel_id = channel_id.get(),
                tmux_session_name = %tmux_session_name,
                provider = %provider.as_str(),
                turn_id = lease.turn_id.as_deref().unwrap_or(""),
                session_key = lease.session_key.as_deref().unwrap_or(""),
                relay_owner = lease.relay_owner.as_str(),
                runtime_kind = lease.runtime_kind.map(RuntimeHandoffKind::as_str).unwrap_or("unknown"),
                current_msg_id = current_msg_id.get(),
                prompt_anchor_message_id = anchor.map(|anchor| anchor.message_id),
                "TUI-direct bridge adapter completed streamed response relay"
            );
            Ok(())
        }
        Err(_) => Err(format!(
            "TUI-direct bridge adapter timed out waiting for completion for provider {}",
            provider.as_str()
        )),
    }
}

/// #3256: forward the buffered prefix and the live transcript-reader stream into
/// the bridge sender, preserving message ordering and guaranteeing a terminal
/// `Done` reaches the bridge exactly once.
///
/// - Leading TUI chrome (`No response requested.` / `Continue from where you
///   left off.`) is stripped from the FIRST non-empty `Text` frame, matching
///   the old `compose_tui_idle_response` behavior so the streamed card never
///   flashes that chrome.
/// - The transcript reader normally emits a terminal `Done` itself; if the
///   stream closes WITHOUT one (e.g. dead session mid-stream), a synthetic
///   `Done` is appended so the bridge still finalizes. A `Done` is forwarded at
///   most once — subsequent frames after a `Done` are dropped, since the bridge
///   has already claimed the turn outcome ("first wins").
///
/// Returns the number of `Text`-content frames forwarded (used by tests to
/// prove progressive relay: more than one before the terminal `Done`).
#[cfg(unix)]
#[allow(dead_code)]
pub(super) fn forward_idle_stream_into_bridge(
    prefix: Vec<StreamMessage>,
    reader_rx: mpsc::Receiver<StreamMessage>,
    tx: mpsc::Sender<StreamMessage>,
) -> usize {
    forward_idle_stream_into_bridge_with_logging(prefix, reader_rx, tx, None)
}

#[cfg(unix)]
fn forward_idle_stream_into_bridge_with_logging(
    prefix: Vec<StreamMessage>,
    reader_rx: mpsc::Receiver<StreamMessage>,
    tx: mpsc::Sender<StreamMessage>,
    log_context: Option<IdleStreamFrameLogContext>,
) -> usize {
    let mut first_text_seen = false;
    let mut done_forwarded = false;
    let mut text_frames_forwarded = 0usize;

    let forward = |message: StreamMessage,
                   first_text_seen: &mut bool,
                   done_forwarded: &mut bool,
                   text_frames_forwarded: &mut usize|
     -> bool {
        if *done_forwarded {
            if let StreamMessage::Text { content } = &message {
                log_idle_stream_text_decision(
                    log_context.as_ref(),
                    "drop_after_done",
                    content.len(),
                );
            }
            // Bridge already finalized on the terminal Done; drop trailing
            // frames (e.g. the reader's synthetic empty Done after the real
            // result Done) to avoid any double-finalize ambiguity.
            return true;
        }
        let message = match message {
            StreamMessage::Text { content } if !*first_text_seen && !content.trim().is_empty() => {
                *first_text_seen = true;
                let stripped =
                    super::super::response_sanitizer::strip_leading_tui_response_chrome(&content);
                StreamMessage::Text { content: stripped }
            }
            other => other,
        };
        let text_len = if let StreamMessage::Text { content } = &message {
            Some(content.len())
        } else {
            None
        };
        let non_empty_text =
            matches!(message, StreamMessage::Text { ref content } if !content.trim().is_empty());
        let is_done = matches!(message, StreamMessage::Done { .. });
        if tx.send(message).is_err() {
            if let Some(content_len) = text_len {
                log_idle_stream_text_decision(
                    log_context.as_ref(),
                    "drop_receiver_closed",
                    content_len,
                );
            }
            // Bridge receiver gone; stop forwarding.
            return false;
        }
        if let Some(content_len) = text_len {
            log_idle_stream_text_decision(log_context.as_ref(), "accept", content_len);
        }
        if non_empty_text {
            *text_frames_forwarded += 1;
        }
        if is_done {
            *done_forwarded = true;
        }
        true
    };

    for message in prefix {
        if !forward(
            message,
            &mut first_text_seen,
            &mut done_forwarded,
            &mut text_frames_forwarded,
        ) {
            return text_frames_forwarded;
        }
    }
    while let Ok(message) = reader_rx.recv() {
        if !forward(
            message,
            &mut first_text_seen,
            &mut done_forwarded,
            &mut text_frames_forwarded,
        ) {
            return text_frames_forwarded;
        }
    }

    if !done_forwarded {
        let _ = tx.send(StreamMessage::Done {
            result: String::new(),
            session_id: None,
        });
    }
    text_frames_forwarded
}

#[cfg(unix)]
#[allow(clippy::too_many_arguments)]
pub(super) fn build_tui_direct_bridge_inflight_state(
    provider: ProviderKind,
    channel_id: ChannelId,
    user_msg_id: MessageId,
    current_msg_id: MessageId,
    prompt_text: &str,
    tmux_session_name: &str,
    output_path: &Path,
    start_offset: u64,
    lease: &ExternalInputRelayLease,
) -> InflightTurnState {
    build_tui_direct_synthetic_inflight_state(
        provider,
        channel_id,
        user_msg_id,
        Some(current_msg_id),
        prompt_text,
        tmux_session_name,
        Some(output_path),
        start_offset,
        lease,
        RelayOwnerKind::None,
    )
}

#[cfg(unix)]
#[allow(dead_code)]
pub(super) fn tui_idle_tail_should_commit_runtime_binding_offset(
    response: &str,
    discord_delivery_succeeded: bool,
) -> bool {
    response.trim().is_empty() || discord_delivery_succeeded
}

#[cfg(unix)]
pub(super) async fn prompt_anchor_for_response_after_wait(
    provider: &str,
    tmux_session_name: &str,
    channel_id: u64,
) -> Option<crate::services::tui_prompt_dedupe::TuiPromptAnchor> {
    let deadline = tokio::time::Instant::now() + CODEX_IDLE_PROMPT_ANCHOR_WAIT;
    loop {
        if let Some(anchor) = crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
            provider,
            tmux_session_name,
            channel_id,
        ) {
            return Some(anchor);
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return None;
        }
        tokio::time::sleep(CODEX_IDLE_PROMPT_ANCHOR_POLL.min(deadline - now)).await;
    }
}
