use super::*;
use crate::services::provider::CancelToken;
use std::io::Write;
use std::sync::mpsc;

pub(super) async fn spawn(
    context: Context,
    plan: proof::Proof,
    witnesses: (Witness, Witness),
) -> ResumeOutcome {
    let (ready, waiting) = tokio::sync::oneshot::channel();
    let (go, started) = mpsc::channel();
    let spawned = std::thread::Builder::new().name("codex_producer_resume".into()).spawn(move || {
        let result = run(&context, plan, witnesses, ready, started);
        if let Err(error) = result {
            retry(&context);
            tracing::warn!(tmux_session = %context.tmux, %error, "Codex normalized producer held");
        }
    });
    if spawned.is_err() {
        return ResumeOutcome::RetryLater;
    }
    match waiting.await {
        Ok(Ok(())) if go.send(()).is_ok() => ResumeOutcome::WriterReady,
        Ok(Err(error)) => ResumeOutcome::Held(error),
        _ => ResumeOutcome::RetryLater,
    }
}

fn guarded<T>(
    context: &Context,
    mut action: impl FnMut() -> Result<T, String>,
) -> Result<T, String> {
    let root = inflight::inflight_runtime_root().ok_or("inflight_root_unavailable")?;
    let path = inflight::inflight_state_path(&root, &ProviderKind::Codex, context.channel.get());
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if !context.current() {
            return Err("watcher_or_reset_changed".into());
        }
        if let Ok(_lock) = inflight::try_lock_inflight_state_path(&path) {
            let state = inflight::load_inflight_state_read_only_result(
                &ProviderKind::Codex,
                context.channel.get(),
            )?
            .ok_or("episode_absent")?;
            if !context.pin.matches_state(&state) {
                return Err("episode_changed".into());
            }
            if let Some(result) = tc::try_with_tmux_source_authority(&context.tmux, |authority| {
                if !context.authority(authority) || !context.current() {
                    return Err("source_or_owner_changed".into());
                }
                let generation = context
                    .reservation
                    .0
                    .lock()
                    .unwrap_or_else(|p| p.into_inner());
                if *generation != context.reservation.1 {
                    return Err("producer_superseded".into());
                }
                let _reset = context
                    .shared
                    .acquire_relay_frontier_mutation_for_incarnation(context.channel, context.reset)
                    .ok_or("relay_reset_changed")?;
                action()
            }) {
                return result;
            }
        }
        if Instant::now() >= deadline {
            return Err("episode_or_authority_busy".into());
        }
        // Keep this exact event pending until the lock is available; never skip on contention.
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn run(
    context: &Context,
    plan: proof::Proof,
    (raw_witness, mut wrapper_witness): (Witness, Witness),
    ready: tokio::sync::oneshot::Sender<Result<(), String>>,
    started: mpsc::Receiver<()>,
) -> Result<(), String> {
    let proof::Proof {
        raw,
        mut wrapper,
        own_turn,
        origin,
        replay,
    } = plan;
    let verify_raw = raw.try_clone().map_err(|e| e.to_string())?;
    let raw_witness = Arc::new(Mutex::new(raw_witness));
    let checked = guarded(context, || {
        raw_witness
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .verify(&verify_raw, true, true)
            .map_err(|e| e.to_string())?;
        wrapper_witness
            .verify(&wrapper, false, true)
            .map_err(|e| e.to_string())
    });
    if let Err(error) = checked {
        let _ = ready.send(Err(error.clone()));
        return Err(error);
    }
    let cancel = Arc::new(CancelToken::new());
    let done = Arc::new(AtomicBool::new(false));
    let bridge = super::super::rebind_runtime::codex_spawn::cancel_bridge(
        context.cancel.clone(),
        cancel.clone(),
        done.clone(),
    )
    .map_err(|e| e.to_string())?;
    let (sender, receiver) = mpsc::channel();
    let path = context.raw_path.clone();
    let session = context.session_id.clone();
    let tail_cancel = cancel.clone();
    let (reader_ready, reader_wait) = mpsc::channel();
    let observed = raw_witness.clone();
    let source = crate::services::codex_tui::rollout_tail::FixedRecoverySource {
        file: raw,
        ready: Some(reader_ready),
        observe: Some(Box::new(move |at, bytes| {
            observed
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .accepted(at, bytes)
                .map_err(|e| e.to_string())
        })),
    };
    let tail = std::thread::Builder::new()
        .name("codex_fixed_rollout_tail".into())
        .spawn(move || {
            crate::services::codex_tui::rollout_tail::tail_fixed_recovery_rollout(
                source,
                Path::new(&path),
                origin,
                Some(session),
                own_turn,
                sender,
                Some(tail_cancel),
            )
        });
    let result = match tail {
        Ok(tail) => {
            let result = if reader_wait.recv().is_ok()
                && ready.send(Ok(())).is_ok()
                && started.recv().is_ok()
            {
                super::super::rebind_runtime::normalized_writer::write_stream(
                    receiver,
                    String::new(),
                    replay,
                    || Ok(()),
                    |message, replay| {
                        let Some(json) =
                            super::super::rebind_runtime::codex_rebind_stream_message_json(message)
                        else {
                            return Ok(false);
                        };
                        if let Some(expected) = replay.front() {
                            if expected != &json {
                                return Err("strict_prefix_mismatch".into());
                            }
                            replay.pop_front();
                            return Ok(false);
                        }
                        let mut bytes = serde_json::to_vec(&json).map_err(|e| e.to_string())?;
                        bytes.push(b'\n');
                        guarded(context, || {
                            raw_witness
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .verify(&verify_raw, true, false)
                                .map_err(|e| e.to_string())?;
                            wrapper_witness
                                .verify(&wrapper, false, false)
                                .map_err(|e| e.to_string())?;
                            if json["type"] == "result" {
                                super::custody::before_terminal(context);
                            }
                            write_line(&mut wrapper, &bytes)?;
                            wrapper_witness
                                .appended(&wrapper, &bytes)
                                .map_err(|e| e.to_string())?;
                            Ok(true)
                        })
                    },
                )
            } else {
                Err("resume_caller_cancelled".into())
            };
            cancel.cancelled.store(true, Ordering::Release);
            let tail_result = tail.join().map_err(|_| "fixed_tail_panicked".to_string());
            result.and_then(|()| tail_result?.map(|_| ()))
        }
        Err(error) => {
            let _ = ready.send(Err(error.to_string()));
            Err(error.to_string())
        }
    };
    done.store(true, Ordering::Release);
    let _ = bridge.join();
    result
}
fn write_line(output: &mut impl Write, bytes: &[u8]) -> Result<(), String> {
    output
        .write_all(bytes)
        .and_then(|()| output.flush())
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[derive(Default)]
    struct CountWrites {
        bytes: Vec<u8>,
        writes: usize,
    }
    impl Write for CountWrites {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.writes += 1;
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    #[test]
    fn normalized_line_and_newline_are_one_write() {
        let mut output = CountWrites::default();
        let mut bytes =
            serde_json::to_vec(&serde_json::json!({"type":"assistant","text":"한글"})).unwrap();
        bytes.push(b'\n');
        write_line(&mut output, &bytes).unwrap();
        assert_eq!(output.bytes, bytes);
        assert_eq!(output.writes, 1);
    }
}
