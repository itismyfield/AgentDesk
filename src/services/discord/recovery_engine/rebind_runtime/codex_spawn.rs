use super::*;

pub(super) struct WriterInput {
    initial_composed_response: String,
    suppression: String,
    replay_events: Vec<serde_json::Value>,
    prepared_output: Option<std::fs::File>,
}

impl WriterInput {
    pub(super) fn ordinary(seed: String, replay_events: Vec<serde_json::Value>) -> Self {
        Self {
            initial_composed_response: seed.clone(),
            suppression: seed,
            replay_events,
            prepared_output: None,
        }
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        String,
        String,
        Vec<serde_json::Value>,
        Option<std::fs::File>,
    ) {
        (
            self.suppression,
            self.initial_composed_response,
            self.replay_events,
            self.prepared_output,
        )
    }
}

pub(super) fn open_writer_output(
    path: &std::path::Path,
    prepared: Option<std::fs::File>,
) -> Result<std::fs::File, String> {
    match prepared {
        Some(output) => Ok(output),
        None => std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| format!("open {}: {error}", path.display())),
    }
}

#[cfg(all(test, unix))]
#[derive(Clone, Default)]
pub(in crate::services::discord::recovery_engine) struct StartGate(
    std::sync::Arc<(std::sync::Mutex<Option<bool>>, std::sync::Condvar)>,
);

#[cfg(all(test, unix))]
impl StartGate {
    pub(in crate::services::discord::recovery_engine) fn wait(&self) -> bool {
        let (lock, changed) = &*self.0;
        let mut state = lock.lock().unwrap_or_else(|e| e.into_inner());
        while state.is_none() {
            state = changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        *state == Some(true)
    }
    fn is_open(&self) -> bool {
        self.0.0.lock().unwrap_or_else(|e| e.into_inner()).as_ref() == Some(&true)
    }
    pub(in crate::services::discord::recovery_engine) fn finish(&self, open: bool) {
        let (lock, changed) = &*self.0;
        *lock.lock().unwrap_or_else(|e| e.into_inner()) = Some(open);
        changed.notify_all();
    }
}

#[cfg(all(test, unix))]
pub(in crate::services::discord::recovery_engine) struct PreparedWriter {
    pub(in crate::services::discord::recovery_engine) gate: StartGate,
    pub(in crate::services::discord::recovery_engine) cancel:
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<Result<(), String>>>,
}

#[cfg(all(test, unix))]
impl PreparedWriter {
    pub(in crate::services::discord::recovery_engine) fn join(mut self) -> Result<(), String> {
        self.thread
            .take()
            .ok_or("writer handle missing")?
            .join()
            .map_err(|_| "writer panicked")?
    }
}

#[cfg(all(test, unix))]
impl Drop for PreparedWriter {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.gate.finish(false);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
}

#[cfg(all(test, unix))]
static VERIFIED_READER_STACK_SIZE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

#[cfg(all(test, unix))]
pub(in crate::services::discord::recovery_engine) struct ReaderThreadFault(usize);

#[cfg(all(test, unix))]
impl Drop for ReaderThreadFault {
    fn drop(&mut self) {
        VERIFIED_READER_STACK_SIZE.store(self.0, std::sync::atomic::Ordering::Release);
    }
}

#[cfg(all(test, unix))]
pub(in crate::services::discord::recovery_engine) fn fail_reader_thread_for_test()
-> ReaderThreadFault {
    ReaderThreadFault(
        VERIFIED_READER_STACK_SIZE.swap(usize::MAX / 2, std::sync::atomic::Ordering::AcqRel),
    )
}

#[cfg(all(test, unix))]
pub(in crate::services::discord::recovery_engine) fn prepare_verified_writer(
    path: std::path::PathBuf,
    mut reader: super::super::legacy_restart_replay::reader::VerifiedReader,
    seed: String,
    generation: (CodexRebindRelayGenerationGate, u64),
) -> Result<PreparedWriter, String> {
    let output = open_writer_output(&path, None)?;
    let input = WriterInput {
        initial_composed_response: seed,
        suppression: String::new(),
        replay_events: Vec::new(),
        prepared_output: Some(output),
    };
    let gate = StartGate::default();
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let thread_gate = gate.clone();
    let thread_cancel = cancel.clone();
    let thread = std::thread::Builder::new()
        .name("codex_verified_replay_writer".into())
        .spawn(move || {
            if input.initial_composed_response != reader.composed_prefix()
                || !input.suppression.is_empty()
                || !input.replay_events.is_empty()
            {
                let error = "verified writer input does not match proof".to_string();
                let _ = ready_tx.send(Err(error.clone()));
                return Err(error);
            }
            let (sender, receiver) = std::sync::mpsc::channel();
            let feed_gate = thread_gate.clone();
            let feed_cancel = thread_cancel.clone();
            let builder = std::thread::Builder::new().name("codex_verified_replay_reader".into());
            let stack = VERIFIED_READER_STACK_SIZE.load(std::sync::atomic::Ordering::Acquire);
            let builder = if stack == 0 {
                builder
            } else {
                builder.stack_size(stack)
            };
            let feeder = builder
                .spawn(move || {
                    if !feed_gate.wait() {
                        return Ok(());
                    }
                    while feed_gate.is_open()
                        && !feed_cancel.load(std::sync::atomic::Ordering::Acquire)
                    {
                        for message in reader.poll().map_err(|hold| format!("{hold:?}"))? {
                            sender
                                .send(message)
                                .map_err(|_| "writer disconnected".to_string())?;
                        }
                        if reader.finished() {
                            return Ok(());
                        }
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err("reader cancelled".to_string())
                })
                .map_err(|e| e.to_string());
            let ready = feeder.as_ref().map(|_| ()).map_err(Clone::clone);
            let _ = ready_tx.send(ready);
            let feeder = feeder?;
            if !thread_gate.wait() {
                return feeder.join().map_err(|_| "reader panicked")?;
            }
            let result = write_codex_rebind_normalized_stream_for_generation(
                &path,
                receiver,
                input,
                &generation.0,
                generation.1,
                None,
            );
            if result.is_err() {
                thread_gate.finish(false);
            }
            let feed_result = feeder.join().map_err(|_| "reader panicked")?;
            result.and(feed_result)
        })
        .map_err(|e| e.to_string())?;
    let prepared = PreparedWriter {
        gate,
        cancel,
        thread: Some(thread),
    };
    ready_rx
        .recv()
        .map_err(|_| "writer preparation disconnected".to_string())??;
    Ok(prepared)
}

pub(in crate::services::discord::recovery_engine) fn spawn_codex_tui_rebind_relay_output(
    tmux_session_name: &str,
    rollout_path: &str,
    raw_start_offset: u64,
    truncate_relay_output: bool,
    watcher_cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    session_id: Option<String>,
    already_relayed_response: String,
    already_normalized_replay_events: Vec<serde_json::Value>,
) -> Result<String, RebindError> {
    let relay_output_path =
        crate::services::tmux_common::session_temp_path(tmux_session_name, "jsonl");
    let (relay_generation_gate, relay_generation) =
        crate::services::tmux_common::with_tmux_source_authority(tmux_session_name, |authority| {
            use crate::services::tui_prompt_dedupe as dedupe;
            let verified = dedupe::codex_verified_requires_proof_under_source_authority(authority);
            if !dedupe::codex_verified_source_allowed_under_source_authority(
                authority,
                rollout_path,
                session_id.as_deref(),
            ) || (truncate_relay_output && verified)
            {
                return Err(RebindError::Internal(
                    "Codex rebind requires source proof, permission and retained relay checkpoint"
                        .into(),
                ));
            }
            let generation =
                prepare_codex_rebind_relay_generation(&relay_output_path, truncate_relay_output)?;
            let installed = crate::services::codex_tui::session::install_codex_tui_runtime_binding_under_source_authority(
                authority,
                Some(raw_start_offset),
                dedupe::TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::CodexTui,
                    output_path: rollout_path.to_string(),
                    relay_output_path: Some(relay_output_path.clone()),
                    input_fifo_path: None,
                    session_id: session_id.clone(),
                    last_offset: raw_start_offset,
                    relay_last_offset: Some(0),
                },
            );
            if (verified && !installed)
                || !dedupe::codex_verified_source_allowed_under_source_authority(
                    authority,
                    rollout_path,
                    session_id.as_deref(),
                )
            {
                return Err(RebindError::Internal(
                    "Codex rebind publication was rejected".into(),
                ));
            }
            Ok(generation)
        })?;

    let tmux_session_name = tmux_session_name.to_string();
    let rollout_path = std::path::PathBuf::from(rollout_path);
    let relay_path = std::path::PathBuf::from(&relay_output_path);
    let watcher_cancel_for_writer = watcher_cancel.clone();
    #[cfg(test)]
    REBIND_WRITER_SPAWNS.with(|count| count.set(count.get() + 1));
    std::thread::Builder::new()
        .name("codex_tui_rebind_relay_writer".to_string())
        .spawn(move || {
            let (sender, receiver) =
                std::sync::mpsc::channel::<crate::services::agent_protocol::StreamMessage>();
            let tail_rollout_path = rollout_path.clone();
            let tail_tmux_session_name = tmux_session_name.clone();
            let tail_session_id = session_id.clone();
            let tail_cancel_token = std::sync::Arc::new(crate::services::provider::CancelToken::new());
            let cancel_bridge_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
            let watcher_cancel_for_bridge = watcher_cancel_for_writer.clone();
            let tail_cancel_for_bridge = tail_cancel_token.clone();
            let cancel_bridge_done_for_thread = cancel_bridge_done.clone();
            let cancel_bridge_handle = std::thread::Builder::new()
                .name("codex_tui_rebind_cancel_bridge".to_string())
                .spawn(move || {
                    while !cancel_bridge_done_for_thread
                        .load(std::sync::atomic::Ordering::Relaxed)
                    {
                        if watcher_cancel_for_bridge.load(std::sync::atomic::Ordering::Relaxed) {
                            tail_cancel_for_bridge
                                .cancelled
                                .store(true, std::sync::atomic::Ordering::Relaxed);
                            break;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                });
            if let Err(error) = &cancel_bridge_handle {
                tracing::warn!(
                    tmux_session = %tmux_session_name,
                    error = %error,
                    "failed to spawn Codex TUI rebind cancel bridge"
                );
            }
            let watcher_cancel_for_alive = watcher_cancel_for_writer.clone();
            let tail_handle = std::thread::Builder::new()
                .name("codex_tui_rebind_rollout_tail".to_string())
                .spawn(move || {
                    crate::services::codex_tui::rollout_tail::tail_rollout_file_from_offset_for_tmux(
                        &tail_rollout_path,
                        raw_start_offset,
                        tail_session_id.as_deref(),
                        sender,
                        Some(tail_cancel_token),
                        || {
                            !watcher_cancel_for_alive
                                .load(std::sync::atomic::Ordering::Relaxed)
                                && crate::services::tui_prompt_dedupe::codex_verified_source_allowed(
                                    &tail_tmux_session_name, &tail_rollout_path.to_string_lossy(), tail_session_id.as_deref())
                                && not_dead(observe_liveness(&tail_tmux_session_name, None))
                        },
                        &tail_tmux_session_name,
                    )
                });

            let writer_result = write_codex_rebind_normalized_stream_for_generation(
                &relay_path,
                receiver,
                WriterInput::ordinary(already_relayed_response, already_normalized_replay_events),
                &relay_generation_gate,
                relay_generation,
                Some((&tmux_session_name, &rollout_path, session_id.as_deref())),
            );
            if let Err(error) = &writer_result {
                tracing::warn!(
                    tmux_session = %tmux_session_name,
                    relay_output_path = %relay_path.display(),
                    error = %error,
                    "Codex TUI rebind relay writer failed"
                );
            }

            match tail_handle {
                Ok(handle) => match handle.join() {
                    Ok(Ok(read_result)) => {
                        let (final_offset, advance_cursor) = match read_result {
                            crate::services::provider::ReadOutputResult::Completed { offset }
                            | crate::services::provider::ReadOutputResult::SessionDied {
                                offset,
                            } => (offset, true),
                            crate::services::provider::ReadOutputResult::Cancelled { offset } => {
                                (offset, false)
                            }
                        };
                        if writer_result.is_ok() && advance_cursor {
                            crate::services::codex_tui::session::advance_codex_tui_runtime_binding_and_marker_offset(
                                &tmux_session_name,
                                &rollout_path,
                                final_offset,
                            );
                        } else if !advance_cursor {
                            tracing::warn!(
                                tmux_session = %tmux_session_name,
                                rollout_path = %rollout_path.display(),
                                final_offset,
                                "Codex TUI rebind relay was cancelled with watcher; preserving previous raw rollout cursor for retry"
                            );
                        } else {
                            tracing::warn!(
                                tmux_session = %tmux_session_name,
                                rollout_path = %rollout_path.display(),
                                final_offset,
                                "Codex TUI rebind relay writer failed; preserving previous raw rollout cursor for retry"
                            );
                        }
                    }
                    Ok(Err(error)) => {
                        tracing::warn!(
                            tmux_session = %tmux_session_name,
                            rollout_path = %rollout_path.display(),
                            error = %error,
                            "Codex TUI rebind rollout tail failed"
                        );
                    }
                    Err(_) => {
                        tracing::warn!(
                            tmux_session = %tmux_session_name,
                            rollout_path = %rollout_path.display(),
                            "Codex TUI rebind rollout tail panicked"
                        );
                    }
                },
                Err(error) => {
                    tracing::warn!(
                        tmux_session = %tmux_session_name,
                        rollout_path = %rollout_path.display(),
                        error = %error,
                        "failed to spawn Codex TUI rebind rollout tail"
                    );
                }
            }
            cancel_bridge_done.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Ok(handle) = cancel_bridge_handle {
                let _ = handle.join();
            }
        })
        .map_err(|error| {
            RebindError::Internal(format!("spawn Codex TUI rebind relay writer: {error}"))
        })?;

    Ok(relay_output_path)
}
