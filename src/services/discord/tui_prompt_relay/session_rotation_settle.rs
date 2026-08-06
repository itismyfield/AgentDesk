//! #5188 (R2): settle the state a Claude SESSION ROTATION leaves behind.
//!
//! When `/clear` (or any continuation cutover) moves the pane onto a new
//! transcript JSONL, the old file is frozen forever. An inflight row still bound
//! to that file is therefore **structurally** unfinalizable: no byte will ever
//! arrive to carry a terminal, so `tui_direct_pending_start` reads `FOREIGN prior
//! inflight is still live` on every later turn, burns its escalation budget, and
//! aborts — permanently, for as long as the pane lives. Cancel does not fix it
//! and neither does `reattach_watcher`: reattaching a watcher to a dead file just
//! re-reads nothing.
//!
//! ## The real risk is the OTHER direction
//! Chasing the rotation eagerly is how you lose data. At the instant the hook
//! reports the new session id, the pre-rotation tail may still owe Discord bytes
//! that were written to the old transcript BEFORE it froze — a genuine answer to
//! a genuine turn. Settling then would discard it. So the plan is ordered:
//! **drain the frozen transcript first, rotate second**, and only give up on the
//! drain once the delivery frontier has provably stopped advancing.
//!
//! [`plan_claude_session_rotation`] is the whole decision and is pure; the apply
//! side below only executes it.

use super::*;

/// How many consecutive drain observations may pass with a frozen delivery
/// frontier before the pre-rotation tail is declared dead and the pinned inflight
/// is settled anyway.
///
/// Consumed by the ~500ms Claude idle poll, so this is a several-second grace —
/// long enough for a tail that is genuinely mid-drain to show progress, short
/// enough that a channel is never wedged for a human-noticeable time. The bound
/// matters: without it a tail that died mid-drain would hold the rotation record
/// (and the channel) forever, which is the very failure being fixed.
pub(in crate::services::discord) const ROTATION_DRAIN_STALL_POLLS: u32 = 20;

/// Everything [`plan_claude_session_rotation`] needs, captured by the caller so
/// the decision itself touches no clock, filesystem, or Discord state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct ClaudeRotationView {
    /// Size of the frozen, pre-rotation transcript.
    pub old_transcript_len: u64,
    /// How far delivery has consumed that transcript.
    pub delivered_frontier: u64,
    /// An inflight row exists for the pane AND is bound to the frozen transcript.
    /// A row bound to some other file is somebody else's problem and is left
    /// strictly alone.
    pub inflight_bound_to_old_transcript: bool,
    /// The pinned row already committed its terminal delivery, so nothing is owed
    /// from the frozen transcript regardless of the byte counts.
    pub inflight_terminal_committed: bool,
    /// Consecutive drain observations without frontier progress.
    pub polls_without_drain_progress: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum ClaudeRotationPlan {
    /// The frozen transcript still holds undelivered bytes for a live inflight
    /// and the tail is still moving. Deliver those FIRST — do not settle, do not
    /// drop the rotation record, re-evaluate on the next poll.
    DrainOldTranscriptFirst { undelivered_bytes: u64 },
    /// An inflight is pinned to the frozen transcript with nothing left owed (or
    /// with a tail that has provably stopped). It can never receive a terminal,
    /// so settle it and release the channel.
    SettleStaleInflight,
    /// Nothing is pinned to the frozen transcript; the rotation is already fully
    /// propagated and the record can be retired.
    RebindOnly,
}

/// Decide what a rotation still owes. Pure.
pub(in crate::services::discord) fn plan_claude_session_rotation(
    view: ClaudeRotationView,
) -> ClaudeRotationPlan {
    if !view.inflight_bound_to_old_transcript {
        return ClaudeRotationPlan::RebindOnly;
    }
    let undelivered_bytes = view
        .old_transcript_len
        .saturating_sub(view.delivered_frontier);
    // The drain-first guard. Removing it turns this fix into a data-loss bug:
    // a live turn's answer, already written to the transcript that `/clear` then
    // froze, would be thrown away in order to chase the new session faster.
    if undelivered_bytes > 0
        && !view.inflight_terminal_committed
        && view.polls_without_drain_progress < ROTATION_DRAIN_STALL_POLLS
    {
        return ClaudeRotationPlan::DrainOldTranscriptFirst { undelivered_bytes };
    }
    ClaudeRotationPlan::SettleStaleInflight
}

/// #5188 (R1): does `existing` carry a session id ADOPTED from a live Claude
/// hook payload, making it authoritative over the on-disk launch script?
///
/// [`claude_continuation_binding_supersedes_launch`] is the pre-existing,
/// filesystem-only cutover test. It is conservative by design and can decline a
/// REAL rotation — it demands that the new transcript's FIRST timestamp be
/// strictly later than the frozen transcript's LAST one, which a trailing
/// rotation-boundary record appended to the old file (or a leading untimestamped
/// record in the new one) defeats. When it declines, the rehydration pass falls
/// through and re-registers the LAUNCH-script binding, silently reverting
/// delivery to the transcript Claude has stopped writing to. That is the observed
/// #5188 signature: `adopted Claude continuation session …` immediately followed
/// by `rehydrated Claude TUI direct relay binding from launch script …
/// transcript_path=<frozen>.jsonl`, after which nothing is ever delivered again.
///
/// A hook payload is different evidence in kind: it was emitted by the running
/// Claude process and names the session it is writing RIGHT NOW, whereas the
/// launch script is a start-time artifact that a `/clear` makes stale forever. So
/// once a payload-adopted id is on record for the pane, a binding carrying that
/// id outranks the launch script.
///
/// Deliberately narrow: it only holds for a ClaudeTui pair where `existing`
/// carries exactly the adopted id, the LAUNCH binding does NOT (so a launch
/// script that has caught up still wins the ordinary `matches_launch` path), and
/// `existing`'s transcript is a real file on disk (a vanished transcript must
/// still fall through to re-derivation).
#[cfg(unix)]
pub(in crate::services::discord) fn hook_adopted_binding_supersedes_launch(
    existing: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    launch: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    hook_adopted_session_id: Option<&str>,
) -> bool {
    let Some(adopted) = hook_adopted_session_id
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return false;
    };
    existing.runtime_kind == RuntimeHandoffKind::ClaudeTui
        && launch.runtime_kind == RuntimeHandoffKind::ClaudeTui
        && existing.session_id.as_deref() == Some(adopted)
        && launch.session_id.as_deref() != Some(adopted)
        && Path::new(&existing.output_path).is_file()
}

/// #5188 (R1): the launch-script rehydration pass's FIRST gate — may the binding
/// already registered for this pane be kept as-is?
///
/// Extracted from the pass so the adopted-session override is exercised through
/// the SAME code production runs, ledger lookup included. Inline in the loop it
/// was unreachable from any test, and deleting the override there changed no
/// test outcome at all.
#[cfg(unix)]
pub(in crate::services::discord) fn existing_claude_binding_outranks_launch_script(
    tmux_session_name: &str,
    existing: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    fresh: Option<&crate::services::tui_prompt_dedupe::TuiRuntimeBinding>,
) -> bool {
    if existing.runtime_kind != RuntimeHandoffKind::ClaudeTui
        || !Path::new(&existing.output_path).exists()
    {
        return false;
    }
    // No launch script to compare against: there is nothing that could replace
    // this binding, so it stands.
    let Some(fresh) = fresh else {
        return true;
    };
    claude_tui_runtime_binding_matches_launch(existing, fresh)
        || claude_continuation_binding_supersedes_launch(existing, fresh)
        || hook_adopted_binding_supersedes_launch(
            existing,
            fresh,
            crate::services::tui_prompt_dedupe::hook_adopted_claude_session_id(tmux_session_name)
                .as_deref(),
        )
}

/// #5188 (R1): the rehydration pass's SECOND gate — may the launch script's
/// re-derived binding overwrite the one already registered?
///
/// A separate gate on purpose: the first one only runs for a pane whose dedupe
/// mirror already resolves to a channel, and a pane that has not got one yet
/// would otherwise still be reverted here.
///
/// Returning `true` for a payload-adopted binding is the #5188 production
/// failure itself — `rehydrated Claude TUI direct relay binding from launch
/// script … transcript_path=<frozen>.jsonl`, after which delivery reads a file
/// that never grows again.
#[cfg(unix)]
pub(in crate::services::discord) fn launch_script_may_replace_binding(
    tmux_session_name: &str,
    existing: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
    fresh: &crate::services::tui_prompt_dedupe::TuiRuntimeBinding,
) -> bool {
    if hook_adopted_binding_supersedes_launch(
        existing,
        fresh,
        crate::services::tui_prompt_dedupe::hook_adopted_claude_session_id(tmux_session_name)
            .as_deref(),
    ) {
        return false;
    }
    !claude_tui_runtime_binding_matches_launch(existing, fresh)
        || !Path::new(&existing.output_path).exists()
}

/// Finalize context for the rotation settle: clear the unfinalizable row and let
/// the queue kick, but do not run completion cleanup (there is no completion to
/// clean up — the transcript that would have carried it is frozen) and do not
/// drain voice.
#[cfg(unix)]
fn rotation_settle_finalize_context() -> super::super::turn_finalizer::FinalizeContext {
    super::super::turn_finalizer::FinalizeContext {
        clear_inflight: true,
        allow_completion_cleanup: false,
        drain_voice: false,
        kickoff_queue: true,
        expected_idempotent_guard_miss: false,
    }
}

/// Per-tick pass: execute the plan for every pane with an unsettled rotation.
///
/// Called from the Claude idle relay loop, which already runs on a ~500ms tick
/// and is the loop that a wedged channel starves. Cheap when there is nothing to
/// do (the ledger is an empty in-memory map).
#[cfg(unix)]
pub(in crate::services::discord) async fn settle_claude_session_rotations(
    shared: &Arc<SharedData>,
) {
    for rotation in crate::services::tui_prompt_dedupe::pending_claude_session_rotations() {
        settle_one_claude_session_rotation(shared, &rotation).await;
    }
}

#[cfg(unix)]
async fn settle_one_claude_session_rotation(
    shared: &Arc<SharedData>,
    rotation: &crate::services::tui_prompt_dedupe::ClaudeSessionRotation,
) {
    let Some(channel_id) = owner_channel_for_tmux_session(
        shared,
        &ProviderKind::Claude,
        &rotation.tmux_session_name,
        RelayEmissionKind::Poll,
    ) else {
        // No authoritative owner channel: nothing can be settled or delivered for
        // this pane yet. Keep the record — the binding half of the rotation (R1)
        // is already in force and a later poll may resolve the channel.
        return;
    };

    let inflight =
        super::super::inflight::load_inflight_state(&ProviderKind::Claude, channel_id.get());
    let inflight_bound_to_old_transcript = inflight.as_ref().is_some_and(|state| {
        state.output_path.as_deref().map(str::trim) == Some(rotation.old_output_path.trim())
    });
    let delivered_frontier = inflight
        .as_ref()
        .map(|state| state.last_offset.max(rotation.old_last_offset))
        .unwrap_or(rotation.old_last_offset);
    let old_transcript_len = std::fs::metadata(&rotation.old_output_path)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    // Fold this observation into the ledger BEFORE planning, so the stall counter
    // the plan reads reflects the frontier we just measured.
    let polls_without_drain_progress = if inflight_bound_to_old_transcript {
        crate::services::tui_prompt_dedupe::record_rotation_drain_progress(
            &rotation.tmux_session_name,
            delivered_frontier,
        )
    } else {
        0
    };

    let view = ClaudeRotationView {
        old_transcript_len,
        delivered_frontier,
        inflight_bound_to_old_transcript,
        inflight_terminal_committed: inflight
            .as_ref()
            .is_some_and(|state| state.terminal_delivery_committed),
        polls_without_drain_progress,
    };

    match plan_claude_session_rotation(view) {
        ClaudeRotationPlan::DrainOldTranscriptFirst { undelivered_bytes } => {
            tracing::debug!(
                provider = "claude",
                channel_id = channel_id.get(),
                tmux_session_name = %rotation.tmux_session_name,
                old_transcript_path = %rotation.old_output_path,
                new_transcript_path = %rotation.new_output_path,
                undelivered_bytes,
                delivered_frontier,
                polls_without_drain_progress,
                "#5188: holding Claude session rotation; the pre-rotation transcript still owes \
                 undelivered bytes to the live turn — delivering those before settling"
            );
        }
        ClaudeRotationPlan::SettleStaleInflight => {
            // #5188 (R4): whatever the frozen transcript still held past the
            // delivery frontier is about to be abandoned. Carry the count into
            // the settle log so a drain that ran out of patience is
            // distinguishable from one that finished, and so the size of what
            // was dropped is on the record instead of being inferred.
            let undelivered_bytes = old_transcript_len.saturating_sub(delivered_frontier);
            let settled =
                submit_rotation_settle(shared, channel_id, rotation, undelivered_bytes).await;
            if settled {
                crate::services::tui_prompt_dedupe::clear_claude_session_rotation(
                    &rotation.tmux_session_name,
                );
            }
        }
        ClaudeRotationPlan::RebindOnly => {
            tracing::info!(
                provider = "claude",
                channel_id = channel_id.get(),
                tmux_session_name = %rotation.tmux_session_name,
                old_session_id = rotation.old_session_id.as_deref().unwrap_or(""),
                new_session_id = %rotation.new_session_id,
                old_transcript_path = %rotation.old_output_path,
                new_transcript_path = %rotation.new_output_path,
                "#5188: Claude session rotation fully propagated; delivery now reads the new \
                 transcript and no inflight is pinned to the frozen one"
            );
            crate::services::tui_prompt_dedupe::clear_claude_session_rotation(
                &rotation.tmux_session_name,
            );
        }
    }
}

/// Settle the inflight pinned to the frozen transcript.
///
/// Re-reads the row under the identity it is about to finalize and re-checks the
/// pin: this runs on a poll loop, so between planning and acting the row may have
/// been replaced by a legitimate successor bound to the NEW transcript, which
/// must never be finalized here.
#[cfg(unix)]
async fn submit_rotation_settle(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    rotation: &crate::services::tui_prompt_dedupe::ClaudeSessionRotation,
    undelivered_bytes: u64,
) -> bool {
    let Some(state) =
        super::super::inflight::load_inflight_state(&ProviderKind::Claude, channel_id.get())
    else {
        return true;
    };
    if state.output_path.as_deref().map(str::trim) != Some(rotation.old_output_path.trim()) {
        // A successor row already owns the slot on the new transcript. The
        // rotation is propagated; leave the successor strictly alone.
        return true;
    }
    let finalizer_turn_id = state.effective_finalizer_turn_id();
    if finalizer_turn_id == 0 {
        tracing::warn!(
            provider = "claude",
            channel_id = channel_id.get(),
            tmux_session_name = %rotation.tmux_session_name,
            old_transcript_path = %rotation.old_output_path,
            "#5188: inflight pinned to a frozen Claude transcript has no finalizer turn id; \
             cannot settle it through the finalizer"
        );
        return false;
    }
    let identity = super::super::inflight::InflightTurnIdentity::from_state(&state);
    let _ = shared
        .turn_finalizer
        .submit_terminal(
            super::super::turn_finalizer::TurnKey::new(
                channel_id,
                finalizer_turn_id,
                shared.restart.current_generation,
            ),
            ProviderKind::Claude,
            super::super::turn_finalizer::TerminalEvent::Complete,
            rotation_settle_finalize_context(),
            shared.clone(),
        )
        .await;

    let gone_or_changed =
        !super::super::inflight::load_inflight_state(&ProviderKind::Claude, channel_id.get())
            .is_some_and(|current| {
                identity == super::super::inflight::InflightTurnIdentity::from_state(&current)
                    && current.effective_finalizer_turn_id() == finalizer_turn_id
            });
    tracing::warn!(
        provider = "claude",
        channel_id = channel_id.get(),
        tmux_session_name = %rotation.tmux_session_name,
        finalizer_turn_id,
        old_session_id = rotation.old_session_id.as_deref().unwrap_or(""),
        new_session_id = %rotation.new_session_id,
        old_transcript_path = %rotation.old_output_path,
        new_transcript_path = %rotation.new_output_path,
        gone_or_changed,
        undelivered_bytes,
        observed_drain_frontier = rotation.observed_drain_frontier,
        polls_without_drain_progress = rotation.polls_without_drain_progress,
        "#5188: settled the inflight pinned to a frozen Claude transcript after a session \
         rotation; it could never have received a terminal (the file it waited on stopped \
         growing), so the channel would otherwise have aborted every later turn. \
         undelivered_bytes>0 means the pre-rotation tail stopped advancing before it drained \
         and that many bytes of the frozen transcript were abandoned"
    );
    gone_or_changed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> ClaudeRotationView {
        ClaudeRotationView {
            old_transcript_len: 0,
            delivered_frontier: 0,
            inflight_bound_to_old_transcript: false,
            inflight_terminal_committed: false,
            polls_without_drain_progress: 0,
        }
    }

    // Direction (a): a rotation with no inflight pinned to the frozen transcript
    // is already fully propagated — delivery follows the new session and the
    // record retires. Before #5188 the binding was reverted to the frozen file by
    // the launch-script rehydration pass and nothing ever retired.
    #[test]
    fn rotation_without_a_pinned_inflight_rebinds_immediately() {
        assert_eq!(
            plan_claude_session_rotation(ClaudeRotationView {
                old_transcript_len: 4096,
                delivered_frontier: 0,
                ..view()
            }),
            ClaudeRotationPlan::RebindOnly,
            "an unpinned frozen transcript is nobody's undelivered work; \
             a huge byte gap must not manufacture a drain wait"
        );
    }

    // Direction (b) — THE REGRESSION THIS FIX MUST NOT CAUSE. A live turn's answer
    // was written to the transcript that `/clear` then froze. Chasing the rotation
    // now would discard it, so the plan must be to drain first.
    #[test]
    fn undelivered_bytes_in_the_frozen_transcript_are_drained_before_the_rotation_settles() {
        assert_eq!(
            plan_claude_session_rotation(ClaudeRotationView {
                old_transcript_len: 8192,
                delivered_frontier: 1024,
                inflight_bound_to_old_transcript: true,
                ..view()
            }),
            ClaudeRotationPlan::DrainOldTranscriptFirst {
                undelivered_bytes: 7168
            },
            "settling here would throw away a real answer that is already on disk"
        );
    }

    #[test]
    fn a_fully_drained_frozen_transcript_settles_its_unfinalizable_inflight() {
        assert_eq!(
            plan_claude_session_rotation(ClaudeRotationView {
                old_transcript_len: 8192,
                delivered_frontier: 8192,
                inflight_bound_to_old_transcript: true,
                ..view()
            }),
            ClaudeRotationPlan::SettleStaleInflight,
            "nothing is owed and the file cannot grow again, so the row is \
             structurally unfinalizable and must not wedge the channel"
        );
    }

    #[test]
    fn a_committed_terminal_settles_even_with_bytes_left_over() {
        assert_eq!(
            plan_claude_session_rotation(ClaudeRotationView {
                old_transcript_len: 8192,
                delivered_frontier: 0,
                inflight_bound_to_old_transcript: true,
                inflight_terminal_committed: true,
                ..view()
            }),
            ClaudeRotationPlan::SettleStaleInflight,
            "trailing bytes after a committed terminal are not undelivered answer"
        );
    }

    #[cfg(unix)]
    fn binding(
        path: &std::path::Path,
        session_id: &str,
    ) -> crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
        crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
            runtime_kind: RuntimeHandoffKind::ClaudeTui,
            output_path: path.display().to_string(),
            relay_output_path: None,
            input_fifo_path: None,
            session_id: Some(session_id.to_string()),
            last_offset: 0,
            relay_last_offset: None,
        }
    }

    // Direction (a), the R1 half: after `/clear` the launch script still names the
    // LAUNCH-time UUID forever. The rehydration pass must not use it to drag the
    // binding back onto the frozen transcript, or delivery reads a dead file for
    // the rest of the pane's life — the exact #5188 production signature.
    #[cfg(unix)]
    #[test]
    fn a_hook_adopted_binding_outranks_the_stale_launch_script() {
        let dir = tempfile::tempdir().expect("tempdir");
        let frozen = dir.path().join("4648aa76.jsonl");
        let rotated = dir.path().join("67f48e65.jsonl");
        std::fs::write(&frozen, b"{}\n").expect("write frozen transcript");
        std::fs::write(&rotated, b"{}\n").expect("write rotated transcript");

        let adopted = binding(&rotated, "67f48e65");
        let launch = binding(&frozen, "4648aa76");

        assert!(
            hook_adopted_binding_supersedes_launch(&adopted, &launch, Some("67f48e65")),
            "the running Claude process reported this session id; the launch script \
             is a start-time artifact that `/clear` made stale forever"
        );
        assert!(
            !hook_adopted_binding_supersedes_launch(&adopted, &launch, None),
            "with no adoption on record the launch script keeps its authority"
        );
        assert!(
            !hook_adopted_binding_supersedes_launch(&adopted, &launch, Some("4648aa76")),
            "an id that is the LAUNCH binding's own must never promote the existing one"
        );
    }

    /// P1-A regression, and the one that matters most in this change.
    ///
    /// Sequence is the production one, in production order:
    ///   1. a `/clear` hook adopts the new session (rotation recorded),
    ///   2. the ~500ms settle tick finds nothing pinned to the frozen
    ///      transcript, plans `RebindOnly`, and retires the rotation record,
    ///   3. the 5s rehydration tick then re-derives the LAUNCH-script binding
    ///      and asks whether it may overwrite the adopted one.
    ///
    /// Step 3 must answer NO. It previously answered YES for every run in which
    /// step 2 got there first — which is nearly all of them — and that answer is
    /// the whole #5188 incident: delivery went back to a transcript Claude had
    /// abandoned and stayed there for 31 minutes with every failure counter at
    /// zero.
    #[cfg(unix)]
    #[test]
    fn rehydration_does_not_revert_an_adopted_binding_after_the_settle_pass_runs() {
        let _serial = crate::services::tui_prompt_dedupe::lock_claude_session_rotations_for_tests();
        let dir = tempfile::tempdir().expect("tempdir");
        let frozen = dir.path().join("4648aa76.jsonl");
        let rotated = dir.path().join("67f48e65.jsonl");
        std::fs::write(&frozen, b"{}\n").expect("write frozen transcript");
        std::fs::write(&rotated, b"{}\n").expect("write rotated transcript");

        let adopted = binding(&rotated, "67f48e65");
        let launch = binding(&frozen, "4648aa76");

        crate::services::tui_prompt_dedupe::record_claude_session_rotation(
            crate::services::tui_prompt_dedupe::ClaudeSessionRotation {
                tmux_session_name: "pane-5188".to_string(),
                old_session_id: Some("4648aa76".to_string()),
                old_output_path: frozen.display().to_string(),
                old_last_offset: 0,
                new_session_id: "67f48e65".to_string(),
                new_output_path: rotated.display().to_string(),
                observed_drain_frontier: 0,
                polls_without_drain_progress: 0,
            },
        );

        // Step 2: exactly what `RebindOnly` does on the first ~500ms tick.
        assert!(
            crate::services::tui_prompt_dedupe::clear_claude_session_rotation("pane-5188"),
            "the settle pass retires the record it just finished with"
        );

        // Step 3, both gates, ~4.5s later.
        assert!(
            !launch_script_may_replace_binding("pane-5188", &adopted, &launch),
            "the launch script must NOT overwrite a payload-adopted binding — \
             surviving the settle pass is the entire point of keeping the \
             adopted-session authority in its own store"
        );
        assert!(
            existing_claude_binding_outranks_launch_script("pane-5188", &adopted, Some(&launch)),
            "and the keep-as-is gate must agree, or the pane is reverted through \
             the other branch instead"
        );
    }

    /// The override is scoped to the pane that actually rotated. A different
    /// pane's binding gets no free pass.
    #[cfg(unix)]
    #[test]
    fn a_pane_with_no_adoption_on_record_still_yields_to_the_launch_script() {
        let _serial = crate::services::tui_prompt_dedupe::lock_claude_session_rotations_for_tests();
        let dir = tempfile::tempdir().expect("tempdir");
        let frozen = dir.path().join("4648aa76.jsonl");
        let rotated = dir.path().join("67f48e65.jsonl");
        std::fs::write(&frozen, b"{}\n").expect("write frozen transcript");
        std::fs::write(&rotated, b"{}\n").expect("write rotated transcript");

        crate::services::tui_prompt_dedupe::record_hook_adopted_claude_session_id(
            "some-other-pane",
            "67f48e65",
        );

        assert!(
            launch_script_may_replace_binding(
                "pane-with-no-adoption",
                &binding(&rotated, "67f48e65"),
                &binding(&frozen, "4648aa76"),
            ),
            "an adoption recorded for a DIFFERENT pane must not keep this pane's \
             binding alive; the ledger lookup has to be keyed by tmux session"
        );
    }

    /// Once `persist_claude_continuation_session` has rewritten the launch
    /// script, the adopted id is no longer evidence of a conflict and the
    /// ordinary match-launch path takes over. This is why the authority needs no
    /// timed expiry: it stops applying by itself.
    #[cfg(unix)]
    #[test]
    fn a_launch_script_that_caught_up_takes_authority_back() {
        let _serial = crate::services::tui_prompt_dedupe::lock_claude_session_rotations_for_tests();
        let dir = tempfile::tempdir().expect("tempdir");
        let rotated = dir.path().join("67f48e65.jsonl");
        std::fs::write(&rotated, b"{}\n").expect("write rotated transcript");

        crate::services::tui_prompt_dedupe::record_hook_adopted_claude_session_id(
            "pane-5188",
            "67f48e65",
        );
        let caught_up = binding(&rotated, "67f48e65");

        assert!(
            !hook_adopted_binding_supersedes_launch(&caught_up, &caught_up, Some("67f48e65")),
            "a launch script naming the adopted id is not stale evidence"
        );
        assert!(
            !launch_script_may_replace_binding("pane-5188", &caught_up, &caught_up),
            "and an identical launch binding needs no overwrite anyway"
        );
    }

    // The override must not resurrect a transcript that no longer exists: a
    // vanished file has to fall through to ordinary re-derivation, otherwise a
    // deleted transcript would pin delivery to nothing at all.
    #[cfg(unix)]
    #[test]
    fn a_hook_adopted_binding_with_no_transcript_on_disk_does_not_win() {
        let dir = tempfile::tempdir().expect("tempdir");
        let frozen = dir.path().join("4648aa76.jsonl");
        std::fs::write(&frozen, b"{}\n").expect("write frozen transcript");
        let missing = dir.path().join("gone.jsonl");

        assert!(
            !hook_adopted_binding_supersedes_launch(
                &binding(&missing, "67f48e65"),
                &binding(&frozen, "4648aa76"),
                Some("67f48e65"),
            ),
            "an adopted id cannot pin delivery to a transcript that is not on disk"
        );
    }

    // The drain wait must be BOUNDED: a tail that died mid-drain must not hold the
    // channel forever, which would just re-create the wedge under a new name.
    #[test]
    fn a_stalled_drain_gives_up_and_settles_at_the_bound() {
        let stalling = ClaudeRotationView {
            old_transcript_len: 8192,
            delivered_frontier: 1024,
            inflight_bound_to_old_transcript: true,
            polls_without_drain_progress: ROTATION_DRAIN_STALL_POLLS - 1,
            ..view()
        };
        assert_eq!(
            plan_claude_session_rotation(stalling),
            ClaudeRotationPlan::DrainOldTranscriptFirst {
                undelivered_bytes: 7168
            },
            "one poll below the bound the drain is still given its chance"
        );
        assert_eq!(
            plan_claude_session_rotation(ClaudeRotationView {
                polls_without_drain_progress: ROTATION_DRAIN_STALL_POLLS,
                ..stalling
            }),
            ClaudeRotationPlan::SettleStaleInflight,
            "at the bound the tail is declared dead and the channel is released"
        );
    }
}
