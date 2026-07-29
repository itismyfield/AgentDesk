//! #3038 S1 tmux watcher terminal commit decisions.

use super::*;

pub(super) fn watcher_tui_gate_blocks_lifecycle(
    gate_outcome: TuiCompletionGateOutcome,
    terminal_delivery_committed: bool,
) -> bool {
    let _ = (gate_outcome, terminal_delivery_committed);
    false
}

pub(super) fn watcher_commit_should_advance_runtime_binding(
    terminal_output_committed: bool,
    gate_outcome: TuiCompletionGateOutcome,
    terminal_delivery_committed: bool,
) -> bool {
    terminal_output_committed
        && !watcher_tui_gate_blocks_lifecycle(gate_outcome, terminal_delivery_committed)
}

#[allow(clippy::too_many_arguments)]
pub(super) fn mark_watcher_terminal_delivery_committed(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    expected_identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
    full_response: &str,
    turn_data_start_offset: u64,
    generation_mtime_ns: Option<i64>,
    last_offset: u64,
) -> bool {
    let Some(expected_identity) = expected_identity else {
        return false;
    };
    if full_response.trim().is_empty() {
        return false;
    }
    // #3169 P1: self-paced loop turns carry `user_msg_id == 0` (no anchored
    // Discord user message), so the original `user_msg_id != 0` requirement
    // skipped them entirely — they never set `terminal_delivery_committed`, and
    // the #3126 stall-watchdog guard (recovery.rs:1346) had no architectural
    // "this turn finished delivering" signal for them, producing the death #1
    // false-positive force-clean. Allow `user_msg_id == 0` turns to commit, but
    // (NOT a blanket relaxation) only when the frame-carried identity is fully
    // anchored: such turns are disambiguated solely by `started_at` +
    // `turn_start_offset` (#3041 P1-3, inflight.rs:669), so a loop turn without a
    // known `turn_start_offset` cannot be safely matched and is still skipped.
    let is_loop_turn = expected_identity.user_msg_id == 0;
    if is_loop_turn && expected_identity.turn_start_offset.is_none() {
        return false;
    }

    // #3558: the old unlocked `load_inflight_state` → mutate → `save_inflight_state`
    // re-wrote `last_offset`/`response_sent_offset` from a stale snapshot, racing a
    // concurrent owner-gated `refresh_inflight_last_offset_*` advance and emitting a
    // spurious `response_sent_offset_monotonic` / `last_offset_monotonic` violation.
    // The locked RMW helper holds the sidecar flock across reload → identity guard →
    // patch → persist. The strong identity guard below (user_msg_id + started_at +
    // tmux_session + turn_start_offset, including the #3169 loop-turn pin) is enforced
    // inside the helper via `InflightTurnIdentity::matches_state`, which compares all
    // four fields — `expected_identity` already carries them — plus the caller-supplied
    // `tmux_session_name`. The commit IS the watermark owner, so it writes
    // `last_offset`/`response_sent_offset`, but the helper `max`-serializes both
    // against the in-lock reload so a late commit never moves them backward.
    let outcome = crate::services::discord::inflight::commit_watcher_terminal_delivery_locked(
        provider,
        channel_id.get(),
        expected_identity,
        tmux_session_name,
        crate::services::discord::inflight::WatcherTerminalCommitPatch {
            full_response: full_response.to_string(),
            last_offset,
            last_watcher_relayed_offset: Some(turn_data_start_offset),
            last_watcher_relayed_generation_mtime_ns: generation_mtime_ns,
        },
    );
    match outcome {
        crate::services::discord::inflight::WatcherTerminalCommitOutcome::Committed => true,
        crate::services::discord::inflight::WatcherTerminalCommitOutcome::Skipped => false,
        crate::services::discord::inflight::WatcherTerminalCommitOutcome::IoError => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = channel_id.get(),
                tmux_session = %tmux_session_name,
                "watcher failed to mirror committed terminal delivery into inflight state"
            );
            false
        }
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct WatcherTerminalCommitSideEffects {
    pub(super) advance_runtime_binding: bool,
    pub(super) advance_confirmed_end: bool,
    pub(super) clear_inflight: bool,
    pub(super) finish_restored_turn: bool,
    pub(super) late_output_retry_possible: bool,
}

#[cfg(test)]
pub(super) fn watcher_terminal_commit_side_effects_for_test(
    terminal_output_committed: bool,
    gate_outcome: TuiCompletionGateOutcome,
    terminal_delivery_committed: bool,
) -> WatcherTerminalCommitSideEffects {
    let lifecycle_allowed = terminal_output_committed
        && !watcher_tui_gate_blocks_lifecycle(gate_outcome, terminal_delivery_committed);
    WatcherTerminalCommitSideEffects {
        advance_runtime_binding: watcher_commit_should_advance_runtime_binding(
            terminal_output_committed,
            gate_outcome,
            terminal_delivery_committed,
        ),
        advance_confirmed_end: lifecycle_allowed,
        clear_inflight: lifecycle_allowed,
        finish_restored_turn: lifecycle_allowed,
        late_output_retry_possible: terminal_output_committed && !lifecycle_allowed,
    }
}

pub(super) fn watcher_terminal_kind_requires_tui_completion_gate(
    terminal_kind: Option<WatcherTerminalKind>,
) -> bool {
    !matches!(terminal_kind, Some(WatcherTerminalKind::SoftUserBoundary))
}

pub(super) fn missing_inflight_after_session_bound_delivery(
    inflight_missing: bool,
    session_bound_relay_delivered: bool,
) -> bool {
    inflight_missing && !session_bound_relay_delivered
}

/// #3350 issue-1 pure core: must a committed pass tombstone+drain the row even
/// when the TUI-direct anchor body was NOT visible (e.g. a suppressed
/// task-notification completion)? A watcher-owned `ExternalInput` synthetic
/// row converges its anchor `⏳ → ✅` on EVERY committed pass — suppressed
/// included (`terminal_output_committed = relay_ok || relay_suppressed`) — and
/// its #3303/#3350 DeferredClaim marker pins exactly this row's identity, so
/// skipping the tombstone lets the TTL sweep stack a false `⚠` on that `✅`.
/// Anything else (bridge-owned, Managed, id-0, session-less) keeps the #3296
/// body-visible-only tombstone scope.
pub(super) fn committed_synthetic_commit_requires_marker_tombstone(
    turn_source_external: bool,
    relay_owner_watcher: bool,
    user_msg_id: u64,
    tmux_session_present: bool,
) -> bool {
    turn_source_external && relay_owner_watcher && user_msg_id != 0 && tmux_session_present
}

/// Row adapter for [`committed_synthetic_commit_requires_marker_tombstone`].
pub(super) fn committed_row_requires_marker_tombstone(
    row: &crate::services::discord::inflight::InflightTurnState,
) -> bool {
    use crate::services::discord::inflight::{RelayOwnerKind, TurnSource};
    committed_synthetic_commit_requires_marker_tombstone(
        row.turn_source == TurnSource::ExternalInput,
        row.relay_owner_kind == RelayOwnerKind::Watcher,
        row.user_msg_id,
        row.tmux_session_name.is_some(),
    )
}

#[cfg(test)]
mod runtime_binding_offset_tests {
    use super::*;

    #[test]
    fn committed_watcher_output_advances_runtime_binding_even_without_inflight() {
        assert!(watcher_commit_should_advance_runtime_binding(
            true,
            TuiCompletionGateOutcome::ConfirmedIdle,
            false,
        ));
    }

    #[test]
    fn uncommitted_watcher_output_does_not_advance_runtime_binding() {
        assert!(!watcher_commit_should_advance_runtime_binding(
            false,
            TuiCompletionGateOutcome::ConfirmedIdle,
            false,
        ));
    }

    #[test]
    fn busy_observation_without_delivery_still_advances_runtime_binding() {
        assert!(watcher_commit_should_advance_runtime_binding(
            true,
            TuiCompletionGateOutcome::BusyObserved,
            false,
        ));
    }

    #[test]
    fn busy_observation_without_terminal_delivery_allows_cleanup() {
        let side_effects = watcher_terminal_commit_side_effects_for_test(
            true,
            TuiCompletionGateOutcome::BusyObserved,
            false,
        );

        assert!(side_effects.advance_runtime_binding);
        assert!(side_effects.advance_confirmed_end);
        assert!(side_effects.clear_inflight);
        assert!(side_effects.finish_restored_turn);
        assert!(!side_effects.late_output_retry_possible);

        let confirmed = watcher_terminal_commit_side_effects_for_test(
            true,
            TuiCompletionGateOutcome::ConfirmedIdle,
            false,
        );
        assert!(confirmed.advance_runtime_binding);
        assert!(confirmed.advance_confirmed_end);
        assert!(confirmed.clear_inflight);
        assert!(confirmed.finish_restored_turn);
        assert!(!confirmed.late_output_retry_possible);
    }

    #[test]
    fn tui_completion_gate_busy_observation_after_terminal_delivery_allows_lifecycle_cleanup() {
        let side_effects = watcher_terminal_commit_side_effects_for_test(
            true,
            TuiCompletionGateOutcome::BusyObserved,
            true,
        );

        assert!(side_effects.advance_runtime_binding);
        assert!(side_effects.advance_confirmed_end);
        assert!(side_effects.clear_inflight);
        assert!(side_effects.finish_restored_turn);
        assert!(!side_effects.late_output_retry_possible);
    }

    #[test]
    fn soft_user_boundary_terminal_skips_tui_completion_gate() {
        assert!(!watcher_terminal_kind_requires_tui_completion_gate(Some(
            WatcherTerminalKind::SoftUserBoundary
        )));
        assert!(watcher_terminal_kind_requires_tui_completion_gate(Some(
            WatcherTerminalKind::SoftStopHookSummary
        )));
        assert!(watcher_terminal_kind_requires_tui_completion_gate(Some(
            WatcherTerminalKind::HardResult
        )));
        assert!(watcher_terminal_kind_requires_tui_completion_gate(None));
    }

    #[test]
    fn acknowledged_session_bound_delivery_is_not_missing_inflight_fallback() {
        assert!(!missing_inflight_after_session_bound_delivery(true, true));
        assert!(missing_inflight_after_session_bound_delivery(true, false));
        assert!(!missing_inflight_after_session_bound_delivery(false, false));
    }

    /// #3350 issue-1: a suppressed (body-invisible) committed pass must still
    /// tombstone a watcher-owned ExternalInput synthetic row — that is the
    /// exact class whose `⏳ → ✅` block fires while the old body-visible-only
    /// tombstone gate skipped, stacking a TTL `⚠` on the `✅`.
    #[test]
    fn suppressed_commit_requires_tombstone_only_for_watcher_synthetic_rows() {
        // The false-⚠ class: watcher-owned synthetic row with a real anchor.
        assert!(committed_synthetic_commit_requires_marker_tombstone(
            true, true, 42, true
        ));
        // Bridge-owned synthetic turn (SC3): finalizes via the bridge, no
        // watcher tombstone owed outside the body-visible #3296 scope.
        assert!(!committed_synthetic_commit_requires_marker_tombstone(
            true, false, 42, true
        ));
        // Managed (non-synthetic) row keeps the #3296 body-visible-only scope.
        assert!(!committed_synthetic_commit_requires_marker_tombstone(
            false, true, 42, true
        ));
        // id-0 rows can never carry an own-pin marker (record rejects zero).
        assert!(!committed_synthetic_commit_requires_marker_tombstone(
            true, true, 0, true
        ));
        // Session-less rows are outside every marker's reconcile scope.
        assert!(!committed_synthetic_commit_requires_marker_tombstone(
            true, true, 42, false
        ));
    }
}

/// #4961 Phase B: break the redrive livelock at the soft-terminal authority
/// refusal — but only over bytes the record proves CONTIGUOUSLY delivered.
///
/// When a re-read frame cannot authenticate soft-terminal authority the watcher
/// drops the range and continues. That is correct for an unproven range, and it
/// is also where the livelock closes: redrive rewound the reader to a frontier
/// `F`, the re-read range is refused here, `confirmed_end_offset` never moves,
/// redrive sees "no progress", backs off, and rewinds to `F` again. Each pass
/// re-creates a streaming panel whose anchor was lost, which is why the same body
/// is posted five to seven times while other blocks are never posted at all.
///
/// The escape is durable proof, not relaxed authority (#4030 forbids widening the
/// identity equality). Proof here has to be stronger than "some delivery in this
/// generation ended past the watermark": `merge_confirmed_frontier` keeps the
/// highest END and never requires it to start where the last one stopped, so an
/// END-only test can jump the watermark ACROSS an undelivered hole and convert a
/// noisy duplicate loop into a silent, unalarmed loss. This therefore requires the
/// proven range to START at or before what is already committed — an unbroken
/// prefix — and it never advances past the range the caller actually refused.
pub(super) fn commit_proven_soft_terminal_backlog(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &str,
    refused_range: (u64, u64),
    source_authority: WatcherSourceAuthority,
) -> bool {
    let transcript_eof = std::fs::metadata(output_path).ok().map(|meta| meta.len());
    let Some((proven_start, proven_end)) = dr::delivered_frontier_range_current_generation(
        provider,
        channel_id,
        tmux_session_name,
        transcript_eof,
    ) else {
        return false;
    };
    let committed = shared.committed_relay_offset(channel_id);
    // Contiguity: a frontier that begins past `committed` leaves a hole whose
    // bytes were never proven delivered. Advancing over it would erase the very
    // backlog the redrive exists to re-deliver.
    if proven_start > committed || proven_end <= committed {
        return false;
    }
    // Never claim more than the caller refused: the refusal is what this catch-up
    // is settling, and bytes past it belong to a frame nobody has consumed yet.
    let target = proven_end.min(refused_range.1.max(committed));
    if target <= committed {
        return false;
    }
    let identity = terminal_long_chunks::watcher_delivery_identity(
        source_authority.generation_mtime_ns,
        source_authority.reset_incarnation,
        None,
    );
    let advanced = matches!(
        terminal_long_chunks::advance_watcher_terminal_delivery(
            crate::services::discord::tmux::WatcherDeliveryTarget {
                shared,
                provider,
                channel_id,
                tmux_session_name,
            },
            identity,
            target,
        ),
        terminal_long_chunks::GuardedWatcherDeliveryResult::AdvancedWithoutProof
            | terminal_long_chunks::GuardedWatcherDeliveryResult::Persisted
    );
    tracing::warn!(
        target: "agentdesk::discord::relay_recovery",
        event = "soft_terminal_frontier_catchup",
        provider = provider.as_str(),
        channel_id = channel_id.get(),
        tmux_session = %tmux_session_name,
        committed,
        proven_start,
        proven_end,
        refused_start = refused_range.0,
        refused_end = refused_range.1,
        target,
        advanced,
        "soft terminal refused authority over a contiguously proven range; advancing the frontier so redrive observes progress"
    );
    advanced
}

#[cfg(test)]
mod soft_terminal_backlog_catchup_tests_4961 {
    use super::*;

    const CH: u64 = 4_961_801;
    const SESSION: &str = "AgentDesk-claude-soft-catchup-4961";

    fn set_generation(session: &str, unix_secs: i64) -> i64 {
        let path = crate::services::tmux_common::session_temp_path(session, "generation");
        std::fs::create_dir_all(std::path::Path::new(&path).parent().unwrap()).unwrap();
        std::fs::write(&path, "phase-b").unwrap();
        filetime::set_file_mtime(&path, filetime::FileTime::from_unix_time(unix_secs, 13)).unwrap();
        dr::current_generation_mtime_ns(session)
    }

    fn seed_frontier(channel: u64, session: &str, generation: i64, range: (u64, u64)) {
        dr::write_delivered_frontier(
            &ProviderKind::Claude,
            channel,
            session,
            dr::DeliveredCommit {
                range,
                generation_mtime_ns: generation,
                attempts: 1,
                panel_msg_id: Some(4_961_802),
                panel_channel_id: Some(channel),
            },
        )
        .expect("seed proven frontier");
    }

    fn authority(
        shared: &Arc<crate::services::discord::SharedData>,
        channel: ChannelId,
        generation: i64,
    ) -> WatcherSourceAuthority {
        WatcherSourceAuthority {
            generation_mtime_ns: generation,
            reset_incarnation: shared.relay_frontier_token(channel).reset_incarnation,
        }
    }

    /// The livelock exit: a refused soft terminal whose range the record proves
    /// contiguously delivered must move `confirmed_end_offset`, so the next redrive
    /// round observes progress instead of rewinding to the same point forever.
    #[test]
    fn contiguous_proof_advances_the_frontier_4961() {
        let temp = tempfile::tempdir().expect("runtime root");
        let _root = crate::config::set_agentdesk_root_for_test(temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(CH);
        let generation = set_generation(SESSION, 1_700_492_300);
        let output = temp.path().join("catchup.jsonl");
        std::fs::write(&output, vec![b'z'; 8192]).expect("transcript");
        seed_frontier(CH, SESSION, generation, (0, 5000));

        assert_eq!(shared.committed_relay_offset(channel), 0);
        assert!(commit_proven_soft_terminal_backlog(
            &shared,
            &ProviderKind::Claude,
            channel,
            SESSION,
            output.to_str().unwrap(),
            (5000, 6200),
            authority(&shared, channel, generation),
        ));
        assert_eq!(shared.committed_relay_offset(channel), 5000);
    }

    /// The regression this guard exists for: a frontier that STARTS past the
    /// watermark leaves an undelivered hole. Advancing over it would erase the very
    /// backlog redrive is meant to re-deliver and would silence the alarm with it —
    /// a noisy duplicate loop traded for a quiet loss.
    #[test]
    fn non_contiguous_proof_must_not_jump_the_hole_4961() {
        let temp = tempfile::tempdir().expect("runtime root");
        let _root = crate::config::set_agentdesk_root_for_test(temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let session = "AgentDesk-claude-soft-catchup-hole-4961";
        let channel = ChannelId::new(4_961_806);
        let generation = set_generation(session, 1_700_492_350);
        let output = temp.path().join("hole.jsonl");
        std::fs::write(&output, vec![b'z'; 8192]).expect("transcript");
        // [0, 3000) was never delivered; only [3000, 5000) is proven.
        seed_frontier(4_961_806, session, generation, (3000, 5000));

        assert!(!commit_proven_soft_terminal_backlog(
            &shared,
            &ProviderKind::Claude,
            channel,
            session,
            output.to_str().unwrap(),
            (5000, 6200),
            authority(&shared, channel, generation),
        ));
        assert_eq!(
            shared.committed_relay_offset(channel),
            0,
            "the undelivered prefix must keep the watermark pinned"
        );
    }

    /// The catch-up settles the refusal; it must not claim bytes past the range the
    /// caller actually refused, which belong to a frame nobody has consumed.
    #[test]
    fn advance_is_capped_at_the_refused_range_4961() {
        let temp = tempfile::tempdir().expect("runtime root");
        let _root = crate::config::set_agentdesk_root_for_test(temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let session = "AgentDesk-claude-soft-catchup-cap-4961";
        let channel = ChannelId::new(4_961_807);
        let generation = set_generation(session, 1_700_492_360);
        let output = temp.path().join("cap.jsonl");
        std::fs::write(&output, vec![b'z'; 16384]).expect("transcript");
        seed_frontier(4_961_807, session, generation, (0, 9000));

        assert!(commit_proven_soft_terminal_backlog(
            &shared,
            &ProviderKind::Claude,
            channel,
            session,
            output.to_str().unwrap(),
            (0, 4000),
            authority(&shared, channel, generation),
        ));
        assert_eq!(
            shared.committed_relay_offset(channel),
            4000,
            "proof reached 9000 but only the refused range may be settled here"
        );
    }

    /// Without durable proof the range is genuinely undelivered: the helper must
    /// stay inert so it can never invent a delivery the user never received.
    #[test]
    fn unproven_backlog_is_left_alone_4961() {
        let temp = tempfile::tempdir().expect("runtime root");
        let _root = crate::config::set_agentdesk_root_for_test(temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let session = "AgentDesk-claude-soft-catchup-noproof-4961";
        let channel = ChannelId::new(4_961_803);
        let generation = set_generation(session, 1_700_492_400);
        let output = temp.path().join("noproof.jsonl");
        std::fs::write(&output, vec![b'z'; 256]).expect("transcript");
        assert!(!commit_proven_soft_terminal_backlog(
            &shared,
            &ProviderKind::Claude,
            channel,
            session,
            output.to_str().unwrap(),
            (0, 256),
            authority(&shared, channel, generation),
        ));
        assert_eq!(shared.committed_relay_offset(channel), 0);
    }

    /// A replaced source incarnation must not be advanced by an older frame's
    /// catch-up — that is the misattribution the guarded funnel exists to stop.
    #[test]
    fn stale_incarnation_catchup_is_refused_4961() {
        let temp = tempfile::tempdir().expect("runtime root");
        let _root = crate::config::set_agentdesk_root_for_test(temp.path());
        let shared = crate::services::discord::make_shared_data_for_tests();
        let session = "AgentDesk-claude-soft-catchup-stale-4961";
        let channel = ChannelId::new(4_961_804);
        let generation = set_generation(session, 1_700_492_500);
        let output = temp.path().join("stale.jsonl");
        std::fs::write(&output, vec![b'z'; 8192]).expect("transcript");
        seed_frontier(4_961_804, session, generation, (0, 5000));
        let stale = WatcherSourceAuthority {
            generation_mtime_ns: generation,
            reset_incarnation: shared
                .relay_frontier_token(channel)
                .reset_incarnation
                .wrapping_add(1),
        };
        assert!(!commit_proven_soft_terminal_backlog(
            &shared,
            &ProviderKind::Claude,
            channel,
            session,
            output.to_str().unwrap(),
            (5000, 6200),
            stale,
        ));
        assert_eq!(shared.committed_relay_offset(channel), 0);
    }
}
