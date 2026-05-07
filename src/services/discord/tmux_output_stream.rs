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
    if (shared.placeholder_live_events_enabled || shared.status_panel_v2_enabled)
        && !events.is_empty()
    {
        shared.placeholder_live_events.push_many(channel_id, events);
        dirty = true;
    }
    if shared.status_panel_v2_enabled && !status_events.is_empty() {
        shared
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
) -> String {
    if shared.status_panel_v2_enabled {
        return crate::services::discord::formatting::build_processing_status_block(indicator);
    }
    let status_block = crate::services::discord::formatting::build_placeholder_status_block(
        indicator,
        prev_tool_status,
        current_tool_line,
        full_response,
    );
    if shared.placeholder_live_events_enabled
        && let Some(block) = shared.placeholder_live_events.render_block(channel_id)
    {
        return format!("{status_block}\n{block}");
    }
    status_block
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
    let mut outcome = WatcherLineOutcome::default();

    while let Some(pos) = buffer.find('\n') {
        let line: String = buffer.drain(..=pos).collect();
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Parse the JSON line
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(trimmed) {
            observe_stream_context(&val, state);
            tool_state.record_placeholder_events_from_json(&val);
            let event_type = val.get("type").and_then(|t| t.as_str()).unwrap_or("");
            match event_type {
                "assistant" => {
                    if let Some(message) = val.get("message") {
                        if let Some(model) = message.get("model").and_then(|value| value.as_str()) {
                            state.last_model = Some(model.to_string());
                        }
                        if let Some(usage) = message.get("usage") {
                            // #1918 — replace, do not accumulate. Each assistant
                            // message's `usage` describes one API call's prompt;
                            // the LAST one is the current context occupancy. The
                            // status panel's Context line summed these values, so
                            // accumulating across a multi-call (tool-use loop) turn
                            // inflated the displayed usage past the window size.
                            state.accum_input_tokens = usage
                                .get("input_tokens")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(state.accum_input_tokens);
                            state.accum_cache_read_tokens = usage
                                .get("cache_read_input_tokens")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(state.accum_cache_read_tokens);
                            state.accum_cache_create_tokens = usage
                                .get("cache_creation_input_tokens")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(state.accum_cache_create_tokens);
                            state.accum_output_tokens = usage
                                .get("output_tokens")
                                .and_then(|value| value.as_u64())
                                .unwrap_or(state.accum_output_tokens);
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
                    //    (accumulated text is stale pre-tool narration)
                    if !outcome.is_prompt_too_long
                        && !outcome.is_auth_error
                        && !outcome.is_provider_overloaded
                        && !result_str.is_empty()
                    {
                        if full_response.is_empty()
                            || (tool_state.any_tool_used && !tool_state.has_post_tool_text)
                        {
                            full_response.clear();
                            full_response.push_str(&result_str);
                        }
                    }
                    // #1918 — context-window usage uses the LAST API call's
                    // per-message `usage`, captured during the assistant-message
                    // branch above. Older revisions copied result.usage here on
                    // the assumption it reflected the final call, but Claude
                    // CLI's result event in multi-call (tool-use loop) turns
                    // reports turn-cumulative counts and inflated the displayed
                    // context occupancy past the window size. Trust the
                    // per-message stream instead.

                    state.final_result = Some(String::new());
                    outcome.found_result = true;
                    // #1216: stop after the first turn-terminating event so a
                    // buffer containing multiple completed turns (post-deploy
                    // backlog, paused watcher resume) does not merge their
                    // `assistant` text into a single `full_response`. The
                    // unprocessed tail stays in `buffer` for the next call.
                    break;
                }
                "system" => {
                    if val.get("subtype").and_then(|s| s.as_str()) == Some("init")
                        && let Some(session_id) =
                            val.get("session_id").and_then(|value| value.as_str())
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
                        }
                    }
                }
                _ => {}
            }
        } else if is_auth_error_message(trimmed) {
            outcome.found_result = true;
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
            outcome.found_result = true;
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
        }
    }

    outcome
}
