//! Tail state is replaced together when the verified binding advances.

use super::*;

pub(super) fn tail_rollout_file_until_assistant_response_with_pane_busy_probe<
    L: Into<PaneLiveness>,
>(
    rollout_path: &Path,
    mut start_offset: u64,
    mut initial_session_id: Option<String>,
    sender: &Sender<StreamMessage>,
    cancel_token: Option<Arc<CancelToken>>,
    mut is_alive: impl FnMut() -> L,
    options: RolloutTailOptions,
) -> Result<(ReadOutputResult, RolloutTailOutcome), String> {
    let RolloutTailOptions {
        terminal_drain,
        assistant_response_deadline,
        pending_tool_call_deadline,
        enable_task_complete_fast_path,
        legacy_terminal_drain,
        apply_pending_tool_deadline_without_assistant_text,
        tmux_session_name,
        discord_origin_prompt,
        mut pane_busy_probe,
        pane_busy_veto_cap,
        terminal_range_eligible,
        own_turn,
        ..
    } = options;
    let mut current_path = rollout_path.to_path_buf();
    let mut source_pin = super::super::verified_tail::SourcePin::new(
        tmux_session_name.as_deref(),
        rollout_path,
        initial_session_id.as_deref(),
    );
    let mut file = if let Some(pin) = source_pin.clone() {
        let Some(opened) = pin.open(cancel_token.as_deref(), start_offset) else {
            return Ok((
                ReadOutputResult::Cancelled {
                    offset: start_offset,
                },
                RolloutTailOutcome {
                    rollout_path: current_path.clone(),
                    harvest: Default::default(),
                    lines_read: 0,
                    bytes_read: 0,
                    final_offset: start_offset,
                    session_id: initial_session_id,
                },
            ));
        };
        current_path = opened.path;
        start_offset = opened.offset;
        initial_session_id = opened.session_id;
        source_pin = Some(opened.pin);
        opened.file
    } else {
        std::fs::File::open(rollout_path)
            .map_err(|error| format!("open Codex rollout {}: {error}", rollout_path.display()))?
    };
    let file_len = file
        .metadata()
        .map_err(|error| format!("stat Codex rollout {}: {error}", rollout_path.display()))?
        .len();
    if source_pin.is_some() && start_offset > file_len {
        return Err("verified start is beyond the pinned native source".into());
    }
    let mut seek_offset = start_offset.min(file_len);
    file.seek(SeekFrom::Start(seek_offset))
        .map_err(|error| format!("seek Codex rollout {}: {error}", rollout_path.display()))?;

    let mut state = RolloutParseState {
        harvest: crate::services::session_backend::ReadHarvestStats {
            source_file: Some(
                crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(&file),
            ),
            ..Default::default()
        },
        session_id: initial_session_id,
        tmux_session_name,
        discord_origin_prompt,
        ..RolloutParseState::default()
    };
    let mut current_offset = seek_offset;
    let mut partial_line = Vec::new();
    let mut buf = [0u8; 8192];
    let mut last_output_at: Option<Instant> = None;
    let mut pane_busy_veto_started_at: Option<Instant> = None;
    let mut started_at = Instant::now();
    let mut hook_events = crate::services::claude_tui::hook_server::subscribe_hook_events();
    let mut terminal_range = (
        seek_offset,
        cancel_token.as_deref().and_then(CancelToken::turn_nonce),
        terminal_range_eligible,
    );
    // The producer suppresses all rollout emissions after cancellation.
    let mut source_identity =
        crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(&file);
    let mut sender = RelaySuppressionSender::new(sender, cancel_token.as_deref());
    sender.source_pin = source_pin.clone();
    sender.source_identity = source_identity;
    let accept = parser::RecordAcceptance::new(cancel_token.as_ref());
    let accept = accept.with_own_turn(own_turn.as_deref(), &mut state);

    let mut death = ConfirmedDeadPane::default();
    loop {
        if sender.cancel_observed() {
            return Ok((
                ReadOutputResult::Cancelled {
                    offset: current_offset,
                },
                outcome(&state, seek_offset, &current_path),
            ));
        }

        if let Some(pin) = &source_pin {
            if let super::super::verified_tail::PinState::Advanced(binding) =
                pin.state(source_identity)
            {
                sender.source_pin = None;
                current_path = PathBuf::from(&binding.output_path);
                source_pin = super::super::verified_tail::SourcePin::new(
                    state.tmux_session_name.as_deref(),
                    &current_path,
                    binding.session_id.as_deref(),
                );
                let Some(opened) = source_pin
                    .clone()
                    .and_then(|pin| pin.open(cancel_token.as_deref(), binding.last_offset))
                else {
                    continue;
                };
                file = opened.file;
                current_path = opened.path;
                source_pin = Some(opened.pin);
                source_identity =
                    crate::services::cluster::stream_relay::SourceFileIdentity::from_open_file(
                        &file,
                    );
                seek_offset = opened.offset;
                file.seek(SeekFrom::Start(seek_offset))
                    .map_err(|error| error.to_string())?;
                current_offset = seek_offset;
                state = RolloutParseState {
                    session_id: opened.session_id,
                    tmux_session_name: state.tmux_session_name.take(),
                    discord_origin_prompt: state.discord_origin_prompt.take(),
                    harvest: crate::services::session_backend::ReadHarvestStats {
                        source_file: Some(source_identity),
                        ..Default::default()
                    },
                    ..Default::default()
                };
                partial_line.clear();
                last_output_at = None;
                started_at = Instant::now();
                pane_busy_veto_started_at = None;
                death.reset();
                terminal_range.0 = seek_offset;
                sender.source_pin = source_pin.clone();
                sender.source_identity = source_identity;
                continue;
            }
            if !pin.wait(cancel_token.as_deref(), source_identity) {
                continue;
            }
            if !file
                .metadata()
                .is_ok_and(|metadata| metadata.len() >= current_offset)
            {
                std::thread::sleep(Duration::from_millis(100));
                continue;
            }
        }
        match file.read(&mut buf) {
            Ok(0) => {
                let mut pane_is_alive =
                    || !death.observe(is_alive().into(), Some((&file, current_offset)));
                let turn = (seek_offset, terminal_range.1);
                let tail = (&mut state, &mut partial_line);
                match parser::herdr_eof(
                    &sender,
                    tail,
                    &current_path,
                    turn,
                    &accept,
                    &mut pane_is_alive,
                ) {
                    Some(Some(ended)) => {
                        return Ok((ended, outcome(&state, seek_offset, &current_path)));
                    }
                    Some(None) => continue,
                    None => {}
                }
                if let Some(ended) = parser::native_turn_eof(
                    &sender,
                    &mut state,
                    &current_path,
                    terminal_range,
                    &accept,
                ) {
                    return Ok((ended, outcome(&state, seek_offset, &current_path)));
                }
                if try_process_complete_partial_line(
                    &mut partial_line,
                    &sender,
                    &mut state,
                    &accept,
                ) {
                    last_output_at = Some(Instant::now());
                    continue;
                }
                observe_codex_completion_hooks(&mut hook_events, &mut state);
                if let Some(finalize_path) =
                    explicit_finalize_path(&mut state, enable_task_complete_fast_path)
                {
                    emit_done(
                        &sender,
                        &mut state,
                        finalize_path,
                        &current_path,
                        current_offset,
                        terminal_range,
                    );
                    return Ok((
                        ReadOutputResult::Completed {
                            offset: current_offset,
                        },
                        outcome(&state, seek_offset, &current_path),
                    ));
                }
                if source_pin.is_some() {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                // Keep in-flight tools out of the drain timer. Legacy writers without lifecycle
                // signals use the longer drain; explicit short or zero overrides remain effective.
                let effective_drain = if state.seen_any_event_msg || terminal_drain.is_zero() {
                    terminal_drain
                } else {
                    match legacy_terminal_drain {
                        Some(legacy) if legacy > terminal_drain => legacy,
                        _ => terminal_drain,
                    }
                };
                if state.saw_assistant_text && !state.has_pending_tool_call() {
                    if effective_drain.is_zero()
                        || last_output_at.is_some_and(|at| at.elapsed() >= effective_drain)
                    {
                        if heuristic_finalize_allowed(&mut state, &current_path, current_offset) {
                            emit_done(
                                &sender,
                                &mut state,
                                RolloutFinalizePath::Heuristic,
                                &current_path,
                                current_offset,
                                terminal_range,
                            );
                            return Ok((
                                ReadOutputResult::Completed {
                                    offset: current_offset,
                                },
                                outcome(&state, seek_offset, &current_path),
                            ));
                        }
                        last_output_at = Some(Instant::now());
                    }
                }
                // Unresolved tools recover after inactivity, including tools started before assistant text.
                let tool_deadline_gate = state.has_pending_tool_call()
                    && (state.saw_assistant_text
                        || apply_pending_tool_deadline_without_assistant_text);
                if tool_deadline_gate
                    && let Some(deadline) = pending_tool_call_deadline
                    && last_output_at.is_some_and(|at| at.elapsed() >= deadline)
                {
                    let busy_signal = pane_busy_probe.as_mut().map_or(
                        super::super::input::CodexPaneBusySignal::Unavailable,
                        |probe| probe(),
                    );
                    if busy_signal == super::super::input::CodexPaneBusySignal::Fresh {
                        let veto_started_at =
                            *pane_busy_veto_started_at.get_or_insert_with(Instant::now);
                        if veto_started_at.elapsed() < pane_busy_veto_cap {
                            tracing::debug!(
                                rollout_path = %rollout_path.display(),
                                pending_keyed = state.pending_tool_calls.len(),
                                pending_unkeyed = state.pending_tool_calls_unkeyed,
                                veto_elapsed_ms = veto_started_at.elapsed().as_millis(),
                                veto_cap_ms = pane_busy_veto_cap.as_millis(),
                                "Codex rollout tail kept pending tool call open because the pane busy signal is fresh"
                            );
                            std::thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                        tracing::warn!(
                            rollout_path = %rollout_path.display(),
                            veto_elapsed_ms = veto_started_at.elapsed().as_millis(),
                            veto_cap_ms = pane_busy_veto_cap.as_millis(),
                            "Codex pane-busy veto reached its hard cap; failing open to pending-tool recovery"
                        );
                    }

                    let elapsed_secs = last_output_at
                        .map(|at| at.elapsed().as_secs())
                        .unwrap_or_default();
                    let tool_first = !state.saw_assistant_text;
                    tracing::warn!(
                        rollout_path = %rollout_path.display(),
                        elapsed_secs,
                        pending_keyed = state.pending_tool_calls.len(),
                        pending_unkeyed = state.pending_tool_calls_unkeyed,
                        tool_first,
                        "Codex rollout tail tool-call deadline expired; emitting Done to unblock turn"
                    );
                    let mut result_text = state.final_text.clone();
                    let warning = if tool_first {
                        format!(
                            "⚠ Codex tool call did not resolve within {}s before any assistant text — emitting empty response to unblock turn.",
                            elapsed_secs
                        )
                    } else {
                        format!(
                            "\n\n⚠ Codex tool call did not resolve within {}s — emitting partial response.",
                            elapsed_secs
                        )
                    };
                    if tool_first {
                        result_text = warning;
                    } else {
                        result_text.push_str(&warning);
                    }
                    sender.send(StreamMessage::Done {
                        result: result_text,
                        session_id: state.session_id.clone(),
                    });
                    return Ok((
                        ReadOutputResult::Completed {
                            offset: current_offset,
                        },
                        outcome(&state, seek_offset, &current_path),
                    ));
                }
                if !pane_is_alive() {
                    let result = if state.saw_assistant_text {
                        emit_done(
                            &sender,
                            &mut state,
                            RolloutFinalizePath::Heuristic,
                            &current_path,
                            current_offset,
                            (terminal_range.0, terminal_range.1, false),
                        );
                        ReadOutputResult::Completed {
                            offset: current_offset,
                        }
                    } else {
                        ReadOutputResult::SessionDied {
                            offset: current_offset,
                        }
                    };
                    return Ok((result, outcome(&state, seek_offset, &current_path)));
                }
                // Rollout activity resets the no-text deadline; post-text completion uses its own drain.
                // Confirm clean negatives and consume unread growth before synthesizing a timeout.
                let idle_elapsed =
                    last_output_at.map_or_else(|| started_at.elapsed(), |at| at.elapsed());
                let unread_growth = file.metadata().is_ok_and(|m| m.len() > current_offset);
                if !state.saw_assistant_text
                    && let Some(deadline) = assistant_response_deadline
                    && idle_elapsed >= deadline
                    && !death.is_confirming_death()
                    && !unread_growth
                {
                    let elapsed_secs = idle_elapsed.as_secs();
                    tracing::warn!(
                        rollout_path = %rollout_path.display(),
                        elapsed_secs,
                        "Codex rollout tail idle past deadline with no assistant response; emitting Done"
                    );
                    sender.send(StreamMessage::Done {
                        result: format!(
                            "⚠ Codex TUI produced no assistant response within {}s — turn timed out.",
                            elapsed_secs
                        ),
                        session_id: state.session_id.clone(),
                    });
                    return Ok((
                        ReadOutputResult::Completed {
                            offset: current_offset,
                        },
                        outcome(&state, seek_offset, &current_path),
                    ));
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(n) => {
                death.reset();
                current_offset += n as u64;
                partial_line.extend_from_slice(&buf[..n]);
                // One read can contain many records; cancellation suppresses every later send.
                while let Some(pos) = partial_line.iter().position(|byte| *byte == b'\n') {
                    // A stop leaves this and later bytes unaccepted in `partial_line`.
                    if accept.accept_next_record(&state) == parser::NextRecord::StopBefore {
                        break;
                    }
                    if let Some(pin) = &source_pin
                        && !pin.wait(cancel_token.as_deref(), source_identity)
                    {
                        break;
                    }
                    let line: Vec<u8> = partial_line.drain(..=pos).collect();
                    state.record(line.len());
                    if process_rollout_line_bytes(&line, &sender, &mut state) {
                        last_output_at = Some(Instant::now());
                    }
                }
            }
            Err(_) if source_pin.is_some() => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                return Err(format!(
                    "read Codex rollout {}: {error}",
                    rollout_path.display()
                ));
            }
        }
    }
}
