//! A refused run has one boundary observer independent of its stoppable writer host.
use super::*;
use crate::services::tui_o::channel_policy::LegacyRun;
use crate::services::tui_o::shadow::capture::{FileMatch, file_identity, file_match};
use crate::services::tui_o::writer::adoption::LegacyView;
use crate::services::tui_o::writer::rotation::bound_source;
use std::sync::Arc;

fn current(events: &[BindingEvent]) -> Option<&BindingEvent> {
    let (source, tmux) = adoption::current(events)?;
    events
        .iter()
        .rev()
        .find(|e| e.tmux_session == tmux && bound_source(e) == Some(&source))
}
/// Only this source-bound rotation refusal grants the Legacy exception.
pub(in crate::services::tui_o::writer) fn normalize<I: HostIo>(
    waiting: &Waiting<'_, I>,
    events: &[BindingEvent],
) -> bool {
    if !waiting.late {
        return false;
    }
    let Some(binding) = current(events) else {
        return false;
    };
    if binding.channel_id != waiting.channel || binding.provider != waiting.provider {
        return false;
    }
    let Some(source) = bound_source(binding) else {
        return false;
    };
    let Ok(meta) = std::fs::metadata(&source.path) else {
        return false;
    };
    if file_match(source, file_identity(&meta)) == FileMatch::Other {
        return false;
    }
    let Ok(epoch) = waiting.legacy.late_epoch(waiting.channel) else {
        return false;
    };
    let run = LegacyRun {
        binding: binding.clone(),
        turn: epoch.turn,
        delivery: epoch.delivery,
        observed_end: meta.len(),
    };
    if !waiting
        .candidate
        .release_child_run(waiting.channel, run.clone())
    {
        return false;
    }
    let (candidate, legacy, log) = (
        waiting.candidate.clone(),
        waiting.legacy.clone(),
        waiting.io.bindings(waiting.channel, waiting.provider),
    );
    // The observer only reopens adoption. It survives host stop, and exits when this exact
    // run changes; all publication still goes through the writer's ordinary fences.
    tokio::spawn(observe(candidate, legacy, log, run));
    true
}

pub(super) async fn parked(candidate: &Candidate) -> bool {
    let mut changed = candidate.child_boundary_notice();
    if candidate.legacy_run().is_none() {
        return candidate.child_boundary_epoch() != 0 && candidate.peek() == Adoption::Deferred;
    }
    while candidate.peek() == Adoption::Released {
        if changed.changed().await.is_err() {
            return false;
        }
    }
    candidate.peek() == Adoption::Deferred
}
async fn observe<B: BindingEvents>(
    candidate: Candidate,
    legacy: Arc<dyn LegacyView>,
    log: Arc<B>,
    run: LegacyRun,
) {
    let channel = run.binding.channel_id;
    let Some(source) = bound_source(&run.binding).cloned() else {
        return;
    };
    let mut observed_source = source.clone();
    let mut observed_end = run.observed_end;
    let mut observed_turn = run.turn.clone();
    let mut wake = Wake::new(&*log, channel);
    loop {
        if !candidate.child_parked() || candidate.legacy_run().as_ref() != Some(&run) {
            return;
        }
        let sends = {
            let _state = candidate.lock();
            candidate.sends()
        };
        let events = log
            .binding_events_since(channel, 0)
            .map(crate::services::tui_o::writer::renumbered::first_named);
        if let Ok(events) = events {
            let now = current(&events);
            if now.is_some_and(|now| {
                now.channel_id != channel || now.provider != run.binding.provider
            }) {
                return;
            }
            if let Ok(epoch) = legacy.late_epoch(channel) {
                let bind = now.is_some_and(|now| {
                    now.seq > run.binding.seq
                        && ((!now.execution_nonce.is_empty()
                            && now.execution_nonce != run.binding.execution_nonce)
                            || now.tmux_session != run.binding.tmux_session)
                });
                let turn = observed_turn.is_some() && epoch.turn != observed_turn;
                let session = legacy
                    .session_ended(&run.binding.tmux_session)
                    .unwrap_or(false);
                let same = now.is_some_and(|now| {
                    now.tmux_session == run.binding.tmux_session
                        && (bound_source(now) == Some(&source)
                            || (!run.binding.execution_nonce.is_empty()
                                && now.execution_nonce == run.binding.execution_nonce)
                            || (observed_turn.is_some() && epoch.turn == observed_turn))
                });
                if same && !bind && !turn {
                    // Compaction keeps the execution owner and fixes the checked source floor.
                    if let Some(next) = now
                        .and_then(bound_source)
                        .filter(|next| **next != observed_source)
                    {
                        if let Ok(meta) = std::fs::metadata(&next.path) {
                            if file_match(next, file_identity(&meta)) != FileMatch::Other {
                                observed_source = next.clone();
                                observed_end = meta.len();
                            }
                        }
                    }
                    if observed_turn.is_none() {
                        observed_turn = epoch.turn.clone();
                    }
                }
                let mut ended = false;
                if same && sends.0 == sends.1 && !legacy.tail_running(&run.binding.tmux_session) {
                    let (view, read) = (legacy.clone(), events.clone());
                    if let Ok(Ok(pin)) = tokio::task::spawn_blocking(move || {
                        adoption::pin_late(&*view, &read, channel)
                    })
                    .await
                    {
                        ended = pin.delivered_to_start()
                            && pin.start() != 0
                            && pin.start() >= observed_end;
                    }
                }
                if bind || turn || session || ended {
                    let floor = events.last().map_or(0, |e| e.seq);
                    if candidate.retry_child_at_boundary(&run, floor) {
                        let reason = if bind {
                            "execution_bind"
                        } else if session {
                            "session_end"
                        } else {
                            "turn_end"
                        };
                        tracing::info!(
                            channel,
                            reason,
                            floor,
                            "[tui_o] child run boundary retries adoption"
                        );
                        return;
                    }
                }
            }
        }
        wake.wait().await;
    }
}
