use super::super::turn_bridge::BridgeCompletionSignal;
use super::*;

#[cfg(unix)]
#[derive(Clone, Copy)]
pub(super) struct IdleReaderCompletion {
    pub(super) offset: u64,
    pub(super) decoded_terminal: bool,
    pub(super) source_file: Option<crate::services::cluster::stream_relay::SourceFileIdentity>,
    pub(super) generation_mtime_ns: i64,
}

#[cfg(unix)]
impl IdleReaderCompletion {
    pub(super) fn from_harvest(
        result: ReadOutputResult,
        stats: crate::services::session_backend::ReadHarvestStats,
        generation_mtime_ns: i64,
    ) -> Self {
        let (offset, decoded_terminal) = match result {
            ReadOutputResult::Completed { offset } => (offset, stats.decoded_terminal),
            ReadOutputResult::Cancelled { offset } | ReadOutputResult::SessionDied { offset } => {
                (offset, false)
            }
        };
        Self {
            offset,
            decoded_terminal,
            source_file: stats.source_file,
            generation_mtime_ns,
        }
    }
}

#[cfg(unix)]
pub(super) type IdleReaderEnd =
    tokio::sync::oneshot::Receiver<Result<IdleReaderCompletion, String>>;

#[cfg(unix)]
struct IdleTerminalSource {
    provider: ProviderKind,
    transcript_path: String,
    tmux_session_name: String,
    turn_nonce: String,
    source_start: u64,
    actor: std::sync::Weak<CancelToken>,
}

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
/// idle tail still admits an empty terminal to deliver recovery guidance.
#[cfg(unix)]
pub(super) fn idle_stream_message_is_content(message: &StreamMessage) -> bool {
    match message {
        // #3256: a `Text`/`Done` body that is ONLY leading TUI chrome (e.g.
        // `No response requested.` / `Continue from where you left off.`) is NOT
        // real content. Strip before classifying prose; an empty Done remains
        // a separate terminal boundary requiring admitted recovery guidance.
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
/// the single bridge turn delivered successfully, including empty-response guidance.
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

/// #3256: STREAM-THROUGH idle bridge relay for the Claude external-input path.
///
/// EXACTLY ONE intake placeholder card and EXACTLY ONE `spawn_turn_bridge` per
/// external turn. Instead of pre-collecting the whole response and feeding the
/// bridge one synthetic `[Text{full}, Done]` (the collect-then-send path this
/// replaced, removed once it had no caller left),
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
    reader_end: Option<IdleReaderEnd>,
    lease: &ExternalInputRelayLease,
    lifetime_guard: Option<Arc<dyn std::any::Any + Send + Sync>>,
) -> Result<Option<u64>, String> {
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
    let gateway = Arc::new(TuiDirectBridgeGateway {
        http,
        shared: shared.clone(),
        provider: provider.clone(),
    });
    stream_tui_idle_response_with_gateway(
        shared,
        provider,
        channel_id,
        IdleBridgeSource {
            tmux_session_name,
            output_path,
            start_offset,
            prompt_text,
            lease,
            lifetime_guard,
        },
        (prefix, reader_rx, reader_end),
        gateway,
        context_compact_percent,
    )
    .await
}

#[cfg(unix)]
pub(super) struct IdleBridgeSource<'a> {
    pub(super) tmux_session_name: &'a str,
    pub(super) output_path: &'a Path,
    pub(super) start_offset: u64,
    pub(super) prompt_text: &'a str,
    pub(super) lease: &'a ExternalInputRelayLease,
    /// Moved into the bridge context so it is released only when the bridge task ends.
    pub(super) lifetime_guard: Option<Arc<dyn std::any::Any + Send + Sync>>,
}

#[cfg(unix)]
pub(super) async fn stream_tui_idle_response_with_gateway(
    shared: &Arc<SharedData>,
    provider: ProviderKind,
    channel_id: ChannelId,
    source: IdleBridgeSource<'_>,
    reader: (
        Vec<StreamMessage>,
        mpsc::Receiver<StreamMessage>,
        Option<IdleReaderEnd>,
    ),
    gateway: Arc<dyn super::super::gateway::TurnGateway>,
    context_compact_percent: u64,
) -> Result<Option<u64>, String> {
    let IdleBridgeSource {
        tmux_session_name,
        output_path,
        start_offset,
        prompt_text,
        lease,
        lifetime_guard,
    } = source;
    let (prefix, reader_rx, reader_end) = reader;
    let claim = super::synthetic_start::bridge_handoff::capture(
        shared,
        &provider,
        channel_id,
        tmux_session_name,
        output_path,
        lease,
    )
    .await?;
    if !claim.row.full_response.is_empty() && start_offset < claim.row.last_offset {
        return Err("idle continuation must resume from its saved source cursor".into());
    }
    let user_msg_id = MessageId::new(claim.row.user_msg_id);
    let current_msg_id = MessageId::new(claim.row.current_msg_id);
    let anchor = crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
        provider.as_str(),
        tmux_session_name,
        channel_id.get(),
    )
    .filter(|anchor| anchor.message_id == user_msg_id.get());
    let (tx, rx) = mpsc::channel();
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    let source = if reader_end.is_some() {
        Some(IdleTerminalSource {
            provider: provider.clone(),
            transcript_path: std::fs::canonicalize(output_path)
                .map_err(|error| error.to_string())?
                .to_string_lossy()
                .into_owned(),
            tmux_session_name: tmux_session_name.to_owned(),
            turn_nonce: claim
                .actor
                .turn_nonce()
                .filter(|nonce| !nonce.is_empty())
                .ok_or("missing captured TUI actor nonce")?
                .to_owned(),
            source_start: claim
                .row
                .turn_start_offset
                .ok_or("missing captured TUI source boundary")?,
            actor: Arc::downgrade(&claim.actor),
        })
    } else {
        None
    };
    let inflight_state = claim.row.clone();
    let bridge = TurnBridgeContext {
        provider: provider.clone(),
        gateway: gateway.clone(),
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
        response_sent_offset: claim.row.response_sent_offset,
        full_response: claim.row.full_response.clone(),
        tmux_last_offset: Some(start_offset),
        new_session_id: None,
        defer_watcher_resume: false,
        reuse_status_panel_message: false,
        completion_tx: Some(completion_tx),
        is_external_input_tui_direct: true, // #3959: suppress mirror chrome footer
        inflight_state,
        _lifetime_guard: lifetime_guard,
    };

    // EXACTLY ONE spawn_turn_bridge_with_pin per external turn.
    let pin = crate::services::discord::tmux::WatcherClaimIncarnation::capture_for_source(
        &shared.tmux_watchers,
        tmux_session_name,
        output_path,
    );
    crate::services::discord::turn_bridge::spawn_turn_bridge_with_pin(
        shared.clone(),
        claim.actor.clone(),
        rx,
        bridge,
        pin,
    );
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
    // `Done`; Claude terminal frames wait for the reader completion proof.
    // A failed reader leaves the captured episode available for recovery.
    let forward_handle = tokio::task::spawn_blocking(move || {
        forward_idle_stream_into_bridge_with_logging(
            prefix,
            reader_rx,
            tx,
            reader_end,
            source,
            Some(frame_log_context),
        )
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
    let reader_outcome = forward_handle.await.map_err(|error| error.to_string());

    // Only NOW bound the post-`Done` bridge finalization (Discord edit/flush),
    // which should land within seconds of the terminal frame being forwarded.
    let completion = tokio::time::timeout(Duration::from_secs(180), completion_rx).await;

    let result = finish_idle_bridge_completion(
        completion,
        gateway.as_ref(),
        &provider,
        (channel_id, user_msg_id, current_msg_id),
        None,
        (tmux_session_name, lease, anchor),
        true,
    )
    .await;
    claim.preserve_continuation(shared).await;
    let (_, source_offset) = reader_outcome?;
    let source_offset = source_offset?;
    result.map(|()| source_offset)
}

// Shared by both adapters; only Finalized may acknowledge delivery or clear the anchor.
#[cfg(unix)]
pub(super) async fn finish_idle_bridge_completion(
    completion: Result<
        Result<BridgeCompletionSignal, tokio::sync::oneshot::error::RecvError>,
        tokio::time::error::Elapsed,
    >,
    gateway: &dyn super::super::gateway::TurnGateway,
    provider: &ProviderKind,
    message_ids: (ChannelId, MessageId, MessageId),
    bridge_created_placeholder: Option<MessageId>,
    turn_context: (
        &str,
        &ExternalInputRelayLease,
        Option<crate::services::tui_prompt_dedupe::TuiPromptAnchor>,
    ),
    streamed: bool,
) -> Result<(), String> {
    let (channel_id, user_msg_id, current_msg_id) = message_ids;
    let (tmux_session_name, lease, anchor) = turn_context;
    match completion {
        Ok(Ok(BridgeCompletionSignal::Finalized)) => {
            ensure_tui_direct_bridge_delivery_committed(
                provider,
                channel_id,
                user_msg_id,
                current_msg_id,
                tmux_session_name,
                lease,
                anchor.map(|anchor| anchor.message_id),
                streamed,
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
                "{}", if streamed { "TUI-direct bridge adapter completed streamed response relay" }
                else { "TUI-direct bridge adapter completed response relay" }
            );
            Ok(())
        }
        aborted @ (Ok(Ok(BridgeCompletionSignal::EntryAborted)) | Ok(Err(_))) => {
            let durable =
                super::super::inflight::load_inflight_state_read_only(provider, channel_id.get());
            if let Some(placeholder) = bridge_created_placeholder.filter(|id| {
                !durable.as_ref().is_some_and(|row| {
                    row.current_msg_id == id.get()
                        || row.user_msg_id == id.get()
                        || row.status_message_id == Some(id.get())
                })
            }) {
                if let Err(error) = gateway.delete_message(channel_id, placeholder).await {
                    tracing::warn!(%error, "failed to delete aborted bridge placeholder");
                }
            }
            tracing::warn!(turn_id = lease.turn_id.as_deref().unwrap_or(""),
                durable_user_msg_id = durable.as_ref().map(|row| row.user_msg_id),
                reason = ?aborted, "TUI-direct bridge entry aborted before authority");
            Err("TUI-direct bridge entry aborted before authority".to_string())
        }
        Ok(Ok(
            signal @ (BridgeCompletionSignal::DeferredToCustody
            | BridgeCompletionSignal::DeferredToOwner
            | BridgeCompletionSignal::Unresolved),
        )) => Err(format!(
            "TUI-direct bridge delivery remains pending: {signal:?}"
        )),
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
/// - The legacy forwarding helper retains its existing fallback Done. The
///   production Claude adapter supplies reader completion evidence and keeps
///   one Done pending until a real decoded terminal is confirmed. Failed reads
///   close the stream while preserving the durable episode for recovery.
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
    forward_idle_stream_into_bridge_with_logging(prefix, reader_rx, tx, None, None, None).0
}

#[cfg(unix)]
fn forward_idle_stream_into_bridge_with_logging(
    prefix: Vec<StreamMessage>,
    reader_rx: mpsc::Receiver<StreamMessage>,
    tx: mpsc::Sender<StreamMessage>,
    reader_end: Option<IdleReaderEnd>,
    source: Option<IdleTerminalSource>,
    log_context: Option<IdleStreamFrameLogContext>,
) -> (usize, Result<Option<u64>, String>) {
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

    // Source readers can synthesize Done or defer Error until shutdown. Keep one
    // terminal frame until their positive decoder evidence arrives; prose still
    // flows immediately. A failed reader closes the bridge without claiming Done.
    let strict_terminal = reader_end.is_some();
    let mut terminal = None;
    for message in prefix.into_iter().chain(reader_rx) {
        if strict_terminal
            && (terminal.is_some()
                || matches!(
                    message,
                    StreamMessage::Done { .. } | StreamMessage::Error { .. }
                ))
        {
            if terminal.is_none() {
                terminal = Some(message);
            }
            continue;
        }
        if !forward(
            message,
            &mut first_text_seen,
            &mut done_forwarded,
            &mut text_frames_forwarded,
        ) {
            return (
                text_frames_forwarded,
                Err("idle bridge receiver closed".into()),
            );
        }
    }
    let completed = match reader_end {
        Some(end) => match end.blocking_recv() {
            Ok(Ok(completed)) if completed.decoded_terminal => Ok(Some(completed)),
            Ok(Ok(_)) => Err("idle source reader ended without a decoded terminal".into()),
            Ok(Err(error)) => Err(error),
            Err(error) => Err(format!("idle source reader lost its completion: {error}")),
        },
        None => Ok(None),
    };
    let source_offset = completed
        .as_ref()
        .map(|done| done.map(|done| done.offset))
        .map_err(Clone::clone);
    if let Ok(completed) = completed
        && !done_forwarded
    {
        let done = terminal.unwrap_or(StreamMessage::Done {
            result: String::new(),
            session_id: None,
        });
        let message = match (completed, source, done) {
            (Some(completed), Some(source), StreamMessage::Done { result, session_id }) => {
                let Some(crate::services::cluster::stream_relay::SourceFileIdentity::Unix {
                    dev,
                    ino,
                }) = completed.source_file
                else {
                    return (
                        text_frames_forwarded,
                        Err("TUI terminal reader has no opened-file identity".into()),
                    );
                };
                if source.provider == ProviderKind::Codex {
                    StreamMessage::CodexTuiTerminalDone {
                        result,
                        session_id,
                        rollout_path: source.transcript_path,
                        tmux_session_name: source.tmux_session_name,
                        turn_nonce: source.turn_nonce,
                        source_start: source.source_start,
                        complete_record_end: completed.offset,
                        captured_source: Some(
                            crate::services::agent_protocol::CapturedTuiTerminalSource {
                                generation_mtime_ns: completed.generation_mtime_ns,
                                source_file_dev: dev,
                                source_file_ino: ino,
                                actor: source.actor,
                            },
                        ),
                    }
                } else {
                    StreamMessage::ClaudeTuiTerminalDone {
                        result,
                        session_id,
                        transcript_path: source.transcript_path,
                        tmux_session_name: source.tmux_session_name,
                        turn_nonce: source.turn_nonce,
                        source_start: source.source_start,
                        complete_record_end: completed.offset,
                        generation_mtime_ns: completed.generation_mtime_ns,
                        source_file_dev: dev,
                        source_file_ino: ino,
                        actor: source.actor,
                    }
                }
            }
            (Some(_), _, _) => {
                return (
                    text_frames_forwarded,
                    Err("TUI terminal source witness missing".into()),
                );
            }
            (None, _, done) => done,
        };
        if tx.send(message).is_err() {
            return (
                text_frames_forwarded,
                Err("idle bridge receiver closed before terminal".into()),
            );
        }
    }
    (text_frames_forwarded, source_offset)
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
