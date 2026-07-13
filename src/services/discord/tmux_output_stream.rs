use super::*;

/// Tracks tool/thinking status during watcher output processing.
pub(in crate::services::discord) struct WatcherToolState {
    /// Current tool status line (e.g. "⚙ Bash: `ls`")
    pub current_tool_line: Option<String>,
    /// Previous distinct tool/thinking status for 2-line trail rendering.
    pub prev_tool_status: Option<String>,
    /// Whether we are currently inside a thinking block
    pub in_thinking: bool,
    /// Whether any tool_use block has been seen in this turn
    pub any_tool_used: bool,
    /// Whether a text block was streamed after the last tool_use
    pub has_post_tool_text: bool,
    /// Structured transcript events collected during watcher replay
    pub transcript_events: Vec<SessionTranscriptEvent>,
    /// Recent user-visible tool/system events for Active placeholder cards.
    placeholder_events: Vec<RecentPlaceholderEvent>,
    /// Provider-normalized status events for the status-panel-v2 message.
    status_events: Vec<StatusEvent>,
}

impl WatcherToolState {
    pub fn new() -> Self {
        Self {
            current_tool_line: None,
            prev_tool_status: None,
            in_thinking: false,
            any_tool_used: false,
            has_post_tool_text: false,
            transcript_events: Vec::new(),
            placeholder_events: Vec::new(),
            status_events: Vec::new(),
        }
    }

    fn record_placeholder_events_from_json(&mut self, value: &serde_json::Value) {
        self.placeholder_events.extend(events_from_json(value));
        self.status_events.extend(status_events_from_json(value));
    }

    fn take_placeholder_events(&mut self) -> Vec<RecentPlaceholderEvent> {
        std::mem::take(&mut self.placeholder_events)
    }

    fn take_status_events(&mut self) -> Vec<StatusEvent> {
        std::mem::take(&mut self.status_events)
    }

    fn set_current_tool_line(&mut self, next_tool_line: Option<String>) {
        let current_tool_line = self.current_tool_line.clone();
        crate::services::discord::formatting::preserve_previous_tool_status(
            &mut self.prev_tool_status,
            current_tool_line.as_deref(),
            next_tool_line.as_deref(),
        );
        self.current_tool_line = next_tool_line;
    }

    fn clear_current_tool_line(&mut self) {
        let current_tool_line = self.current_tool_line.clone();
        crate::services::discord::formatting::preserve_previous_tool_status(
            &mut self.prev_tool_status,
            current_tool_line.as_deref(),
            None,
        );
        self.current_tool_line = None;
    }

    fn mark_thinking(&mut self) {
        if self.current_tool_line.as_deref() != Some(REDACTED_THINKING_STATUS_LINE) {
            self.set_current_tool_line(Some(REDACTED_THINKING_STATUS_LINE.to_string()));
        }
    }
}

pub(in crate::services::discord) fn flush_placeholder_live_events(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    tool_state: &mut WatcherToolState,
) -> bool {
    let events = tool_state.take_placeholder_events();
    let status_events = tool_state.take_status_events();
    let mut dirty = false;
    if (shared.ui.placeholder_live_events_enabled || shared.ui.status_panel_v2_enabled)
        && !events.is_empty()
    {
        shared
            .ui
            .placeholder_live_events
            .push_many(channel_id, events);
        dirty = true;
    }
    if shared.ui.status_panel_v2_enabled && !status_events.is_empty() {
        shared
            .ui
            .placeholder_live_events
            .push_status_events(channel_id, status_events);
        dirty = true;
    }
    dirty
}

pub(in crate::services::discord) fn force_next_watcher_status_update(
    last_status_update: &mut tokio::time::Instant,
) {
    *last_status_update =
        tokio::time::Instant::now() - crate::services::discord::status_update_interval();
}

pub(in crate::services::discord) fn build_watcher_placeholder_status_block(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    indicator: &str,
    prev_tool_status: Option<&str>,
    current_tool_line: Option<&str>,
    full_response: &str,
    status_panel_msg_id: Option<serenity::MessageId>,
) -> String {
    if watcher_placeholder_uses_status_panel_only(
        shared.ui.status_panel_v2_enabled,
        status_panel_msg_id,
    ) {
        return crate::services::discord::formatting::build_processing_status_block(indicator);
    }
    let status_block = crate::services::discord::formatting::build_placeholder_status_block(
        indicator,
        prev_tool_status,
        current_tool_line,
        full_response,
    );
    if watcher_placeholder_inlines_live_events(
        shared.ui.placeholder_live_events_enabled,
        shared.ui.status_panel_v2_enabled,
        status_panel_msg_id,
    ) && let Some(block) = shared.ui.placeholder_live_events.render_block(channel_id)
    {
        return format!("{status_block}\n{block}");
    }
    status_block
}

fn watcher_placeholder_uses_status_panel_only(
    status_panel_v2_enabled: bool,
    status_panel_msg_id: Option<serenity::MessageId>,
) -> bool {
    status_panel_v2_enabled && status_panel_msg_id.is_some()
}

fn watcher_placeholder_inlines_live_events(
    placeholder_live_events_enabled: bool,
    status_panel_v2_enabled: bool,
    status_panel_msg_id: Option<serenity::MessageId>,
) -> bool {
    placeholder_live_events_enabled || (status_panel_v2_enabled && status_panel_msg_id.is_none())
}

/// Process buffered lines for the tmux watcher.
/// Extracts text content, tracks tool status, and detects result events.
/// Returns true if a "result" event was found.
pub(in crate::services::discord) fn process_watcher_lines(
    buffer: &mut String,
    state: &mut StreamLineState,
    full_response: &mut String,
    tool_state: &mut WatcherToolState,
) -> WatcherLineOutcome {
    process_watcher_lines_for_turn(buffer, state, full_response, tool_state, None, None)
}

pub(in crate::services::discord) fn process_watcher_lines_for_turn(
    buffer: &mut String,
    state: &mut StreamLineState,
    full_response: &mut String,
    tool_state: &mut WatcherToolState,
    buffer_start_offset: Option<u64>,
    turn_start_offset: Option<u64>,
) -> WatcherLineOutcome {
    let mut outcome = WatcherLineOutcome::default();
    let mut next_line_offset = buffer_start_offset;

    while let Some(pos) = buffer.find('\n') {
        let line_start_offset = next_line_offset;
        let line_len = pos + 1;
        if let Some(offset) = next_line_offset.as_mut() {
            *offset = offset.saturating_add(line_len as u64);
        }
        let line: String = buffer.drain(..=pos).collect();
        let trimmed = line.trim();
        let pre_turn_line = should_skip_pre_turn_line(turn_start_offset, line_start_offset);
        if trimmed.is_empty() {
            if pre_turn_line {
                outcome.pre_turn_bytes_skipped =
                    outcome.pre_turn_bytes_skipped.saturating_add(line_len);
            }
            continue;
        }

        // Parse the JSON line
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            let event_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if let Some(skip) = pre_turn_line_skip(
                turn_start_offset,
                line_start_offset,
                terminal_kind_for_json_evidence(&val),
            ) {
                tracing::warn!(
                    terminal_kind = skip.terminal_kind.as_str(),
                    evidence_offset = skip.evidence_offset,
                    turn_start_offset = skip.turn_start_offset,
                    "tmux watcher skipped terminal evidence before this turn's start offset"
                );
                outcome.pre_turn_bytes_skipped =
                    outcome.pre_turn_bytes_skipped.saturating_add(line_len);
                continue;
            }
            if pre_turn_line {
                outcome.pre_turn_bytes_skipped =
                    outcome.pre_turn_bytes_skipped.saturating_add(line_len);
                continue;
            }
            if event_type == "user" && watcher_user_event_is_prompt_boundary(&val) {
                if outcome.soft_terminal_candidate || !full_response.trim().is_empty() {
                    if !outcome.soft_terminal_candidate {
                        outcome.soft_terminal_candidate = true;
                        outcome.terminal_kind = Some(WatcherTerminalKind::SoftUserBoundary);
                        outcome.terminal_evidence_offset = line_start_offset;
                    }
                    buffer.insert_str(0, &line);
                    break;
                }
            }
            observe_stream_context(&val, state);
            tool_state.record_placeholder_events_from_json(&val);
            match event_type {
                "assistant" => {
                    if let Some(message) = val.get("message") {
                        if let Some(model) = message.get("model").and_then(|value| value.as_str()) {
                            state.last_model = Some(model.to_string());
                        }
                        if let Some(usage) = message.get("usage") {
                            // #1918: input/cache_read/cache_create replace so the
                            // status panel Context line reflects the LAST API call
                            // (context occupancy of the most recent call). The
                            // previous code summed per-call usage and inflated
                            // multi-call (tool-use loop) turns past the window
                            // size. output_tokens stays accumulated because turn
                            // analytics expect the cumulative output total.
                            // Missing fields use 0, not carry-over of the prior
                            // call's value; otherwise tool-loop calls that omit
                            // cache_read/cache_create would re-inflate.
                            state.saw_per_message_usage = true;
                            state.accum_input_tokens = usage
                                .get("input_tokens")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0);
                            state.accum_cache_read_tokens = usage
                                .get("cache_read_input_tokens")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0);
                            state.accum_cache_create_tokens = usage
                                .get("cache_creation_input_tokens")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(0);
                            state.accum_output_tokens = state.accum_output_tokens.saturating_add(
                                usage
                                    .get("output_tokens")
                                    .and_then(|value| value.as_u64())
                                    .unwrap_or(0),
                            );
                        }
                        // Text content from assistant message
                        if let Some(content) = message.get("content") {
                            if let Some(arr) = content.as_array() {
                                for block in arr {
                                    let block_type = block.get("type").and_then(|t| t.as_str());
                                    if block_type == Some("text") {
                                        if let Some(text) =
                                            block.get("text").and_then(|t| t.as_str())
                                        {
                                            // #4275: the watcher/jsonl path emits each
                                            // assistant text block as a discrete event
                                            // (split by interleaved tool_use blocks), so a
                                            // bare push_str glued the tail of one segment
                                            // onto the head of the next with no boundary —
                                            // "…정리합니다." + "macOS엔…" became one run-on
                                            // line. Mirror the #3608 streaming-path
                                            // separator: insert "\n\n" only when the two
                                            // segments form a genuine sentence boundary
                                            // (not a decimal, file extension, markdown
                                            // continuation, mid-inline-code span, or — r2,
                                            // folded into the predicate itself — inside an
                                            // open ``` code fence). content_block_delta
                                            // (line ~360, qwen intra-block streaming) must
                                            // stay a bare push_str — a separator there would
                                            // fracture a single sentence.
                                            if !full_response.is_empty()
                                                && crate::services::discord::semantic_boundaries::semantic_chunk_separator_needed(
                                                    &full_response,
                                                    text,
                                                )
                                            {
                                                full_response.push_str("\n\n");
                                            }
                                            full_response.push_str(text);
                                            outcome.assistant_text_seen = true;
                                            push_transcript_event(
                                                &mut tool_state.transcript_events,
                                                SessionTranscriptEvent {
                                                    kind: SessionTranscriptEventKind::Assistant,
                                                    tool_name: None,
                                                    summary: None,
                                                    content: text.to_string(),
                                                    status: Some("success".to_string()),
                                                    is_error: false,
                                                },
                                            );
                                            if tool_state.any_tool_used {
                                                tool_state.has_post_tool_text = true;
                                            }
                                            tool_state.clear_current_tool_line();
                                        }
                                    } else if block_type == Some("tool_use") {
                                        tool_state.any_tool_used = true;
                                        tool_state.has_post_tool_text = false;
                                        let name = block
                                            .get("name")
                                            .and_then(|n| n.as_str())
                                            .unwrap_or("Tool");
                                        let input_str = block
                                            .get("input")
                                            .map(|i| i.to_string())
                                            .unwrap_or_default();
                                        let summary = format_tool_input(name, &input_str);
                                        let display = if summary.is_empty() {
                                            format!("⚙ {}", name)
                                        } else {
                                            let truncated: String =
                                                summary.chars().take(500).collect();
                                            format!("⚙ {}: {}", name, truncated)
                                        };
                                        tool_state.set_current_tool_line(Some(display));
                                        push_transcript_event(
                                            &mut tool_state.transcript_events,
                                            SessionTranscriptEvent {
                                                kind: SessionTranscriptEventKind::ToolUse,
                                                tool_name: Some(name.to_string()),
                                                summary: (!summary.is_empty()).then_some(summary),
                                                content: input_str,
                                                status: Some("running".to_string()),
                                                is_error: false,
                                            },
                                        );
                                    } else if block_type == Some("thinking") {
                                        tool_state.mark_thinking();
                                        push_transcript_event(
                                            &mut tool_state.transcript_events,
                                            redacted_thinking_transcript_event(),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                "content_block_start" => {
                    if let Some(cb) = val.get("content_block") {
                        let cb_type = cb.get("type").and_then(|t| t.as_str());
                        if cb_type == Some("thinking") {
                            tool_state.in_thinking = true;
                            tool_state.mark_thinking();
                        } else if cb_type == Some("tool_use") {
                            tool_state.any_tool_used = true;
                            tool_state.has_post_tool_text = false;
                            let name = cb.get("name").and_then(|n| n.as_str()).unwrap_or("Tool");
                            tool_state.set_current_tool_line(Some(format!("⚙ {}", name)));
                        }
                    }
                }
                "content_block_delta" => {
                    if let Some(delta) = val.get("delta") {
                        if delta.get("thinking").and_then(|t| t.as_str()).is_some() {
                            tool_state.mark_thinking();
                        } else if let Some(text) = delta.get("text").and_then(|t| t.as_str()) {
                            full_response.push_str(text);
                            outcome.assistant_text_seen = true;
                            if tool_state.any_tool_used {
                                tool_state.has_post_tool_text = true;
                            }
                            tool_state.clear_current_tool_line();
                        }
                    }
                }
                "content_block_stop" => {
                    if tool_state.in_thinking {
                        tool_state.in_thinking = false;
                        tool_state.mark_thinking();
                        push_transcript_event(
                            &mut tool_state.transcript_events,
                            redacted_thinking_transcript_event(),
                        );
                    } else if let Some(line) = tool_state.current_tool_line.clone() {
                        // Tool completed — mark with checkmark
                        if line.starts_with("⚙") {
                            tool_state.set_current_tool_line(Some(line.replacen("⚙", "✓", 1)));
                        }
                    }
                }
                "result" => {
                    outcome.stale_resume_detected = outcome.stale_resume_detected
                        || crate::services::discord::turn_bridge::result_event_has_stale_resume_error(&val);
                    if let Some(session_id) = val.get("session_id").and_then(|value| value.as_str())
                    {
                        state.last_session_id = Some(session_id.to_string());
                    }
                    let is_error = val
                        .get("is_error")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    let result_str = extract_result_error_text(&val);
                    push_transcript_event(
                        &mut tool_state.transcript_events,
                        SessionTranscriptEvent {
                            kind: if is_error {
                                SessionTranscriptEventKind::Error
                            } else {
                                SessionTranscriptEventKind::Result
                            },
                            tool_name: None,
                            summary: Some(if result_str.trim().is_empty() {
                                if is_error {
                                    "error".to_string()
                                } else {
                                    "completed".to_string()
                                }
                            } else {
                                truncate_str(&result_str, 120).to_string()
                            }),
                            content: result_str.clone(),
                            status: Some(if is_error { "error" } else { "success" }.to_string()),
                            is_error,
                        },
                    );

                    if is_error {
                        if is_prompt_too_long_message(&result_str) {
                            outcome.is_prompt_too_long = true;
                        }
                        if is_auth_error_message(&result_str) {
                            outcome.is_auth_error = true;
                            outcome.auth_error_message.get_or_insert(result_str.clone());
                        }
                        if let Some(message) = detect_provider_overload_message(&result_str) {
                            outcome.is_provider_overloaded = true;
                            outcome.provider_overload_message.get_or_insert(message);
                        }
                    }

                    // Use result text when streaming didn't capture the final response:
                    // 1. full_response is empty — no text was streamed at all
                    // 2. tools were used but no text was streamed after the last tool
                    //    — append result_str so earlier narration is preserved (#2749)
                    if !outcome.is_prompt_too_long
                        && !outcome.is_auth_error
                        && !outcome.is_provider_overloaded
                        && !result_str.is_empty()
                    {
                        if full_response.trim().is_empty() {
                            full_response.clear();
                            full_response.push_str(&result_str);
                        } else if tool_state.any_tool_used && !tool_state.has_post_tool_text {
                            let trimmed_result = result_str.trim();
                            // Skip only when the final newline-delimited chunk
                            // of full_response is an exact duplicate of
                            // result_str. Tail-substring matches against
                            // narration are a false-positive risk.
                            let already_present = !trimmed_result.is_empty()
                                && full_response
                                    .trim_end_matches('\n')
                                    .rsplit('\n')
                                    .next()
                                    .map(|last| last.trim() == trimmed_result)
                                    .unwrap_or(false);
                            if !trimmed_result.is_empty() && !already_present {
                                if !full_response.ends_with('\n') {
                                    full_response.push('\n');
                                }
                                full_response.push_str(&result_str);
                            }
                        }
                    }
                    // #1918/#3344: Claude emits per-message `usage`, so the
                    // assistant-message branch already captured the LAST call's
                    // occupancy. For terminal-usage providers, adopt the result
                    // frame's nested usage with shared provenance + magnitude
                    // gating: the Codex legacy wrapper's terminal accounting is
                    // known-cumulative (top-level `input_tokens`/`output_tokens`
                    // marker) and is suppressed so CTW renders honest "unknown"
                    // instead of a fabricated full window; Qwen-shaped per-call
                    // usage is adopted unchanged. See
                    // `session_backend::adopt_terminal_result_usage`.
                    crate::services::session_backend::adopt_terminal_result_usage(&val, state);

                    state.final_result = Some(String::new());
                    strip_leading_tui_response_chrome_in_place(full_response, &mut outcome);
                    outcome.found_result = true;
                    outcome.terminal_kind = Some(WatcherTerminalKind::HardResult);
                    outcome.terminal_evidence_offset = line_start_offset;
                    // #1216: stop after the first turn-terminating event so a
                    // buffer containing multiple completed turns (post-deploy
                    // backlog, paused watcher resume) does not merge their
                    // `assistant` text into a single `full_response`. The
                    // unprocessed tail stays in `buffer` for the next call.
                    break;
                }
                "system" => {
                    if val.get("subtype").and_then(|s| s.as_str()) == Some("init")
                        && let Some(session_id) = val
                            .get("session_id")
                            .or_else(|| val.get("sessionId"))
                            .and_then(|value| value.as_str())
                    {
                        state.last_session_id = Some(session_id.to_string());
                    }
                    // Detect auto-compaction events from Claude Code
                    if let Some(msg) = val.get("message").and_then(|m| m.as_str()) {
                        let lower = msg.to_ascii_lowercase();
                        if lower.contains("compacted")
                            || lower.contains("auto-compact")
                            || lower.contains("conversation has been compressed")
                        {
                            outcome.auto_compacted = true;
                        }
                    }
                    if let Some(subtype) = val.get("subtype").and_then(|s| s.as_str()) {
                        if subtype == "compact" || subtype == "auto_compact" {
                            outcome.auto_compacted = true;
                        }
                        // `task_notification` is the authoritative
                        // provider-normalized marker for a background-trigger
                        // turn (Claude emits it directly; Codex normalizes
                        // `background_event` into the same JSONL shape). It
                        // lets us distinguish a background-trigger turn from
                        // a normal foreground turn whose inflight file was
                        // merely cleared early by turn_bridge.
                        if subtype == "task_notification" {
                            outcome.task_notification_kind = merge_task_notification_kind(
                                outcome.task_notification_kind,
                                classify_task_notification_kind(&val, state),
                            );
                            if let Some(context) = super::super::task_notification_delivery::TaskNotificationContext::from_stream_json(&val, state) {
                                outcome.task_notification_context =
                                    super::super::task_notification_delivery::merge_context(
                                        outcome.task_notification_context.take(),
                                        context,
                                    );
                            }
                        }
                        // #2110/#relay-commit: Claude TUI transcript can
                        // emit `stop_hook_summary` before late assistant text.
                        // Treat it as a soft terminal candidate only; the
                        // watcher commits after readiness/quiescence or a
                        // debounce, while hard `result` remains immediate.
                        if subtype == "stop_hook_summary" {
                            if let Some(session_id) = val
                                .get("session_id")
                                .or_else(|| val.get("sessionId"))
                                .and_then(|value| value.as_str())
                            {
                                state.last_session_id = Some(session_id.to_string());
                            }
                            strip_leading_tui_response_chrome_in_place(full_response, &mut outcome);
                            outcome.soft_terminal_candidate = true;
                            outcome.terminal_kind = Some(WatcherTerminalKind::SoftStopHookSummary);
                            outcome.terminal_evidence_offset = line_start_offset;
                        }
                    }
                }
                _ => {}
            }
        } else if is_auth_error_message(trimmed) {
            if let Some(skip) = pre_turn_line_skip(
                turn_start_offset,
                line_start_offset,
                Some(WatcherTerminalKind::AuthError),
            ) {
                tracing::warn!(
                    terminal_kind = skip.terminal_kind.as_str(),
                    evidence_offset = skip.evidence_offset,
                    turn_start_offset = skip.turn_start_offset,
                    "tmux watcher skipped terminal evidence before this turn's start offset"
                );
                outcome.pre_turn_bytes_skipped =
                    outcome.pre_turn_bytes_skipped.saturating_add(line_len);
                continue;
            }
            if pre_turn_line {
                outcome.pre_turn_bytes_skipped =
                    outcome.pre_turn_bytes_skipped.saturating_add(line_len);
                continue;
            }
            outcome.found_result = true;
            outcome.terminal_kind = Some(WatcherTerminalKind::AuthError);
            outcome.terminal_evidence_offset = line_start_offset;
            outcome.is_auth_error = true;
            outcome
                .auth_error_message
                .get_or_insert(trimmed.to_string());
            push_transcript_event(
                &mut tool_state.transcript_events,
                SessionTranscriptEvent {
                    kind: SessionTranscriptEventKind::Error,
                    tool_name: None,
                    summary: Some("authentication error".to_string()),
                    content: trimmed.to_string(),
                    status: Some("error".to_string()),
                    is_error: true,
                },
            );
            state.final_result = Some(String::new());
            // #1216: see `result` arm — stop after a turn-terminating event.
            break;
        } else if let Some(message) = detect_provider_overload_message(trimmed) {
            if let Some(skip) = pre_turn_line_skip(
                turn_start_offset,
                line_start_offset,
                Some(WatcherTerminalKind::ProviderOverload),
            ) {
                tracing::warn!(
                    terminal_kind = skip.terminal_kind.as_str(),
                    evidence_offset = skip.evidence_offset,
                    turn_start_offset = skip.turn_start_offset,
                    "tmux watcher skipped terminal evidence before this turn's start offset"
                );
                outcome.pre_turn_bytes_skipped =
                    outcome.pre_turn_bytes_skipped.saturating_add(line_len);
                continue;
            }
            if pre_turn_line {
                outcome.pre_turn_bytes_skipped =
                    outcome.pre_turn_bytes_skipped.saturating_add(line_len);
                continue;
            }
            outcome.found_result = true;
            outcome.terminal_kind = Some(WatcherTerminalKind::ProviderOverload);
            outcome.terminal_evidence_offset = line_start_offset;
            outcome.is_provider_overloaded = true;
            outcome.provider_overload_message.get_or_insert(message);
            push_transcript_event(
                &mut tool_state.transcript_events,
                SessionTranscriptEvent {
                    kind: SessionTranscriptEventKind::Error,
                    tool_name: None,
                    summary: Some("provider overload".to_string()),
                    content: trimmed.to_string(),
                    status: Some("error".to_string()),
                    is_error: true,
                },
            );
            state.final_result = Some(String::new());
            // #1216: see `result` arm — stop after a turn-terminating event.
            break;
        } else if pre_turn_line {
            outcome.pre_turn_bytes_skipped =
                outcome.pre_turn_bytes_skipped.saturating_add(line_len);
        }
    }

    outcome
}

#[derive(Debug, Clone, Copy)]
struct PreTurnTerminalSkip {
    evidence_offset: u64,
    turn_start_offset: u64,
    terminal_kind: WatcherTerminalKind,
}

fn pre_turn_line_skip(
    turn_start_offset: Option<u64>,
    line_start_offset: Option<u64>,
    terminal_kind: Option<WatcherTerminalKind>,
) -> Option<PreTurnTerminalSkip> {
    let terminal_kind = terminal_kind?;
    let evidence_offset = line_start_offset?;
    let turn_start_offset = turn_start_offset?;
    (evidence_offset < turn_start_offset).then_some(PreTurnTerminalSkip {
        evidence_offset,
        turn_start_offset,
        terminal_kind,
    })
}

fn should_skip_pre_turn_line(
    turn_start_offset: Option<u64>,
    line_start_offset: Option<u64>,
) -> bool {
    matches!(
        (line_start_offset, turn_start_offset),
        (Some(line), Some(turn_start)) if line < turn_start
    )
}

fn terminal_kind_for_json_evidence(value: &serde_json::Value) -> Option<WatcherTerminalKind> {
    match value.get("type").and_then(|t| t.as_str()) {
        Some("result") => Some(WatcherTerminalKind::HardResult),
        Some("system")
            if value.get("subtype").and_then(|s| s.as_str()) == Some("stop_hook_summary") =>
        {
            Some(WatcherTerminalKind::SoftStopHookSummary)
        }
        _ => None,
    }
}

fn watcher_user_event_is_prompt_boundary(value: &serde_json::Value) -> bool {
    if value
        .get("isMeta")
        .and_then(serde_json::Value::as_bool)
        .is_some_and(|is_meta| is_meta)
    {
        return false;
    }
    let Some(message) = value.get("message") else {
        return false;
    };
    if message
        .get("role")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|role| role != "user")
    {
        return false;
    }
    message_content_has_user_prompt_text(message)
}

fn message_content_has_user_prompt_text(message: &serde_json::Value) -> bool {
    match message.get("content") {
        Some(serde_json::Value::String(text)) => !text.trim().is_empty(),
        Some(serde_json::Value::Array(items)) => items.iter().any(|item| {
            item.get("text")
                .or_else(|| item.get("input_text"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|text| !text.trim().is_empty())
        }),
        _ => false,
    }
}

fn strip_leading_tui_response_chrome_in_place(
    full_response: &mut String,
    outcome: &mut WatcherLineOutcome,
) {
    let cleaned = crate::services::discord::response_sanitizer::strip_leading_tui_response_chrome(
        full_response,
    );
    if cleaned != *full_response {
        full_response.clear();
        full_response.push_str(&cleaned);
        if full_response.trim().is_empty() {
            outcome.assistant_text_seen = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::session_backend::StreamLineState;

    mod provider_output_guard_tests {
        include!("tmux_output_stream/provider_output_guard_tests.rs");
    }

    #[test]
    fn watcher_status_panel_fallback_inlines_events_without_status_message() {
        assert!(
            watcher_placeholder_inlines_live_events(false, true, /*status_panel_msg_id*/ None),
            "status-panel-v2 watcher fallback must keep recent tool/subagent monitoring visible"
        );
    }

    #[test]
    fn watcher_status_panel_bound_placeholder_keeps_events_on_status_panel() {
        assert!(
            watcher_placeholder_uses_status_panel_only(true, Some(MessageId::new(99))),
            "bound status-panel-v2 watcher should avoid duplicating recent events in placeholder"
        );
        assert!(
            !watcher_placeholder_inlines_live_events(false, true, Some(MessageId::new(99))),
            "recent events stay on the dedicated status panel when that message is available"
        );
    }

    /// #2110/#relay-commit: Claude TUI transcript jsonl can emit
    /// `system/stop_hook_summary` before late assistant text. It must remain
    /// a soft candidate so the watcher can debounce/readiness-confirm before
    /// committing the terminal relay.
    #[test]
    fn process_watcher_lines_treats_stop_hook_summary_as_soft_terminal_candidate() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"tui reply\"}]},\"sessionId\":\"sess-tui\"}\n",
            "{\"type\":\"system\",\"subtype\":\"stop_hook_summary\",\"sessionId\":\"sess-tui\",\"hookCount\":1,\"hasOutput\":true}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\" late tail\"}]},\"sessionId\":\"sess-tui\"}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(!outcome.found_result);
        assert!(outcome.soft_terminal_candidate);
        assert_eq!(
            outcome.terminal_kind,
            Some(WatcherTerminalKind::SoftStopHookSummary)
        );
        assert_eq!(full_response, "tui reply late tail");
        assert_eq!(state.last_session_id.as_deref(), Some("sess-tui"));
        assert!(buffer.trim().is_empty());
    }

    #[test]
    fn process_watcher_lines_for_turn_skips_pre_start_terminal_evidence() {
        let prior_assistant = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"old reply\"}]}}\n";
        let prior_stop =
            "{\"type\":\"system\",\"subtype\":\"stop_hook_summary\",\"sessionId\":\"old\"}\n";
        let turn_start_offset = (prior_assistant.len() + prior_stop.len()) as u64;
        let mut buffer = format!("{prior_assistant}{prior_stop}");
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome = process_watcher_lines_for_turn(
            &mut buffer,
            &mut state,
            &mut full_response,
            &mut tool_state,
            Some(0),
            Some(turn_start_offset),
        );

        assert!(!outcome.found_result);
        assert!(!outcome.soft_terminal_candidate);
        assert_eq!(outcome.terminal_evidence_offset, None);
        assert_eq!(outcome.pre_turn_bytes_skipped, turn_start_offset as usize);
        assert!(full_response.is_empty());
        assert!(buffer.is_empty());
    }

    #[test]
    fn process_watcher_lines_for_turn_keeps_valid_post_start_terminal_evidence() {
        let prior_assistant = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"old reply\"}]}}\n";
        let prior_stop =
            "{\"type\":\"system\",\"subtype\":\"stop_hook_summary\",\"sessionId\":\"old\"}\n";
        let current_assistant = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"new reply\"}]}}\n";
        let current_result = "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"new reply\",\"session_id\":\"new\"}\n";
        let turn_start_offset = (prior_assistant.len() + prior_stop.len()) as u64;
        let mut buffer =
            format!("{prior_assistant}{prior_stop}{current_assistant}{current_result}");
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome = process_watcher_lines_for_turn(
            &mut buffer,
            &mut state,
            &mut full_response,
            &mut tool_state,
            Some(0),
            Some(turn_start_offset),
        );

        assert!(outcome.found_result);
        assert_eq!(outcome.terminal_kind, Some(WatcherTerminalKind::HardResult));
        assert_eq!(
            outcome.terminal_evidence_offset,
            Some(turn_start_offset + current_assistant.len() as u64)
        );
        assert_eq!(outcome.pre_turn_bytes_skipped, turn_start_offset as usize);
        assert_eq!(full_response, "new reply");
        assert!(buffer.is_empty());
    }

    #[test]
    fn process_watcher_lines_splits_late_user_prompt_after_assistant_text() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"previous reply\"}]},\"sessionId\":\"sess-tui\"}\n",
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"ssh direct prompt\"}]},\"sessionId\":\"sess-tui\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"ssh direct reply\"}]},\"sessionId\":\"sess-tui\"}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(!outcome.found_result);
        assert!(outcome.soft_terminal_candidate);
        assert_eq!(
            outcome.terminal_kind,
            Some(WatcherTerminalKind::SoftUserBoundary)
        );
        assert_eq!(full_response, "previous reply");
        assert!(
            buffer.contains("ssh direct prompt"),
            "the next turn must remain buffered for the following watcher pass"
        );
        assert!(!buffer.contains("previous reply"));
    }

    #[test]
    fn process_watcher_lines_does_not_split_tool_result_user_messages() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"using tool\"},{\"type\":\"tool_use\",\"name\":\"Bash\",\"input\":{}}]},\"sessionId\":\"sess-tui\"}\n",
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"content\":\"done\"}]},\"sessionId\":\"sess-tui\"}\n",
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\" after tool\"}]},\"sessionId\":\"sess-tui\"}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(!outcome.soft_terminal_candidate);
        assert_eq!(full_response, "using tool after tool");
        assert!(buffer.trim().is_empty());
    }

    #[test]
    fn process_watcher_lines_omits_bulk_success_and_keeps_following_assistant_text() {
        let raw = format!("bulk-secret-{}", "x".repeat(9 * 1024));
        let mut buffer = format!(
            "{}\n{}\n",
            serde_json::json!({
                "type": "user",
                "message": {"role": "user", "content": [{
                    "type": "tool_result", "content": raw, "is_error": false
                }]}
            }),
            serde_json::json!({
                "type": "assistant",
                "message": {"content": [{"type": "text", "text": "relay survived"}]}
            })
        );
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();
        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);
        assert!(!outcome.soft_terminal_candidate && buffer.is_empty());
        assert_eq!(full_response, "relay survived");
        assert!(tool_state.placeholder_events.is_empty());
    }

    #[test]
    fn process_watcher_lines_strips_leading_tui_no_response_before_result() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"No response requested.\\n\\nreal answer\"}]},\"sessionId\":\"sess-tui\"}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\",\"session_id\":\"sess-tui\"}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(outcome.assistant_text_seen);
        assert_eq!(full_response, "real answer");
    }

    #[test]
    fn process_watcher_lines_suppresses_pure_tui_no_response() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"No response requested.\"}]},\"sessionId\":\"sess-tui\"}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"done\",\"session_id\":\"sess-tui\"}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(!outcome.assistant_text_seen);
        assert!(full_response.is_empty());
    }

    /// #3344 round 2 — provenance gate: the codex legacy tmux wrapper's
    /// `emit_success_result` ALWAYS stamps the terminal frame with top-level
    /// `input_tokens`/`output_tokens`. That marker means the frame's nested
    /// `usage` is KNOWN-cumulative codex-legacy accounting, so it is NEVER
    /// adopted as per-call context — even this small, early, plausible-looking
    /// value (`input+cache_read = 1000`). RATIONALE: an honest-unknown CTW
    /// beats a sometimes-right number. The same wrapper's nested `usage` grows
    /// from per-call-ish early to wildly cumulative as the session advances; a
    /// magnitude-only gate would adopt it through the entire blind window
    /// (model window → 2M) and silently drift into a misleading 100%. The
    /// legitimate per-call occupancy for codex turns flows from the session
    /// `token_count` records (`info.last_token_usage`, #3331), not this frame.
    /// (On base this small value was adopted; now suppressed.)
    #[test]
    fn process_watcher_lines_suppresses_codex_legacy_early_per_call_usage() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"codex reply\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"codex reply\",\"session_id\":\"sess-cdx\",\"duration_ms\":12,\"input_tokens\":30000,\"output_tokens\":50,\"usage\":{\"input_tokens\":400,\"cache_read_input_tokens\":600,\"output_tokens\":50}}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(!state.saw_per_message_usage);
        // Codex-legacy provenance (top-level token fields present) → suppress.
        assert_eq!(state.accum_input_tokens, 0);
        assert_eq!(state.accum_cache_read_tokens, 0);
        assert_eq!(state.accum_cache_create_tokens, 0);
        assert_eq!(state.accum_output_tokens, 0);
        let usage = crate::db::turns::TurnTokenUsage {
            input_tokens: state.accum_input_tokens,
            cache_create_tokens: state.accum_cache_create_tokens,
            cache_read_tokens: state.accum_cache_read_tokens,
            output_tokens: state.accum_output_tokens,
        };
        assert_eq!(usage.context_occupancy_input_tokens(), 0);
    }

    /// #3344 round 2 — blind-window pin: a codex-legacy frame whose nested
    /// occupancy (500_000) sits ABOVE the model window (~258_400) but BELOW the
    /// old 2M magnitude ceiling. The retired magnitude-only gate adopted this,
    /// rendering a bogus clamped 100%. The provenance gate (top-level token
    /// marker) suppresses it regardless of magnitude. (Fails on base, where the
    /// 500k occupancy cleared the ceiling and was adopted.)
    #[test]
    fn process_watcher_lines_suppresses_codex_legacy_blind_window_usage() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"codex reply\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"codex reply\",\"session_id\":\"sess-cdx\",\"duration_ms\":12,\"input_tokens\":500000,\"output_tokens\":400,\"usage\":{\"input_tokens\":100000,\"cache_read_input_tokens\":400000,\"output_tokens\":400}}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(!state.saw_per_message_usage);
        assert_eq!(state.accum_input_tokens, 0);
        assert_eq!(state.accum_cache_read_tokens, 0);
        assert_eq!(state.accum_cache_create_tokens, 0);
        assert_eq!(state.accum_output_tokens, 0);
    }

    /// #3344 regression: the Codex legacy tmux wrapper can hand back a terminal
    /// frame carrying session-cumulative accounting (the issue's exact values:
    /// `cache_read_input_tokens=4022400`, `input_tokens=344752`, ~4.37M occupancy
    /// against a 258400 window). With the top-level codex-legacy marker present,
    /// the provenance gate suppresses it; the accumulators stay zero so
    /// `stream_line_state_token_usage` returns None and the panel/recap render an
    /// honest "unknown" instead of a clamped, misleading 100%.
    #[test]
    fn process_watcher_lines_suppresses_cumulative_codex_wrapper_result_usage() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"codex reply\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"codex reply\",\"session_id\":\"sess-cdx\",\"duration_ms\":12,\"input_tokens\":4367152,\"output_tokens\":1280,\"usage\":{\"input_tokens\":344752,\"cache_read_input_tokens\":4022400,\"output_tokens\":1280}}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(!state.saw_per_message_usage);
        // The cumulative shape is suppressed: every accumulator stays zero, so
        // context occupancy is unknown (no false full-window render).
        assert_eq!(state.accum_input_tokens, 0);
        assert_eq!(state.accum_cache_read_tokens, 0);
        assert_eq!(state.accum_cache_create_tokens, 0);
        assert_eq!(state.accum_output_tokens, 0);
        let usage = crate::db::turns::TurnTokenUsage {
            input_tokens: state.accum_input_tokens,
            cache_create_tokens: state.accum_cache_create_tokens,
            cache_read_tokens: state.accum_cache_read_tokens,
            output_tokens: state.accum_output_tokens,
        };
        assert_eq!(usage.context_occupancy_input_tokens(), 0);
    }

    /// #3344 round 2 — backstop pin: a NON-codex terminal-usage provider (no
    /// top-level token marker) whose nested occupancy clears the 2M
    /// defense-in-depth ceiling is still rejected. The provenance gate does not
    /// fire (no codex marker), so the magnitude backstop catches the
    /// millions-scale garbage from any provider.
    #[test]
    fn process_watcher_lines_backstop_rejects_other_provider_millions_usage() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"reply\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"reply\",\"session_id\":\"sess-x\",\"duration_ms\":12,\"usage\":{\"input_tokens\":3000000,\"cache_read_input_tokens\":1000000,\"output_tokens\":120}}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(!state.saw_per_message_usage);
        assert_eq!(state.accum_input_tokens, 0);
        assert_eq!(state.accum_cache_read_tokens, 0);
        assert_eq!(state.accum_cache_create_tokens, 0);
        assert_eq!(state.accum_output_tokens, 0);
    }

    /// #3344 fallback regression: a provider that legitimately reports per-call
    /// token counts solely on the terminal `result.usage` (Qwen's tmux wrapper)
    /// must keep flowing through the fallback unchanged — the cumulative-shape
    /// guard never trips for sane per-call magnitudes.
    #[test]
    fn process_watcher_lines_keeps_per_call_terminal_usage_fallback() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"qwen reply\"}]}}\n",
            "{\"type\":\"result\",\"subtype\":\"success\",\"result\":\"qwen reply\",\"session_id\":\"sess-qwen\",\"duration_ms\":7,\"usage\":{\"input_tokens\":1200,\"cache_read_input_tokens\":300,\"cache_creation_input_tokens\":80,\"output_tokens\":256}}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(outcome.found_result);
        assert!(!state.saw_per_message_usage);
        assert_eq!(state.accum_input_tokens, 1200);
        assert_eq!(state.accum_cache_read_tokens, 300);
        assert_eq!(state.accum_cache_create_tokens, 80);
        assert_eq!(state.accum_output_tokens, 256);
    }

    #[test]
    fn process_watcher_lines_strips_tui_no_response_before_stop_hook_summary() {
        let mut buffer = concat!(
            "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"No response requested.\\n\\nssh direct answer\"}]},\"sessionId\":\"sess-tui\"}\n",
            "{\"type\":\"system\",\"subtype\":\"stop_hook_summary\",\"sessionId\":\"sess-tui\",\"hookCount\":1,\"hasOutput\":true}\n",
        )
        .to_string();
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        let outcome =
            process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(!outcome.found_result);
        assert!(outcome.soft_terminal_candidate);
        assert_eq!(full_response, "ssh direct answer");
    }

    // ---- #4275: watcher/jsonl segment-boundary separator ----

    fn assistant_text_line(text: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {"content": [{"type": "text", "text": text}]},
            "sessionId": "sess-4275",
        })
        .to_string()
            + "\n"
    }

    fn assistant_tool_use_line(name: &str) -> String {
        serde_json::json!({
            "type": "assistant",
            "message": {"content": [{
                "type": "tool_use",
                "name": name,
                "input": {"command": "timeout 5 sleep 10"},
            }]},
            "sessionId": "sess-4275",
        })
        .to_string()
            + "\n"
    }

    /// Drive two discrete assistant text blocks through the real watcher entry
    /// point and return the accumulated `full_response`.
    fn watcher_full_response_for_two_segments(first: &str, second: &str) -> String {
        let mut buffer = String::new();
        buffer.push_str(&assistant_text_line(first));
        buffer.push_str(&assistant_text_line(second));
        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();
        process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);
        full_response
    }

    /// #4275 live repro (2026-07-08): Claude interleaves assistant text with a
    /// `tool_use`, so the watcher/jsonl path sees the pre-tool and post-tool
    /// prose as two discrete `assistant` text events. Before the fix a bare
    /// `push_str` glued them into one run-on line ("…정리합니다.macOS엔…"); the
    /// guard must insert a blank-line boundary at the sentence break.
    #[test]
    fn process_watcher_lines_inserts_boundary_between_tool_split_assistant_segments() {
        let mut buffer = String::new();
        buffer.push_str(&assistant_text_line("필요한 명령어를 정리합니다."));
        buffer.push_str(&assistant_tool_use_line("Bash"));
        buffer.push_str(&assistant_text_line("macOS엔 timeout이 없네요."));

        let mut state = StreamLineState::new();
        let mut full_response = String::new();
        let mut tool_state = WatcherToolState::new();

        process_watcher_lines(&mut buffer, &mut state, &mut full_response, &mut tool_state);

        assert!(
            full_response.contains("정리합니다.\n\nmacOS"),
            "split assistant segments must gain a blank-line boundary, got: {full_response:?}"
        );
    }

    /// Negative: a decimal fraction spanning the split ("1." + "2") must NOT
    /// gain a boundary — the guard evaluates but declines.
    #[test]
    fn process_watcher_lines_keeps_decimal_across_split_segments() {
        let full = watcher_full_response_for_two_segments("버전은 1.", "2로 올렸습니다.");
        assert!(
            !full.contains("\n\n"),
            "decimal fraction must not gain a boundary, got: {full:?}"
        );
        assert!(
            full.contains("1.2"),
            "decimal must stay glued, got: {full:?}"
        );
    }

    /// Negative: a file extension spanning the split ("config." + "yaml") must
    /// NOT gain a boundary.
    #[test]
    fn process_watcher_lines_keeps_file_extension_across_split_segments() {
        let full = watcher_full_response_for_two_segments("설정은 config.", "yaml에 있습니다.");
        assert!(
            !full.contains("\n\n"),
            "file extension must not gain a boundary, got: {full:?}"
        );
        assert!(
            full.contains("config.yaml"),
            "extension must stay glued, got: {full:?}"
        );
    }

    /// Negative: a markdown list continuation ("- item." + "- next") must NOT
    /// gain a boundary that would fracture the list structure.
    #[test]
    fn process_watcher_lines_keeps_markdown_continuation_across_split_segments() {
        let full = watcher_full_response_for_two_segments("- 첫 항목.", "- 다음 항목");
        assert!(
            !full.contains("\n\n"),
            "markdown list continuation must not gain a boundary, got: {full:?}"
        );
    }

    /// Negative (#4275 r2, codex review Finding 1): when the accumulated
    /// response ends INSIDE an open ``` code fence, a fenced line ending with
    /// sentence punctuation ("complete.") followed by a non-whitespace-start
    /// segment must NOT gain a boundary — "\n\n" inside fenced content would
    /// corrupt it. Mirrors the #3608 streaming-path
    /// `streamed_text_inside_open_code_fence` guard.
    #[test]
    fn process_watcher_lines_keeps_open_code_fence_interior_across_split_segments() {
        let full =
            watcher_full_response_for_two_segments("```bash\necho complete.", "echo two\n```");
        assert!(
            !full.contains("complete.\n\n"),
            "open-fence interior must not gain a boundary, got: {full:?}"
        );
        assert!(
            full.contains("complete.echo two"),
            "fenced content must stay glued, got: {full:?}"
        );
    }

    /// Positive pair for the open-fence negative: once the fence is CLOSED,
    /// a genuine sentence boundary after the fence gains the separator again.
    #[test]
    fn process_watcher_lines_inserts_boundary_after_closed_code_fence_segment() {
        let full = watcher_full_response_for_two_segments(
            "```bash\necho hi\n```\n실행을 완료했습니다.",
            "다음 단계로 넘어갑니다.",
        );
        assert!(
            full.contains("완료했습니다.\n\n다음"),
            "closed fence must not suppress a genuine boundary, got: {full:?}"
        );
    }

    /// Negative: text with no sentence-terminal punctuation ("…계속" + "진행…")
    /// is a mid-sentence continuation and must NOT gain a boundary.
    #[test]
    fn process_watcher_lines_keeps_unterminated_run_on_across_split_segments() {
        let full = watcher_full_response_for_two_segments("이 작업은 계속", "진행됩니다.");
        assert!(
            !full.contains("\n\n"),
            "unterminated continuation must not gain a boundary, got: {full:?}"
        );
        assert!(
            full.contains("계속진행됩니다"),
            "continuation must stay glued, got: {full:?}"
        );
    }
}
