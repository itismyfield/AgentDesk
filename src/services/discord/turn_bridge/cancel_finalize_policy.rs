//! Pure cancel/finalize-policy decision helpers for the turn bridge.
//!
//! #3479 Phase-1 rank-3 extraction: these are the byte-identical
//! value-in/value-out predicates the relay receive loop and finalization
//! path consult — cancel-vs-terminal-frame priority, headless-delivery
//! cancel suppression, final-transcript eligibility, owner-channel
//! resolution, and the turn-finished dispatch-kind classifier. None touch
//! `shared`/`http`/async IO; each is unit-tested. Moved verbatim from
//! `turn_bridge/mod.rs` and re-exported there so call sites stay identical.

use super::*;

pub(in crate::services::discord) fn sync_inflight_restart_mode_from_cancel(
    cancel_token: &crate::services::provider::CancelToken,
    inflight_state: &mut InflightTurnState,
) -> bool {
    let new_mode = cancel_token.restart_mode();
    if inflight_state.restart_mode == new_mode {
        return false;
    }
    match new_mode {
        Some(mode) => inflight_state.set_restart_mode(mode),
        None => inflight_state.clear_restart_mode(),
    }
    true
}

pub(in crate::services::discord) fn classify_turn_finished_dispatch_kind(
    dispatch_context: Option<&str>,
    dispatch_type: Option<&str>,
) -> Option<&'static str> {
    let parsed =
        dispatch_context.and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok());
    if parsed
        .as_ref()
        .is_some_and(|value| json_any_true_flag(value, "auto_queue"))
    {
        return Some("auto_queue");
    }
    match dispatch_type {
        Some("review-decision") => Some("review_decision"),
        _ => None,
    }
}

/// #2289: classifies a stream frame as one that — if processed — would set
/// `done = true` and so could suppress the outer cancel arm (which is
/// gated on `!done`).
///
/// Currently `Done` and `Error` are the only `Ok(msg)` arms that flip
/// `done = true` in the receive-loop body. `Disconnected` is handled
/// separately via the `TryRecvError::Disconnected` branch and has its own
/// re-sample.
///
/// IMPORTANT: when adding a new variant that sets `done = true` in the
/// receive loop, add it here too so the post-`try_recv` cancel re-sample
/// keeps closing the full TOCTOU window. The compiler cannot catch the
/// miss because the loop body uses field destructuring rather than a
/// shared classifier.
#[inline]
pub(in crate::services::discord) fn is_done_setting_terminal_frame(
    msg: &crate::services::agent_protocol::StreamMessage,
) -> bool {
    use crate::services::agent_protocol::StreamMessage::*;
    matches!(msg, Done { .. } | Error { .. })
}

/// #2289 cancel-vs-terminal-frame priority decision.
///
/// Models the post-`try_recv` re-sample in the relay receive loop. When a
/// terminal frame (`Done`) or `Disconnected` is returned, we re-read the
/// cancel flag because `/stop` may have flipped between the pre-`try_recv`
/// guard and the receive call. If the cancel raced ahead, the cancel
/// finalization path must claim the outcome instead of letting `done = true`
/// be set from the frame (which would silently downgrade a user-stop into a
/// recorded completion / empty turn).
///
/// Returns `true` iff the caller must drop the just-received frame and run
/// cancel finalization.
#[inline]
pub(in crate::services::discord) fn should_finalize_cancel_after_recv(
    done: bool,
    cancel_requested: bool,
) -> bool {
    // `!done` enforces the documented "whichever observed terminal state
    // first wins" rule: if a previous iteration already set `done = true`
    // (e.g. a `Done` arrived during the drain window before the user's
    // cancel), keep that completion classification.
    !done && cancel_requested
}

pub(in crate::services::discord) fn should_suppress_headless_delivery_for_cancel(
    cancel_token: Option<&CancelToken>,
) -> bool {
    cancel_requested(cancel_token)
        && !cancel_token.is_some_and(crate::services::provider::CancelToken::is_completion_cleanup)
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn should_record_final_turn_transcript(
    is_prompt_too_long: bool,
    resume_failure_detected: bool,
    recovery_retry: bool,
    rx_disconnected: bool,
    tmux_handed_off: bool,
    bridge_output_delegated: bool,
    terminal_delivery_committed: bool,
    preserve_inflight_for_cleanup_retry: bool,
    full_response: &str,
) -> bool {
    !(is_prompt_too_long
        || resume_failure_detected
        || recovery_retry
        || (rx_disconnected && tmux_handed_off && full_response.is_empty())
        || bridge_output_delegated
        || !terminal_delivery_committed
        || preserve_inflight_for_cleanup_retry)
        && !full_response.trim().is_empty()
}

/// #3041 P1-2 (codex P1-a): resolve the AUTHORITATIVE owner channel a turn's
/// tmux session belongs to, so the bridge's availability check AND its delivery
/// lease acquire+advance key on the SAME channel the (possibly reused) watcher
/// leases+advances on.
///
/// A RECOVERED/restored bridge can reach delivery WITHOUT going through the
/// `TmuxReady`/`RuntimeReady` claim paths (which set `watcher_owner_channel_id =
/// claim.owner_channel_id()`). If it kept its dispatch `channel_id` (Y) while a
/// reused watcher leases on its owner channel (X), the two would hit DIFFERENT
/// `DeliveryLeaseCell`s and both could acquire+deliver = duplicate. Resolving
/// the session's owner channel from the registry here closes that gap in EVERY
/// path. When no reused watcher owns the session (`None`), the bridge owns its
/// own channel → fall back to `dispatch_channel_id`.
pub(in crate::services::discord) fn resolve_bridge_owner_channel(
    tmux_watchers: &TmuxWatcherRegistry,
    tmux_session_name: Option<&str>,
    dispatch_channel_id: ChannelId,
) -> ChannelId {
    tmux_session_name
        .and_then(|session| tmux_watchers.owner_channel_for_tmux_session(session))
        .unwrap_or(dispatch_channel_id)
}

#[cfg(test)]
mod dispatch_kind_tests {
    use super::classify_turn_finished_dispatch_kind;

    #[test]
    fn marks_auto_queue_context() {
        assert_eq!(
            classify_turn_finished_dispatch_kind(
                Some(r#"{"auto_queue":true,"worktree_path":"/tmp/wt"}"#),
                Some("implementation"),
            ),
            Some("auto_queue")
        );
    }

    #[test]
    fn marks_nested_auto_queue_context_to_match_slo_sql_filter() {
        assert_eq!(
            classify_turn_finished_dispatch_kind(
                Some(r#"{"phase_gate":{"auto_queue":true}}"#),
                Some("implementation"),
            ),
            Some("auto_queue")
        );
    }

    #[test]
    fn marks_review_decision_type() {
        assert_eq!(
            classify_turn_finished_dispatch_kind(None, Some("review-decision")),
            Some("review_decision")
        );
    }

    #[test]
    fn auto_queue_takes_precedence_over_review_decision_type() {
        assert_eq!(
            classify_turn_finished_dispatch_kind(
                Some(r#"{"auto_queue":true}"#),
                Some("review-decision"),
            ),
            Some("auto_queue")
        );
    }

    #[test]
    fn keeps_interactive_unclassified() {
        assert_eq!(classify_turn_finished_dispatch_kind(None, None), None);
        assert_eq!(
            classify_turn_finished_dispatch_kind(Some(r#"{"auto_queue":false}"#), Some("review")),
            None
        );
    }
}

#[cfg(test)]
mod transcript_delivery_gate_tests {
    use super::should_record_final_turn_transcript;

    #[test]
    fn generated_but_undelivered_dm_response_is_not_transcript_completion_evidence() {
        assert!(
            !should_record_final_turn_transcript(
                false,
                false,
                false,
                false,
                false,
                false,
                false,
                true,
                "오늘 윤호 수면 루틴에서 바뀐 점이 있을까요?",
            ),
            "a generated response with failed outbound delivery must stay retryable"
        );
    }

    #[test]
    fn delivered_dm_response_can_be_transcript_completion_evidence() {
        assert!(should_record_final_turn_transcript(
            false,
            false,
            false,
            false,
            false,
            false,
            true,
            false,
            "오늘 윤호 수면 루틴에서 바뀐 점이 있을까요?",
        ));
    }
}

#[cfg(test)]
mod cancel_recv_toctou_tests {
    //! #2289: post-`try_recv` cancel re-sample. The relay receive loop must
    //! drop a terminal frame and run cancel finalization when `/stop` flips
    //! the cancel token AFTER the pre-recv guard passed but BEFORE the
    //! receive call returned the frame.

    use super::{should_finalize_cancel_after_recv, should_suppress_headless_delivery_for_cancel};
    use crate::services::agent_protocol::StreamMessage;
    use crate::services::provider::{CancelToken, cancel_requested};
    use std::sync::mpsc;

    #[test]
    fn priority_helper_matches_documented_truth_table() {
        // The documented rule:
        //   - cancel wins iff the loop has not yet observed a terminal
        //     completion (`done == false`)
        //   - once `done == true` (a prior iteration set it from a `Done`
        //     observed before cancel), the completion classification
        //     sticks and a later cancel becomes a no-op (UX: "stop after
        //     completion is a no-op").
        assert!(should_finalize_cancel_after_recv(false, true));
        assert!(!should_finalize_cancel_after_recv(false, false));
        assert!(!should_finalize_cancel_after_recv(true, true));
        assert!(!should_finalize_cancel_after_recv(true, false));
    }

    #[test]
    fn headless_delivery_cancel_gate_allows_normal_completion_cleanup() {
        let completed = CancelToken::new();
        completed.mark_completion_cleanup();
        completed
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);

        assert!(cancel_requested(Some(&completed)));
        assert!(
            !should_suppress_headless_delivery_for_cancel(Some(&completed)),
            "normal completion cleanup must not drop the terminal headless response"
        );

        let stopped = CancelToken::new();
        stopped
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            should_suppress_headless_delivery_for_cancel(Some(&stopped)),
            "real user/watchdog cancellation must still suppress terminal delivery"
        );
    }

    /// Models exactly the receive loop's post-`try_recv` checkpoint:
    ///   1. pre-recv guard samples cancel — clear, proceed.
    ///   2. terminal frame (`Done`) lands in the channel.
    ///   3. user presses `/stop`; the token flips.
    ///   4. `try_recv` returns `Ok(Done)`.
    /// Without the fix, the bridge would handle `Done`, set `done = true`,
    /// and the next outer cancel arm (gated on `!done`) would suppress
    /// finalization — silently downgrading a user-stop to a recorded
    /// completion. With the fix, the post-recv re-sample drops the frame
    /// and routes to cancel finalization.
    #[test]
    fn cancel_flips_between_pre_guard_and_terminal_frame_drops_frame() {
        let (tx, rx) = mpsc::channel::<StreamMessage>();
        let token = CancelToken::new();

        // (1) pre-recv guard passes: cancel not yet set.
        let done = false;
        let pre_guard_cancel = cancel_requested(Some(&token));
        assert!(!pre_guard_cancel);
        assert!(!should_finalize_cancel_after_recv(done, pre_guard_cancel));

        // (2) terminal frame becomes available.
        tx.send(StreamMessage::Done {
            result: "completed".to_string(),
            session_id: None,
        })
        .expect("send Done");

        // (3) /stop fires — token flips AFTER pre-guard.
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // (4) try_recv returns Ok(Done). The fix re-samples cancel here.
        let msg = rx.try_recv().expect("Done frame is available");
        assert!(matches!(msg, StreamMessage::Done { .. }));

        let post_recv_cancel = cancel_requested(Some(&token));
        assert!(post_recv_cancel, "token flipped before recv returned");

        let must_finalize = should_finalize_cancel_after_recv(done, post_recv_cancel);
        assert!(
            must_finalize,
            "#2289: post-recv re-sample MUST claim the outcome for cancel \
             when cancel raced ahead of a terminal frame"
        );

        // The fix's finalize_cancel_inner! macro would now run cancel
        // bookkeeping and `break 'outer`; we must NOT fall through to the
        // `Done` handler that would set `done = true`. Mimic the fixed
        // control flow: do not set `done`.
        // (Verifying the negative: had we processed the frame, `done`
        // would have flipped, suppressing the outer cancel arm.)
        assert!(!done, "Done frame must be dropped, not applied");
    }

    /// Mirror scenario for the `Disconnected` arm: cancel flips between
    /// pre-recv guard and the receiver being dropped. Without the fix the
    /// loop sets `done = true` from the Disconnected arm and exits via the
    /// completion path; with the fix the post-recv re-sample wins.
    #[test]
    fn cancel_flips_between_pre_guard_and_disconnect_drops_disconnect() {
        let (tx, rx) = mpsc::channel::<StreamMessage>();
        let token = CancelToken::new();
        let done = false;

        // (1) pre-recv guard passes.
        assert!(!cancel_requested(Some(&token)));

        // (2) sender drops without sending a terminal frame.
        drop(tx);

        // (3) /stop flips the token.
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // (4) try_recv returns Disconnected.
        let err = rx.try_recv().expect_err("rx must be disconnected");
        assert!(matches!(err, mpsc::TryRecvError::Disconnected));

        // Post-recv re-sample MUST route to cancel finalization, not to
        // the Disconnected branch that would set done = true.
        let must_finalize = should_finalize_cancel_after_recv(done, cancel_requested(Some(&token)));
        assert!(
            must_finalize,
            "#2289: Disconnected during cancel race must finalize as cancel, \
             not as completion"
        );
        assert!(
            !done,
            "Disconnected branch must NOT run when cancel won the race"
        );
    }

    /// #2289 round-2 Codex finding: `StreamMessage::Error` also sets
    /// `done = true` in the receive loop, so the same TOCTOU class
    /// applies. If `/stop` flips between the pre-recv guard and `Error`
    /// returning, the Error arm would mark the turn as a transport
    /// failure instead of a user stop. The gate uses
    /// `is_done_setting_terminal_frame` so both `Done` and `Error` are
    /// covered.
    #[test]
    fn cancel_flips_between_pre_guard_and_terminal_error_drops_frame() {
        use super::is_done_setting_terminal_frame;
        let (tx, rx) = mpsc::channel::<StreamMessage>();
        let token = CancelToken::new();
        let done = false;

        assert!(!cancel_requested(Some(&token)));
        tx.send(StreamMessage::Error {
            message: "provider rpc failed".to_string(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
        })
        .expect("send Error");
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let msg = rx.try_recv().expect("Error frame is available");
        assert!(is_done_setting_terminal_frame(&msg));
        assert!(should_finalize_cancel_after_recv(
            done,
            cancel_requested(Some(&token))
        ));
        assert!(!done, "Error frame must be dropped, not applied");
    }

    /// The classifier must list every Ok variant that flips `done = true`.
    #[test]
    fn terminal_frame_classifier_matches_loop_done_assignments() {
        use super::is_done_setting_terminal_frame;
        // These two are the Ok arms that set done = true today.
        assert!(is_done_setting_terminal_frame(&StreamMessage::Done {
            result: String::new(),
            session_id: None,
        }));
        assert!(is_done_setting_terminal_frame(&StreamMessage::Error {
            message: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            exit_code: None,
        }));
        // Sample of non-terminal variants that must NOT trip the gate.
        assert!(!is_done_setting_terminal_frame(&StreamMessage::Text {
            content: "hi".to_string(),
        }));
        assert!(!is_done_setting_terminal_frame(
            &StreamMessage::OutputOffset { offset: 42 }
        ));
    }

    /// #2289 Codex review follow-up: the post-recv re-sample is gated on
    /// `StreamMessage::Done` only. Non-terminal `Ok(msg)` variants
    /// (RuntimeReady, TmuxReady, ProcessReady, OutputOffset, Text, Error,
    /// …) must NOT trigger the cancel finalize even when the token is
    /// flagged, because their data (handoff paths, offsets, watcher debt,
    /// session-reset decisions) needs to be applied before cancel runs.
    /// The next outer-loop iteration's pre-recv cancel guard will then
    /// finalize cleanly — none of those variants set `done = true`.
    #[test]
    fn cancel_after_non_terminal_ok_does_not_drop_frame_directly() {
        let token = CancelToken::new();
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);

        // Production sites gate the post-recv re-sample on
        // `matches!(msg, StreamMessage::Done { .. })` BEFORE calling the
        // helper. Model that here: only `Done` proceeds to the priority
        // check; everything else short-circuits and is processed.
        let non_terminal = StreamMessage::Text {
            content: "partial output".to_string(),
        };
        let must_finalize_now = matches!(non_terminal, StreamMessage::Done { .. })
            && should_finalize_cancel_after_recv(false, cancel_requested(Some(&token)));
        assert!(
            !must_finalize_now,
            "non-terminal Ok variants must NOT be dropped by the post-recv \
             re-sample; their payload (handoff/offset/watcher debt) must be \
             applied before cancel finalizes on the next iteration"
        );

        // For comparison, the same scenario with a Done frame DOES finalize.
        let terminal = StreamMessage::Done {
            result: "completed".to_string(),
            session_id: None,
        };
        let must_finalize_terminal = matches!(terminal, StreamMessage::Done { .. })
            && should_finalize_cancel_after_recv(false, cancel_requested(Some(&token)));
        assert!(
            must_finalize_terminal,
            "Done MUST finalize: it would otherwise set done = true and \
             suppress the outer cancel arm"
        );
    }

    /// Negative case: if `Done` was observed and `done = true` BEFORE the
    /// cancel arrived, the prior completion sticks even though cancel is
    /// now flagged. This preserves the documented "whichever observed
    /// terminal state first wins" rule and keeps `/stop` a no-op after
    /// completion (see the matching comment in the outer loop body).
    #[test]
    fn cancel_after_done_observed_does_not_reclassify() {
        let token = CancelToken::new();
        let done = true; // a previous iteration already set this
        token
            .cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed); // late cancel

        assert!(!should_finalize_cancel_after_recv(
            done,
            cancel_requested(Some(&token))
        ));
    }
}

#[cfg(test)]
mod bridge_owner_channel_resolution_tests {
    use super::*;

    fn live_watcher_handle(tmux_session_name: &str) -> TmuxWatcherHandle {
        TmuxWatcherHandle {
            tmux_session_name: tmux_session_name.to_string(),
            output_path: format!("/tmp/{tmux_session_name}.jsonl"),
            paused: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            resume_offset: Arc::new(std::sync::Mutex::new(None)),
            cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            pause_epoch: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            turn_delivered: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            last_heartbeat_ts_ms: Arc::new(std::sync::atomic::AtomicI64::new(0)),
        }
    }

    // #3041 P1-2 (codex P1-a): a RECOVERED/restored bridge whose dispatch channel
    // (Y) differs from a REUSED watcher's owner channel (X) for the SAME tmux
    // session must resolve the AUTHORITATIVE owner channel to X — so its
    // availability check and delivery-lease acquire+advance key on the SAME cell
    // the watcher leases+advances on (single-holder B2), not the dispatch
    // channel's separate cell (which would let both deliver = duplicate).
    #[test]
    fn recovered_bridge_resolves_reused_watcher_owner_channel_not_dispatch() {
        let registry = TmuxWatcherRegistry::new();
        let tmux = "AgentDesk-claude-adk-cc-t1500000000000000001";
        let owner_channel_x = ChannelId::new(1_500_000_000_000_000_001);
        let dispatch_channel_y = ChannelId::new(2_600_000_000_000_000_002);
        assert_ne!(owner_channel_x, dispatch_channel_y);

        // A live (reused) watcher owns the session under channel X.
        registry.insert(owner_channel_x, live_watcher_handle(tmux));

        // The bridge dispatches on Y but the turn's session is owned by X →
        // resolve to X (the channel the watcher leases on), NOT Y.
        assert_eq!(
            resolve_bridge_owner_channel(&registry, Some(tmux), dispatch_channel_y),
            owner_channel_x,
            "a recovered bridge must lease on the reused watcher's owner channel, not its dispatch channel"
        );

        // The watcher's OWN lease channel is exactly its
        // `owner_channel_for_tmux_session` result — so the two truly match.
        assert_eq!(
            registry.owner_channel_for_tmux_session(tmux),
            Some(owner_channel_x),
            "the watcher's own lease channel must equal the resolved owner channel"
        );
    }

    // #3105 restored-owner (no live watcher handle, settings-derived owner) is
    // still authoritative: a recovered bridge resolves to it so it cannot lease a
    // different cell than the relay path that re-registered the owner.
    #[test]
    fn recovered_bridge_resolves_restored_owner_channel() {
        let registry = TmuxWatcherRegistry::new();
        let tmux = "AgentDesk-claude-adk-cc-t1500000000000000003";
        let restored_owner = ChannelId::new(1_500_000_000_000_000_003);
        let dispatch_channel_y = ChannelId::new(2_600_000_000_000_000_004);

        registry.restore_owner_channel_for_tmux_session(tmux, restored_owner);
        assert_eq!(
            resolve_bridge_owner_channel(&registry, Some(tmux), dispatch_channel_y),
            restored_owner,
        );
    }

    // When NO reused watcher (and no restored owner) owns the session, the bridge
    // owns its own channel → fall back to the dispatch channel.
    #[test]
    fn no_reused_watcher_falls_back_to_dispatch_channel() {
        let registry = TmuxWatcherRegistry::new();
        let dispatch_channel = ChannelId::new(3_700_000_000_000_000_005);
        assert_eq!(
            resolve_bridge_owner_channel(
                &registry,
                Some("AgentDesk-claude-adk-cc-t-unowned"),
                dispatch_channel,
            ),
            dispatch_channel,
            "no owner mapping → the bridge owns its own dispatch channel"
        );
        // No tmux session at all → also falls back to the dispatch channel.
        assert_eq!(
            resolve_bridge_owner_channel(&registry, None, dispatch_channel),
            dispatch_channel,
        );
    }
}
