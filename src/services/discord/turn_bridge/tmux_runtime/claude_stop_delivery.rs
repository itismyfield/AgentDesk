//! Exactly-once and phase-safe Claude turn interrupt policy.

use super::interrupt_policy::{
    ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON, ClaudeTurnInterruptDelivery,
    ProviderTurnInterruptOutcome, build_claude_interrupt_control_line,
    claude_turn_interrupt_delivery,
};
use super::process_table::{pane_foreground_is_provider_wrapper, write_line_to_wrapper_fifo};
use super::tmux_runtime_paths;
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::tui_turn_state::TuiTurnState;
use std::io::{BufRead, Seek};
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ClaudeStopTurnIdentity {
    output_path: String,
    file_identity: (u64, u64),
    user_line_start: u64,
    user_line_end: u64,
    user_line_hash: u64,
    user_entry_id: Option<String>,
}

impl ClaudeStopTurnIdentity {
    fn capture(output_path: &str) -> Option<Self> {
        latest_claude_user_turn_identity(Path::new(output_path))
    }

    fn still_current(&self) -> bool {
        latest_claude_user_turn_identity(Path::new(&self.output_path)).as_ref() == Some(self)
    }
}

#[cfg(unix)]
fn transcript_file_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (metadata.dev(), metadata.ino())
}

#[cfg(not(unix))]
fn transcript_file_identity(metadata: &std::fs::Metadata) -> (u64, u64) {
    let created_ns = metadata
        .created()
        .ok()
        .and_then(|created| created.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0);
    (created_ns, 0)
}

fn latest_claude_user_turn_identity(path: &Path) -> Option<ClaudeStopTurnIdentity> {
    use std::hash::{Hash, Hasher};

    const MAX_TAIL_BYTES: u64 = 256 * 1024;

    let mut file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    let len = metadata.len();
    let file_identity = transcript_file_identity(&metadata);
    let start = len.saturating_sub(MAX_TAIL_BYTES);
    file.seek(std::io::SeekFrom::Start(start)).ok()?;
    let mut reader = std::io::BufReader::new(file);
    let mut line = String::new();
    let mut offset = start;
    if start > 0 {
        offset = offset.saturating_add(reader.read_line(&mut line).ok()? as u64);
    }

    let mut latest = None;
    loop {
        line.clear();
        let line_start = offset;
        let bytes_read = reader.read_line(&mut line).ok()?;
        if bytes_read == 0 {
            return latest;
        }
        offset = offset.saturating_add(bytes_read as u64);
        if !line.ends_with('\n') {
            return latest;
        }
        let Ok(json) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
            continue;
        };
        let Some((_prompt, user_entry_id)) =
            crate::services::tui_prompt_dedupe::extract_claude_transcript_user_prompt_with_entry_id(
                &json,
            )
        else {
            continue;
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        line.as_bytes().hash(&mut hasher);
        latest = Some(ClaudeStopTurnIdentity {
            output_path: path.display().to_string(),
            file_identity,
            user_line_start: line_start,
            user_line_end: offset,
            user_line_hash: hasher.finish(),
            user_entry_id,
        });
    }
}

fn transcript_identity_allows_delivery(identity: Option<&ClaudeStopTurnIdentity>) -> bool {
    identity.is_none_or(ClaudeStopTurnIdentity::still_current)
}

fn stream_json_interrupt_phase(
    structured_state: Option<TuiTurnState>,
    submit_pending: bool,
) -> ClaudeTuiInterruptPhase {
    match structured_state {
        Some(TuiTurnState::Idle) if submit_pending => ClaudeTuiInterruptPhase::UserSubmitted,
        Some(TuiTurnState::Idle) => ClaudeTuiInterruptPhase::PromptReady,
        Some(TuiTurnState::UserSubmitted) => ClaudeTuiInterruptPhase::UserSubmitted,
        Some(TuiTurnState::Streaming) => ClaudeTuiInterruptPhase::ActiveGeneration,
        Some(TuiTurnState::Unknown) | None => ClaudeTuiInterruptPhase::Ambiguous,
    }
}

fn deliver_claimed_claude_stop<R, Write>(
    token: &CancelToken,
    tmux_session_name: &str,
    transcript_identity: Option<&ClaudeStopTurnIdentity>,
    write: Write,
) -> Result<R, String>
where
    Write: FnOnce() -> Result<R, String>,
{
    let Some(delivery_guard) = token.lock_current_claude_interrupt_session(tmux_session_name)
    else {
        return Err("stale Claude stop session generation before provider write".to_string());
    };
    if !transcript_identity_allows_delivery(transcript_identity) {
        return Err("stale Claude stop transcript identity before provider write".to_string());
    }
    delivery_guard.commit_success(write())
}

/// F1: run the interactive-TUI Escape delivery under the SAME per-pane composer
/// mutation lock `/compact` steering (and every normal composer mutation) holds,
/// so a user stop-Escape and a busy-pane auto `/compact` can never interleave —
/// an Escape landing between the `/compact` literal and its Enter would swallow
/// the user's stop, or the auto-Enter would falsely confirm a `/compact` the
/// operator meant to cancel.
///
/// Lock order (P2 #4616): the composer lock is taken OUTSIDE (before) the global
/// interrupt-registry delivery guard — `deliver_claimed_claude_stop` acquires the
/// registry guard *inside* the closure passed here. The acquisition order is
/// therefore composer-lock → interrupt-registry-lock. This is the deliberate
/// reversal of the earlier registry-first order: parking on the per-pane composer
/// lock (up to `SELECTOR_OPEN_TIMEOUT` + confirm, ~seconds) while holding the
/// GLOBAL registry lock froze every pane's turn-start bind and every other stop
/// delivery. Composer-first keeps the global registry lock held only for the
/// brief generation check + tmux send.
///
/// No cycle: the interrupt-registry lock is a sink in the lock graph — apart from
/// this path only `bind_claude_tmux_session` takes it, and bind acquires no lock
/// that leads back to the composer or session-turn locks (it only touches the
/// token-local `tmux_session` leaf mutex). So no path runs interrupt-registry →
/// composer, and composer → interrupt-registry stays strictly one-directional.
/// Generic over the delivery so the lock routing is unit-testable without a live
/// tmux pane.
fn deliver_tui_escape_under_composer_lock(
    session_name: &str,
    run_under_composer: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
        session_name,
        run_under_composer,
    )
}

/// Route the claimed stop delivery under the correct lock order for its mechanism.
///
/// The interactive Escape holds the per-pane composer lock across the WHOLE
/// claimed delivery (`deliver_claimed_claude_stop`: session-generation check +
/// tmux send), acquiring it before the global interrupt-registry guard so the
/// registry lock is never held while parked on the composer lock (P2 #4616). The
/// wrapper FIFO control request needs no composer lock. Either way the generation
/// fence stays atomic: the generation check and the provider write both run under
/// the registry guard held inside `deliver_claimed_claude_stop`, so no newer turn
/// can publish its generation between the check and the write.
fn deliver_claimed_claude_stop_under_lock_order(
    token: &CancelToken,
    session_name: &str,
    delivery: ClaudeTurnInterruptDelivery,
    transcript_identity: Option<&ClaudeStopTurnIdentity>,
    write: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    match delivery {
        ClaudeTurnInterruptDelivery::TuiEscape => {
            deliver_tui_escape_under_composer_lock(session_name, || {
                deliver_claimed_claude_stop(token, session_name, transcript_identity, write)
            })
        }
        ClaudeTurnInterruptDelivery::StreamJsonControlRequest => {
            deliver_claimed_claude_stop(token, session_name, transcript_identity, write)
        }
    }
}

struct ClaudeStopDeliveryReservation<'a> {
    token: &'a CancelToken,
}

impl<'a> ClaudeStopDeliveryReservation<'a> {
    fn claim(token: &'a CancelToken) -> Option<Self> {
        if token.claim_claude_interrupt() {
            Some(Self { token })
        } else {
            None
        }
    }
}

impl Drop for ClaudeStopDeliveryReservation<'_> {
    fn drop(&mut self) {
        let _ = self.token.release_claude_interrupt_claim();
    }
}

/// Claude state observed at the single stop-delivery ownership boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ClaudeTuiInterruptPhase {
    PromptReady,
    UserSubmitted,
    ActiveGeneration,
    Ambiguous,
}

impl ClaudeTuiInterruptPhase {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::PromptReady => "prompt_ready",
            Self::UserSubmitted => "user_submitted",
            Self::ActiveGeneration => "active_generation",
            Self::Ambiguous => "ambiguous",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ClaudeStopDeliveryDecision {
    Deliver(ClaudeTurnInterruptDelivery),
    SkipDuplicate,
    SkipPreGeneration,
    SkipAmbiguous,
}

impl ClaudeStopDeliveryDecision {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Deliver(_) => "deliver",
            Self::SkipDuplicate => "skip_duplicate",
            Self::SkipPreGeneration => "skip_pre_generation",
            Self::SkipAmbiguous => "skip_ambiguous",
        }
    }
}

pub(super) fn classify_tui_interrupt_phase(
    structured_state: Option<TuiTurnState>,
    pane_ready: bool,
    pane_active: bool,
) -> ClaudeTuiInterruptPhase {
    match structured_state {
        Some(TuiTurnState::Idle) => ClaudeTuiInterruptPhase::PromptReady,
        Some(TuiTurnState::UserSubmitted) => ClaudeTuiInterruptPhase::UserSubmitted,
        Some(TuiTurnState::Streaming) if pane_active => ClaudeTuiInterruptPhase::ActiveGeneration,
        Some(TuiTurnState::Streaming | TuiTurnState::Unknown) => ClaudeTuiInterruptPhase::Ambiguous,
        None if pane_ready => ClaudeTuiInterruptPhase::PromptReady,
        None => ClaudeTuiInterruptPhase::Ambiguous,
    }
}

/// Decide delivery after the caller has reserved the token-local ownership fence.
pub(super) fn decide_claimed_claude_stop_delivery(
    delivery: ClaudeTurnInterruptDelivery,
    tui_phase: ClaudeTuiInterruptPhase,
) -> ClaudeStopDeliveryDecision {
    let phase_allows_delivery = match delivery {
        ClaudeTurnInterruptDelivery::TuiEscape => {
            matches!(tui_phase, ClaudeTuiInterruptPhase::ActiveGeneration)
        }
        ClaudeTurnInterruptDelivery::StreamJsonControlRequest => !matches!(
            tui_phase,
            ClaudeTuiInterruptPhase::PromptReady | ClaudeTuiInterruptPhase::Ambiguous
        ),
    };

    if phase_allows_delivery {
        ClaudeStopDeliveryDecision::Deliver(delivery)
    } else if matches!(tui_phase, ClaudeTuiInterruptPhase::Ambiguous) {
        ClaudeStopDeliveryDecision::SkipAmbiguous
    } else {
        ClaudeStopDeliveryDecision::SkipPreGeneration
    }
}

/// Cancel Claude's active turn while preserving its tmux session.
///
/// Delivery uses Escape for the interactive TUI or a stream-json interrupt
/// control request for the wrapper FIFO. A token-local CAS reserves one attempt
/// and commits only after successful provider I/O; skipped or failed attempts
/// roll back. Interactive Escape additionally requires structured streaming
/// state and positive active-pane evidence.
pub(super) async fn interrupt_claude_turn_session_preserving(
    token: &Arc<CancelToken>,
    tmux_session: Option<String>,
    reason: &str,
) -> ProviderTurnInterruptOutcome {
    // #3169: an anonymous internal PreserveSession teardown
    // (`turn_bridge_cancelled`, no user `cancel_source`) must NOT cancel the
    // live claude turn — leave it running for the watcher to reconcile, exactly
    // as the prior SIGINT-suppression did, just without the session-kill risk.
    if reason == ANONYMOUS_TURN_BRIDGE_TEARDOWN_REASON {
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: false,
            sigint_target_missing: false,
        };
    }

    let Some(session_name) = tmux_session.clone() else {
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: true,
            sigint_target_missing: false,
        };
    };

    let expected_generation = token.claude_interrupt_generation();
    let Some(mut reservation) = ClaudeStopDeliveryReservation::claim(token) else {
        tracing::info!(
            "claude turn interrupt decision: provider=claude session={} generation={} reason={} mechanism=not_probed runtime_kind=not_probed structured_state=not_probed pane_ready=not_probed pane_active=not_probed pane_has_draft=not_probed phase=not_probed decision={}",
            session_name,
            expected_generation,
            reason,
            ClaudeStopDeliveryDecision::SkipDuplicate.as_str()
        );
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: false,
            sigint_target_missing: false,
        };
    };

    let session_for_probe = session_name.clone();
    let token_for_probe = Arc::clone(token);
    let probe_result = tokio::task::spawn_blocking(move || {
        let is_wrapper = pane_foreground_is_provider_wrapper(&session_for_probe);
        let delivery = claude_turn_interrupt_delivery(is_wrapper);
        let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session(
            &session_for_probe,
        );
        let runtime_kind = binding
            .as_ref()
            .map(|binding| binding.runtime_kind)
            .or_else(|| {
                crate::services::tmux_common::resolve_tmux_runtime_kind_marker(&session_for_probe)
            });
        let (default_output_path, default_input_fifo_path) = tmux_runtime_paths(&session_for_probe);
        let wrapper_input_fifo_path = if matches!(
            delivery,
            ClaudeTurnInterruptDelivery::StreamJsonControlRequest
        ) {
            Some(
                binding
                    .as_ref()
                    .and_then(|binding| binding.input_fifo_path.clone())
                    .unwrap_or(default_input_fifo_path),
            )
        } else {
            None
        };
        let output_path = binding
            .as_ref()
            .map(|binding| binding.output_path.clone())
            .unwrap_or(default_output_path);
        let structured_state = if matches!(delivery, ClaudeTurnInterruptDelivery::TuiEscape) {
            binding.as_ref().and_then(|binding| {
                crate::services::tui_turn_state::runtime_binding_turn_state(
                    &ProviderKind::Claude,
                    binding,
                )
            })
        } else {
            Some(
                crate::services::tui_turn_state::observe_claude_jsonl_turn_state(
                    std::path::Path::new(&output_path),
                ),
            )
        };
        let turn_identity = ClaudeStopTurnIdentity::capture(&output_path);
        let pane = if matches!(delivery, ClaudeTurnInterruptDelivery::TuiEscape) {
            crate::services::platform::tmux::capture_pane(&session_for_probe, -160)
        } else {
            None
        };
        let pane_ready = pane.as_deref().is_some_and(
            crate::services::tmux_common::tmux_capture_indicates_claude_tui_ready_for_input,
        );
        let pane_active = pane.as_deref().is_some_and(
            crate::services::tmux_common::tmux_capture_indicates_claude_tui_actively_streaming,
        );
        let pane_has_draft = pane.as_deref().is_some_and(
            crate::services::tmux_common::tmux_capture_indicates_claude_tui_prompt_draft,
        );
        let phase = match delivery {
            ClaudeTurnInterruptDelivery::TuiEscape => classify_tui_interrupt_phase(
                structured_state,
                pane_ready || pane_has_draft,
                pane_active,
            ),
            ClaudeTurnInterruptDelivery::StreamJsonControlRequest => stream_json_interrupt_phase(
                structured_state,
                token_for_probe.claude_interrupt_submit_pending(),
            ),
        };
        (
            delivery,
            runtime_kind,
            wrapper_input_fifo_path,
            turn_identity,
            structured_state,
            pane_ready,
            pane_active,
            pane_has_draft,
            phase,
        )
    })
    .await;

    let (
        delivery,
        runtime_kind,
        wrapper_input_fifo_path,
        turn_identity,
        structured_state,
        pane_ready,
        pane_active,
        pane_has_draft,
        phase,
    ) = match probe_result {
        Ok(probe) => probe,
        Err(error) => {
            tracing::warn!(
                "claude turn interrupt probe join error: session={} reason={} generation={} decision=skip_ambiguous error={}",
                session_name,
                reason,
                expected_generation,
                error
            );
            return ProviderTurnInterruptOutcome {
                tmux_session,
                sent_keys: false,
                fallback_sigint_pid: None,
                missing_tmux_session: false,
                sigint_target_missing: false,
            };
        }
    };

    let decision = decide_claimed_claude_stop_delivery(delivery, phase);
    tracing::info!(
        "claude turn interrupt decision: provider=claude session={} generation={} reason={} mechanism={:?} runtime_kind={} structured_state={} pane_ready={} pane_active={} pane_has_draft={} phase={} decision={}",
        session_name,
        expected_generation,
        reason,
        delivery,
        runtime_kind
            .map(crate::services::agent_protocol::RuntimeHandoffKind::as_str)
            .unwrap_or("unknown"),
        structured_state
            .map(|state| state.as_str())
            .unwrap_or("unavailable"),
        pane_ready,
        pane_active,
        pane_has_draft,
        phase.as_str(),
        decision.as_str()
    );

    let ClaudeStopDeliveryDecision::Deliver(delivery) = decision else {
        return ProviderTurnInterruptOutcome {
            tmux_session,
            sent_keys: false,
            fallback_sigint_pid: None,
            missing_tmux_session: false,
            sigint_target_missing: false,
        };
    };

    // The session-level generation check and provider write run under one lock in
    // `lock_current_claude_interrupt_session`; a newer turn cannot publish itself
    // between the check and the Escape/FIFO write. Transcript identity is
    // supplemental only: when the turn-start entry has fallen outside the bounded
    // tail window, fail open after the authoritative session-generation check.
    //
    // P2 #4616: `deliver_claimed_claude_stop_under_lock_order` takes the per-pane
    // composer lock OUTSIDE this registry guard for the interactive Escape, so we
    // never hold the GLOBAL interrupt-registry lock while parked on the composer
    // lock (which can wait up to `SELECTOR_OPEN_TIMEOUT` + confirm). The `write`
    // closure below is provider I/O only — it acquires no composer lock.
    let session_for_task = session_name.clone();
    let token_for_task = Arc::clone(token);
    let request_id = format!("agentdesk-interrupt-{}", uuid::Uuid::new_v4());
    let delivery_result = tokio::task::spawn_blocking(move || {
        deliver_claimed_claude_stop_under_lock_order(
            token_for_task.as_ref(),
            &session_for_task,
            delivery,
            turn_identity.as_ref(),
            || match delivery {
                ClaudeTurnInterruptDelivery::TuiEscape => {
                    match crate::services::platform::tmux::send_keys(&session_for_task, &["Escape"])
                    {
                        Ok(output) if output.status.success() => Ok(()),
                        Ok(output) => Err(format!(
                            "tmux send-keys Escape failed: status={}",
                            output.status
                        )),
                        Err(error) => Err(format!("tmux send-keys Escape error: {error}")),
                    }
                }
                ClaudeTurnInterruptDelivery::StreamJsonControlRequest => {
                    let Some(input_fifo) = wrapper_input_fifo_path else {
                        return Err("claude wrapper input FIFO unavailable after probe".to_string());
                    };
                    let line = build_claude_interrupt_control_line(&request_id);
                    write_line_to_wrapper_fifo(&input_fifo, &line)
                }
            },
        )
        .map_err(|error| format!("{error}: expected_generation={expected_generation}"))
    })
    .await;

    let delivered = match delivery_result {
        Ok(Ok(())) => {
            tracing::info!(
                "claude turn interrupt delivered (session preserved): session={} generation={} reason={} mechanism={:?} phase={}",
                session_name,
                expected_generation,
                reason,
                delivery,
                phase.as_str()
            );
            true
        }
        Ok(Err(error)) => {
            // Deliberately NO SIGINT fallback: a failed turn-cancel must not
            // escalate to a session-kill. The cooperative cancel flag still
            // flips in `cancel_active_token`, and the watcher reconciles the
            // turn on its next pass.
            tracing::warn!(
                "claude turn interrupt delivery failed (session left intact, no SIGINT escalation): session={} generation={} reason={} mechanism={:?} phase={} error={}",
                session_name,
                expected_generation,
                reason,
                delivery,
                phase.as_str(),
                error
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                "claude turn interrupt join error: session={} generation={} reason={} mechanism={:?} phase={} error={}",
                session_name,
                expected_generation,
                reason,
                delivery,
                phase.as_str(),
                error
            );
            false
        }
    };

    ProviderTurnInterruptOutcome {
        tmux_session,
        sent_keys: delivered,
        fallback_sigint_pid: None,
        missing_tmux_session: false,
        sigint_target_missing: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn reserved_decision(
        token: &CancelToken,
        delivery: ClaudeTurnInterruptDelivery,
        phase: ClaudeTuiInterruptPhase,
        delivery_succeeded: bool,
    ) -> ClaudeStopDeliveryDecision {
        let Some(reservation) = ClaudeStopDeliveryReservation::claim(token) else {
            return ClaudeStopDeliveryDecision::SkipDuplicate;
        };
        let decision = decide_claimed_claude_stop_delivery(delivery, phase);
        if delivery_succeeded && matches!(decision, ClaudeStopDeliveryDecision::Deliver(_)) {
            let session = format!(
                "claude-stop-policy-test-{}",
                token.claude_interrupt_generation()
            );
            token.bind_claude_tmux_session(&session);
            token
                .lock_current_claude_interrupt_session(&session)
                .expect("test generation must acquire delivery guard")
                .commit_success(Ok::<(), ()>(()))
                .expect("test delivery must commit");
        }
        drop(reservation);
        decision
    }

    fn decision_mutates_composer(decision: ClaudeStopDeliveryDecision) -> bool {
        matches!(
            decision,
            ClaudeStopDeliveryDecision::Deliver(ClaudeTurnInterruptDelivery::TuiEscape)
        )
    }

    #[test]
    fn prompt_ready_and_just_injected_never_escape_or_mutate_composer() {
        for phase in [
            classify_tui_interrupt_phase(Some(TuiTurnState::Idle), true, false),
            classify_tui_interrupt_phase(Some(TuiTurnState::UserSubmitted), false, false),
        ] {
            let token = CancelToken::new();
            let decision =
                reserved_decision(&token, ClaudeTurnInterruptDelivery::TuiEscape, phase, false);
            assert!(matches!(
                phase,
                ClaudeTuiInterruptPhase::PromptReady | ClaudeTuiInterruptPhase::UserSubmitted
            ));
            assert_eq!(decision, ClaudeStopDeliveryDecision::SkipPreGeneration);
            assert!(!decision_mutates_composer(decision));
            assert_eq!(
                reserved_decision(
                    &token,
                    ClaudeTurnInterruptDelivery::TuiEscape,
                    ClaudeTuiInterruptPhase::ActiveGeneration,
                    true,
                ),
                ClaudeStopDeliveryDecision::Deliver(ClaudeTurnInterruptDelivery::TuiEscape),
                "a pre-generation skip must roll back so the active-generation retry can deliver"
            );
            assert_eq!(
                reserved_decision(
                    &token,
                    ClaudeTurnInterruptDelivery::TuiEscape,
                    ClaudeTuiInterruptPhase::ActiveGeneration,
                    true,
                ),
                ClaudeStopDeliveryDecision::SkipDuplicate,
                "only a successful delivery commits the fence"
            );
        }
    }

    #[test]
    fn active_generation_escape_is_delivered_once_across_stop_race() {
        const RACERS: usize = 8;

        let token = Arc::new(CancelToken::new());
        let session = format!(
            "claude-stop-race-test-{}",
            token.claude_interrupt_generation()
        );
        token.bind_claude_tmux_session(&session);
        let writes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(RACERS + 1));
        let release_winner = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();

        for _ in 0..RACERS {
            let token = Arc::clone(&token);
            let session = session.clone();
            let writes = Arc::clone(&writes);
            let barrier = Arc::clone(&barrier);
            let release_winner = Arc::clone(&release_winner);
            handles.push(thread::spawn(move || {
                barrier.wait();
                let Some(reservation) = ClaudeStopDeliveryReservation::claim(token.as_ref()) else {
                    return Ok::<_, String>(ClaudeStopDeliveryDecision::SkipDuplicate);
                };
                let decision = decide_claimed_claude_stop_delivery(
                    ClaudeTurnInterruptDelivery::TuiEscape,
                    ClaudeTuiInterruptPhase::ActiveGeneration,
                );
                if matches!(decision, ClaudeStopDeliveryDecision::Deliver(_)) {
                    deliver_claimed_claude_stop(token.as_ref(), &session, None, || {
                        writes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        release_winner.wait();
                        Ok::<(), String>(())
                    })?;
                }
                drop(reservation);
                Ok(decision)
            }));
        }
        barrier.wait();
        while writes.load(std::sync::atomic::Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        release_winner.wait();

        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("stop racer must not panic"))
            .collect();
        assert!(
            results.iter().all(Result::is_ok),
            "race losers exit cleanly"
        );
        assert_eq!(
            writes.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the claim guard must permit exactly one observable provider write"
        );
    }

    #[test]
    fn a_new_turn_generation_gets_a_fresh_delivery_fence() {
        let first = CancelToken::new();
        let next = CancelToken::new();
        assert_ne!(
            first.claude_interrupt_generation(),
            next.claude_interrupt_generation()
        );

        for token in [&first, &next] {
            assert!(matches!(
                reserved_decision(
                    token,
                    ClaudeTurnInterruptDelivery::TuiEscape,
                    ClaudeTuiInterruptPhase::ActiveGeneration,
                    true,
                ),
                ClaudeStopDeliveryDecision::Deliver(_)
            ));
        }
    }

    #[test]
    fn ambiguous_or_unconfirmed_streaming_state_fails_safe_and_allows_retry() {
        for phase in [
            classify_tui_interrupt_phase(Some(TuiTurnState::Streaming), false, false),
            classify_tui_interrupt_phase(Some(TuiTurnState::Unknown), false, true),
            classify_tui_interrupt_phase(None, false, true),
        ] {
            let token = CancelToken::new();
            assert_eq!(phase, ClaudeTuiInterruptPhase::Ambiguous);
            assert_eq!(
                reserved_decision(&token, ClaudeTurnInterruptDelivery::TuiEscape, phase, false,),
                ClaudeStopDeliveryDecision::SkipAmbiguous
            );
            assert_eq!(
                reserved_decision(
                    &token,
                    ClaudeTurnInterruptDelivery::TuiEscape,
                    ClaudeTuiInterruptPhase::ActiveGeneration,
                    true,
                ),
                ClaudeStopDeliveryDecision::Deliver(ClaudeTurnInterruptDelivery::TuiEscape),
                "an ambiguous skip must roll back so a confirmed retry can deliver"
            );
        }
    }

    #[test]
    fn wrapper_submit_pending_overrides_prior_idle_state() {
        let token = CancelToken::new();
        assert_eq!(
            decide_claimed_claude_stop_delivery(
                ClaudeTurnInterruptDelivery::StreamJsonControlRequest,
                ClaudeTuiInterruptPhase::PromptReady,
            ),
            ClaudeStopDeliveryDecision::SkipPreGeneration
        );

        token.mark_claude_interrupt_submit_pending();
        let phase = stream_json_interrupt_phase(
            Some(TuiTurnState::Idle),
            token.claude_interrupt_submit_pending(),
        );
        assert_eq!(
            decide_claimed_claude_stop_delivery(
                ClaudeTurnInterruptDelivery::StreamJsonControlRequest,
                phase,
            ),
            ClaudeStopDeliveryDecision::Deliver(
                ClaudeTurnInterruptDelivery::StreamJsonControlRequest
            )
        );
    }

    #[test]
    fn production_delivery_boundary_holds_generation_lock_through_write() {
        let session = "claude-stop-production-boundary";
        let stale = Arc::new(CancelToken::new());
        let current = Arc::new(CancelToken::new());
        stale.bind_claude_tmux_session(session);
        assert!(stale.claim_claude_interrupt());

        let entered_write = Arc::new(Barrier::new(2));
        let release_write = Arc::new(Barrier::new(2));
        let stale_for_write = Arc::clone(&stale);
        let entered_for_write = Arc::clone(&entered_write);
        let release_for_write = Arc::clone(&release_write);
        let writer = thread::spawn(move || {
            deliver_claimed_claude_stop(stale_for_write.as_ref(), session, None, || {
                entered_for_write.wait();
                release_for_write.wait();
                Ok::<(), String>(())
            })
        });
        entered_write.wait();

        let (bind_started_tx, bind_started_rx) = std::sync::mpsc::channel();
        let (bind_finished_tx, bind_finished_rx) = std::sync::mpsc::channel();
        let current_for_bind = Arc::clone(&current);
        let binder = thread::spawn(move || {
            bind_started_tx.send(()).unwrap();
            current_for_bind.bind_claude_tmux_session(session);
            bind_finished_tx.send(()).unwrap();
        });
        bind_started_rx.recv().unwrap();
        assert!(
            bind_finished_rx
                .recv_timeout(std::time::Duration::from_millis(20))
                .is_err(),
            "generation publish must wait for provider write"
        );

        release_write.wait();
        bind_finished_rx.recv().unwrap();
        writer.join().expect("writer must not panic").unwrap();
        binder.join().expect("binder must not panic");
        assert!(
            stale
                .lock_current_claude_interrupt_session(session)
                .is_none()
        );
    }

    #[test]
    fn failed_delivery_rolls_back_but_successful_delivery_commits() {
        let token = CancelToken::new();
        assert_eq!(
            reserved_decision(
                &token,
                ClaudeTurnInterruptDelivery::TuiEscape,
                ClaudeTuiInterruptPhase::ActiveGeneration,
                false,
            ),
            ClaudeStopDeliveryDecision::Deliver(ClaudeTurnInterruptDelivery::TuiEscape)
        );
        assert_eq!(
            reserved_decision(
                &token,
                ClaudeTurnInterruptDelivery::TuiEscape,
                ClaudeTuiInterruptPhase::ActiveGeneration,
                true,
            ),
            ClaudeStopDeliveryDecision::Deliver(ClaudeTurnInterruptDelivery::TuiEscape),
            "a failed provider write must leave the same turn retryable"
        );
        assert_eq!(
            reserved_decision(
                &token,
                ClaudeTurnInterruptDelivery::TuiEscape,
                ClaudeTuiInterruptPhase::ActiveGeneration,
                true,
            ),
            ClaudeStopDeliveryDecision::SkipDuplicate,
            "the first successful provider write permanently commits the fence"
        );
    }

    fn write_turn(path: &Path, entry_id: Option<&str>, text: &str) {
        let mut user = serde_json::json!({
            "type": "user",
            "message": { "role": "user", "content": text }
        });
        if let Some(entry_id) = entry_id {
            user["uuid"] = serde_json::Value::String(entry_id.to_string());
        }
        let assistant = serde_json::json!({
            "type": "assistant",
            "message": { "content": [] }
        });
        std::fs::write(path, format!("{user}\n{assistant}\n"))
            .expect("write Claude transcript fixture");
    }

    #[test]
    fn missing_turn_identity_fails_open_after_session_generation_check() {
        assert!(
            transcript_identity_allows_delivery(None),
            "a long-running turn whose user entry fell outside the tail window must still reach Escape"
        );
    }

    #[test]
    fn tool_result_user_envelope_does_not_advance_turn_identity() {
        let temp = tempfile::tempdir().expect("temp transcript root");
        let path = temp.path().join("turn.jsonl");
        write_turn(&path, Some("turn-a"), "first");
        let identity =
            ClaudeStopTurnIdentity::capture(path.to_str().unwrap()).expect("capture turn identity");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .and_then(|mut file| {
                use std::io::Write;
                writeln!(
                    file,
                    "{}",
                    serde_json::json!({
                        "type": "user",
                        "message": {
                            "role": "user",
                            "content": [{ "type": "tool_result", "content": "done" }]
                        },
                        "uuid": "tool-result-a"
                    })
                )
            })
            .expect("append tool_result fixture");

        assert!(
            identity.still_current(),
            "tool_result activity inside one turn must not make a valid stop stale"
        );
    }

    #[test]
    fn turn_identity_rejects_newer_user_entry_before_delivery() {
        let temp = tempfile::tempdir().expect("temp transcript root");
        let path = temp.path().join("turn.jsonl");
        write_turn(&path, Some("turn-a"), "first");
        let identity = ClaudeStopTurnIdentity::capture(path.to_str().unwrap())
            .expect("capture turn A identity");
        assert!(identity.still_current());

        write_turn(&path, Some("turn-b"), "second");
        assert!(
            !identity.still_current(),
            "a stale stop must not pass the write-adjacent fence after turn B starts"
        );
    }

    #[test]
    fn turn_identity_without_uuid_still_detects_replaced_turn() {
        let temp = tempfile::tempdir().expect("temp transcript root");
        let path = temp.path().join("turn.jsonl");
        write_turn(&path, None, "first");
        let identity = ClaudeStopTurnIdentity::capture(path.to_str().unwrap())
            .expect("capture identity without uuid");
        assert!(identity.still_current());

        write_turn(&path, None, "second");
        assert!(!identity.still_current());
    }

    #[test]
    fn wrapper_interrupt_is_fenced_but_can_cancel_submitted_generation() {
        let token = CancelToken::new();
        assert!(matches!(
            reserved_decision(
                &token,
                ClaudeTurnInterruptDelivery::StreamJsonControlRequest,
                ClaudeTuiInterruptPhase::UserSubmitted,
                true,
            ),
            ClaudeStopDeliveryDecision::Deliver(_)
        ));
        assert_eq!(
            reserved_decision(
                &token,
                ClaudeTurnInterruptDelivery::StreamJsonControlRequest,
                ClaudeTuiInterruptPhase::UserSubmitted,
                true,
            ),
            ClaudeStopDeliveryDecision::SkipDuplicate
        );
    }

    /// F1 mutation guard: `deliver_tui_escape_under_composer_lock` must acquire
    /// the SAME per-pane composer mutation lock `/compact` steering holds, so a
    /// user stop-Escape cannot interleave its key send with a busy-pane auto
    /// `/compact`. While a simulated `/compact` holds the composer lock, the
    /// Escape send must NOT run; it proceeds only once the lock releases.
    /// Reverting the routing (sending the Escape without the composer lock) lets
    /// it run immediately, failing the `recv_timeout(..).is_err()` assertion.
    #[cfg(unix)]
    #[test]
    fn tui_escape_delivery_serializes_with_compact_composer_lock() {
        use std::sync::mpsc;
        use std::time::Duration;

        let session = format!("claude-4591-stop-escape-{}", uuid::Uuid::new_v4());
        let (holding_tx, holding_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (escape_ran_tx, escape_ran_rx) = mpsc::channel();

        // Simulate `/compact` steering holding the composer lock for this session.
        let lock_session = session.clone();
        thread::spawn(move || {
            crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
                &lock_session,
                || {
                    holding_tx.send(()).expect("signal compact holding lock");
                    release_rx.recv().expect("await release");
                },
            );
        });
        holding_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("compact must acquire the composer lock first");

        let escape_session = session.clone();
        thread::spawn(move || {
            let result = deliver_tui_escape_under_composer_lock(&escape_session, || {
                escape_ran_tx.send(()).expect("signal escape send ran");
                Ok::<(), String>(())
            });
            assert!(result.is_ok(), "escape send closure returns Ok");
        });
        assert!(
            escape_ran_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "the stop Escape must wait behind the /compact composer lock"
        );
        release_tx.send(()).expect("release compact");
        escape_ran_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("the stop Escape proceeds once the composer lock releases");
    }

    /// P2 #4616 regression: the interactive stop-Escape must NOT hold the GLOBAL
    /// interrupt-registry lock (`ACTIVE_GENERATION_BY_TMUX`) while it is parked on
    /// the per-pane composer lock. `deliver_claimed_claude_stop_under_lock_order`
    /// (the exact routing production uses) takes the composer lock OUTSIDE the
    /// registry delivery guard, so while a busy-pane `/compact` holds the composer
    /// lock for one pane, a *different* pane's turn-start bind — and every other
    /// registry op — still proceeds immediately.
    ///
    /// Guard removal: reverting to the registry-first order (acquiring the registry
    /// delivery guard before waiting on the composer lock, as the pre-#4616 rework
    /// did via `deliver_claimed_claude_stop(.., || deliver_tui_escape_under_composer_lock(..))`)
    /// would hold the global registry lock across the multi-second composer wait,
    /// and the other-session `bind_claude_tmux_session` below would block until
    /// `/compact` released — failing the `recv_timeout(..)` this test expects to
    /// succeed promptly.
    #[cfg(unix)]
    #[test]
    fn stop_escape_never_holds_interrupt_registry_while_parked_on_composer_lock() {
        use std::sync::mpsc;
        use std::time::Duration;

        let session = format!("claude-4616-p2-stall-{}", uuid::Uuid::new_v4());
        let other_session = format!("claude-4616-p2-other-{}", uuid::Uuid::new_v4());

        // Publish a live generation for `session` and reserve the delivery so the
        // stop path can acquire the interrupt-registry guard once it holds composer.
        let token = Arc::new(CancelToken::new());
        token.bind_claude_tmux_session(&session);
        assert!(token.claim_claude_interrupt());

        let (compact_holding_tx, compact_holding_rx) = mpsc::channel();
        let (release_compact_tx, release_compact_rx) = mpsc::channel();
        let (delivery_done_tx, delivery_done_rx) = mpsc::channel();

        // A busy-pane `/compact` holds the composer lock for `session`.
        let compact_session = session.clone();
        thread::spawn(move || {
            crate::services::claude_tui::composer_lock::with_composer_mutation_lock(
                &compact_session,
                || {
                    compact_holding_tx
                        .send(())
                        .expect("signal compact holding composer");
                    release_compact_rx.recv().expect("await compact release");
                },
            );
        });
        compact_holding_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("compact must acquire the composer lock first");

        // Drive the production stop routing for `session`. It must block on the
        // composer lock (held by `/compact`) BEFORE it can touch the registry.
        let stop_session = session.clone();
        let stop_token = Arc::clone(&token);
        thread::spawn(move || {
            let result = deliver_claimed_claude_stop_under_lock_order(
                stop_token.as_ref(),
                &stop_session,
                ClaudeTurnInterruptDelivery::TuiEscape,
                None,
                || Ok::<(), String>(()),
            );
            delivery_done_tx.send(result).expect("signal delivery done");
        });
        assert!(
            delivery_done_rx
                .recv_timeout(Duration::from_millis(50))
                .is_err(),
            "the stop Escape must wait behind the /compact composer lock"
        );

        // CRITICAL: while the stop is parked on the per-pane composer lock, the
        // GLOBAL interrupt-registry must be free — a different pane's turn-start
        // bind proceeds without waiting for `/compact` to release. This is the
        // property the pre-#4616 registry-first order violated.
        let (bind_done_tx, bind_done_rx) = mpsc::channel();
        let bind_session = other_session.clone();
        thread::spawn(move || {
            let other = CancelToken::new();
            other.bind_claude_tmux_session(&bind_session);
            // A fresh generation must also be able to acquire its own delivery
            // guard, proving the global registry lock is fully released — not
            // merely available for the `bind` insert half.
            assert!(other.claim_claude_interrupt());
            assert!(
                other
                    .lock_current_claude_interrupt_session(&bind_session)
                    .is_some()
            );
            bind_done_tx
                .send(())
                .expect("signal other-session bind done");
        });
        bind_done_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("a different pane's registry op must not block on the parked composer wait");

        // Release `/compact`; the parked stop now proceeds and commits the fence.
        release_compact_tx.send(()).expect("release compact");
        let delivered = delivery_done_rx
            .recv_timeout(Duration::from_millis(250))
            .expect("the stop Escape proceeds once the composer lock releases");
        assert_eq!(delivered, Ok(()));
        assert!(
            !token.claim_claude_interrupt(),
            "a committed fence refuses a second claim"
        );
    }
}
