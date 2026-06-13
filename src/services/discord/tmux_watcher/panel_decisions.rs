//! #3038 S1 tmux watcher status-panel and finalize decisions.

use super::*;

/// #3016 S3 (the A2 / phase-5 enabler): the fresh-idle finalize DECISION,
/// factored out of the production watcher loop so the EXACT production routing is
/// unit-testable end-to-end (the enclosing `tmux_output_watcher_with_restore` is
/// not). It fuses two independent disambiguators:
///
///   1. The STRUCTURAL completion signal (`CompletionSignal`, S1) — the
///      authority that finally distinguishes "turn done" from "paused-live",
///      which the old flag-only path could not:
///        * `Done`       — a structural JSONL terminator is proven on disk
///                         (Claude `result`/`system`, Codex `turn.completed`).
///                         Even when the committed response text is EMPTY/
///                         suppressed, this is a genuine completed turn → finalize.
///        * `PausedLive` — NO terminator (Busy/Inconclusive): paused at a
///                         selector / permission prompt, a subagent still running,
///                         or a long silent tool call. NEVER finalize → defer.
///        * `Unknown`    — non-JSONL runtime (LegacyTmuxWrapper / ProcessBackend /
///                         ClaudeEAdapter, or a non-JSONL provider): the transcript
///                         probe cannot speak, so the pane-idle proxy is the sole
///                         terminal authority. #3016 phase-5b1 routes `Unknown` to
///                         the SAME finalize path as `Done` (flag-independent): the
///                         fresh-idle gate already PROVES pane idle (it only fires
///                         after `watcher_session_ready_for_input` — the SAME
///                         `pane_ready_fallback_allowed && tmux_session_ready_for_input`
///                         predicate the 5a far-backstop uses for `Unknown` — held
///                         over the idle timeout), so finalizing promptly here is
///                         behaviour-equivalent to the old `mailbox_finalize_owed`
///                         flag (owed was ~always true at this arm), without the
///                         1800s far-backstop latency.
///
///   2. The A2-banked wrong-turn-race defenses (only relevant once the signal
///      says we *would* finalize): a follow-up turn can claim the same session
///      during the cleanup `.await`s, so before the destructive clear we
///        * `AbortFollowupTookOver` — `paused_now || epoch_changed` (the SAME
///          predicate as the canonical pause/epoch guard at tmux.rs:7806, but
///          evaluated HERE because this branch `continue`s before that guard
///          would run); and
///        * `SkipStale` — the PINNED pre-cleanup snapshot is a NEWER turn that
///          began AT/AFTER this committed range (`committed_completion_is_stale_for_newer_turn`,
///          or `pinned_finalize_user_msg_id == 0`). Finalizing would release the
///          follow-up; skip and preserve inflight.
///
/// The two combine so that the defer decision keys on the STRUCTURAL TERMINATOR,
/// not on response emptiness — which is the fix for the contradiction that killed
/// the first A2 attempt (the old defer guard deferred `delegated && empty`, the
/// SAME condition as the empty completion it wanted to finalize, so finalize was
/// unreachable). Here an empty-but-TERMINATED completion routes to `Finalize`.
///
/// Degenerate-empty-offset safety: a genuine current-turn fresh idle always has
/// `turn_start_offset < current_offset` (`FreshIdle` requires `output_ever_grew`,
/// and the watcher resumes at `turn_start_offset`), so the pinned id is its real,
/// non-zero id and `SkipStale` cannot misfire on the current turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum FreshIdleFinalizeDecision {
    /// `PausedLive` (no terminator) — defer; preserve inflight, keep waiting.
    DeferPausedLive,
    /// #3016 phase-5b1 (codex HIGH fix): `Unknown` (non-JSONL runtime) with an
    /// EMPTY response — defer; preserve inflight. A non-JSONL turn awaiting a
    /// selector / permission / interactive prompt can look pane-idle with empty
    /// output and has no structured `PausedLive` signal; finalizing here would
    /// kill it mid-work. This is the flag-independent reconstruction of the OLD
    /// (pre-5b1) `delegated_finalize_owed && empty → defer` condition (`owed` was
    /// ~always true for a delegated `Unknown` at this arm, so it was effectively
    /// "empty → defer"). The 5a 1800s far-backstop remains its finalizer.
    DeferEmptyUnknown,
    /// A follow-up turn paused the watcher / bumped the epoch during the cleanup
    /// awaits — abort before the destructive clear; preserve inflight.
    AbortFollowupTookOver,
    /// The pinned pre-cleanup snapshot is a NEWER turn (started AT/AFTER this
    /// committed range) — skip the finalize so the follow-up is not released.
    SkipStale { pinned_user_msg_id: u64 },
    /// `Done` (terminator proven, even if empty) OR NON-empty `Unknown`
    /// (non-JSONL runtime at proven pane-idle) AND no follow-up took over —
    /// finalize via the single-authority path with the PINNED current-turn id.
    Finalize { user_msg_id: u64 },
}

#[allow(clippy::too_many_arguments)]
pub(super) fn watcher_fresh_idle_finalize_decision(
    completion_signal: crate::services::discord::turn_finalizer::CompletionSignal,
    full_response_is_empty: bool,
    paused_now: bool,
    epoch_changed: bool,
    pinned_pre_cleanup_inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
    current_offset: u64,
) -> FreshIdleFinalizeDecision {
    use crate::services::discord::turn_finalizer::CompletionSignal;
    // `Done`  — a structural JSONL terminator is proven on disk → genuine
    //           completion, so it finalizes regardless of emptiness.
    // `Unknown` — non-JSONL runtime (#3016 phase-5b1, codex HIGH fix): the
    //           structural probe cannot speak, so the pane-idle proxy is the only
    //           terminal authority. Reaching this point already PROVES pane idle
    //           (the fresh-idle gate fires only after `watcher_session_ready_for_input`
    //           held over the idle timeout). A NON-empty `Unknown` finalizes
    //           promptly here (flag-independent, the intended 5b1 improvement). An
    //           EMPTY `Unknown`, however, DEFERS: a non-JSONL turn awaiting a
    //           selector / permission / interactive prompt can look pane-idle with
    //           empty output and has no structured `PausedLive` signal, so
    //           finalizing it would kill the turn mid-work. Deferring on emptiness
    //           is the flag-independent reconstruction of the OLD (pre-5b1)
    //           `delegated_finalize_owed && empty → defer` condition (`owed` was
    //           ~always true for a delegated `Unknown` at this arm). The 5a 1800s
    //           far-backstop remains the finalizer for the deferred empty case.
    match completion_signal {
        // No structural terminator: paused at a selector / permission prompt /
        // subagent running / long silent tool call. NEVER finalize.
        CompletionSignal::PausedLive => return FreshIdleFinalizeDecision::DeferPausedLive,
        // Empty non-JSONL `Unknown`: could be awaiting an interactive prompt with no
        // `PausedLive` signal. Defer (the codex HIGH fix); far-backstop finalizes.
        CompletionSignal::Unknown if full_response_is_empty => {
            return FreshIdleFinalizeDecision::DeferEmptyUnknown;
        }
        // `Done` (even empty) or NON-empty `Unknown`: fall through to finalize.
        CompletionSignal::Done | CompletionSignal::Unknown => {}
    }
    // The A2 wrong-turn-race defenses, applied identically to `Done` and non-empty
    // `Unknown` before releasing the turn (paused/epoch abort, then the
    // stale-for-newer-turn skip).
    if paused_now || epoch_changed {
        return FreshIdleFinalizeDecision::AbortFollowupTookOver;
    }
    let stale = committed_completion_is_stale_for_newer_turn(
        pinned_pre_cleanup_inflight,
        None,
        tmux_session_name,
        current_offset,
    );
    let pinned = pinned_finalize_user_msg_id(
        pinned_pre_cleanup_inflight,
        tmux_session_name,
        current_offset,
    );
    if stale || pinned == 0 {
        return FreshIdleFinalizeDecision::SkipStale {
            pinned_user_msg_id: pinned,
        };
    }
    FreshIdleFinalizeDecision::Finalize {
        user_msg_id: pinned,
    }
}

/// #3419 R2 (codex HIGH): the turn-watchdog TIMEOUT finalize decision. The
/// timeout path has NO committed output range, so it cannot reuse the fresh-idle
/// `current_offset` range test. Instead it pins the identity the watcher
/// ATTACHED to at the top of this loop pass (`startup_inflight_snapshot`, the
/// turn this watcher instance is watching) and finalizes ONLY when that pinned
/// turn is STILL the live on-disk inflight owner — exactly the case that wedges
/// the soft-queue (the watcher's own restored Discord turn holds the mailbox
/// cancel_token and timed out without a result).
///
/// `Finalize` requires BOTH:
///   1. the pinned snapshot carries a non-zero `user_msg_id` on this session
///      (a restored Discord turn — the only turn that owns a wedge-prone mailbox
///      token; `reacquire_watcher_inflight_for_active_stream` synthetic id-0
///      rows never call `mailbox_try_start_turn`, so an id-0 timeout can NEVER
///      strand the queue and never needs this finalize), AND
///   2. the CURRENT on-disk inflight identity still EQUALS the pinned identity
///      (`user_msg_id` + `started_at` + `tmux_session_name`, the same triple
///      `InflightTurnIdentity::matches_state` uses). A mismatch means a NEWER
///      turn B took over the session during the long timeout window — B owns the
///      mailbox and finalizes itself, so finalizing here would `mailbox_finish_
///      turn_if_matches` / id-0-collapse onto B and STEAL it.
///
/// Otherwise `Skip`: never submit id-0 (the `resolve_channel_only` id-0 collapse
/// + unconditional `mailbox_finish_turn` at turn_finalizer.rs:1047 is the exact
/// new-turn-steal hazard), and never finalize a mismatched/absent turn. The
/// watcher's original turn was already superseded; the queue is not trapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TimeoutFinalizeDecision {
    /// No pinned watcher turn, the pinned turn is id-0 (synthetic, no mailbox
    /// token), or the on-disk inflight is no longer the pinned turn (a newer
    /// turn took over) — do NOT finalize; just fall through. Carries the pinned
    /// id (0 when absent/synthetic) purely for the skip log.
    Skip { pinned_user_msg_id: u64 },
    /// The watcher's own pinned restored Discord turn is STILL the live inflight
    /// owner and timed out — finalize it via the single-authority path with its
    /// REAL pinned id and an identity-matched clear.
    Finalize { user_msg_id: u64 },
}

pub(super) fn watcher_timeout_finalize_decision(
    pinned_startup_inflight: Option<&InflightTurnState>,
    current_on_disk_inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> TimeoutFinalizeDecision {
    let Some(pinned) = pinned_startup_inflight.filter(|state| {
        state.user_msg_id != 0
            && state.tmux_session_name.as_deref().map(str::trim) == Some(tmux_session_name.trim())
    }) else {
        return TimeoutFinalizeDecision::Skip {
            pinned_user_msg_id: pinned_startup_inflight.map(|s| s.user_msg_id).unwrap_or(0),
        };
    };
    let pinned_identity =
        crate::services::discord::inflight::InflightTurnIdentity::from_state(pinned);
    // The current on-disk row must STILL be the pinned turn. A newer turn B that
    // started during the timeout window has a different identity → Skip (B owns
    // the mailbox and finalizes itself).
    let still_owner = current_on_disk_inflight
        .map(crate::services::discord::inflight::InflightTurnIdentity::from_state)
        .is_some_and(|current| current == pinned_identity);
    if still_owner {
        TimeoutFinalizeDecision::Finalize {
            user_msg_id: pinned.user_msg_id,
        }
    } else {
        TimeoutFinalizeDecision::Skip {
            pinned_user_msg_id: pinned.user_msg_id,
        }
    }
}

pub(super) fn watcher_should_clear_stale_terminal_message_ids(
    inflight_present: bool,
    has_assistant_response: bool,
    placeholder_msg_id: Option<serenity::MessageId>,
) -> bool {
    has_assistant_response && !inflight_present && placeholder_msg_id.is_some()
}

/// #3003: decide whether the watcher must proactively create a status-panel-v2
/// message for the live turn.
///
/// The Discord intake path (`turn_bridge::mod.rs` ~4356) re-designates the
/// existing user/placeholder message as the panel and publishes a fresh answer
/// message. A pure TUI-direct turn (`TurnSource::ExternalInput` /
/// `ExternalAdopted`) has no preceding Discord-origin message to re-designate,
/// so the panel is never created and `status_panel_msg_id` stays `None` — the
/// dedicated v2 panel never appears for tmux-typed input. When v2 is enabled,
/// no panel exists yet, and the live turn is an external-input turn, the watcher
/// creates the panel itself. Branching on `turn_source` here is presentation
/// bookkeeping only (mirrors the terminal message-id adoption gate at
/// `adopt_watcher_terminal_message_ids_from_inflight`); it does not influence
/// relay membership or completion semantics (#2285 E).
pub(super) fn watcher_should_create_external_input_status_panel(
    status_panel_v2_enabled: bool,
    status_panel_present: bool,
    inflight_represents_external_input: bool,
) -> bool {
    status_panel_v2_enabled && !status_panel_present && inflight_represents_external_input
}

pub(super) fn enqueue_watcher_status_panel_orphan(
    shared: &SharedData,
    provider: &ProviderKind,
    channel_id: ChannelId,
    panel_msg_id: serenity::MessageId,
) {
    crate::services::discord::status_panel_orphan_store::enqueue_separate_status_panel_orphan(
        shared.ui.status_panel_v2_enabled,
        provider,
        &shared.token_hash,
        channel_id.get(),
        panel_msg_id.get(),
    );
}

/// #3003 (codex P2): a status-panel-v2 message already persisted on the
/// matching-session inflight row that the restore seed could not re-hydrate.
///
/// `restored_watcher_turn_from_inflight` returns `None` while
/// `current_msg_id == 0`, so a panel created for a TUI-direct turn *before* its
/// answer placeholder exists is persisted (`status_message_id`) but never
/// re-seeded into `status_panel_msg_id` after a watcher restart. Adopting the
/// persisted id here keeps the watcher from publishing a duplicate/orphan panel.
/// Returns the persisted id only when the inflight belongs to this
/// `tmux_session_name`, mirroring the restore-path session guard. Synthetic
/// headless ids are filtered via `normalize_status_panel_message_id` (codex P2
/// r3) so the adoption path never edits a nonexistent Discord message.
/// #3077 (codex P1): decision for the TUI-direct status-panel publish site
/// once the atomic [`bind_status_panel`] has returned. The bind — not the
/// pre-send `identity_matches` snapshot — is the source of truth for whether the
/// just-sent panel was recorded on the inflight row, so the watcher's local
/// handle MUST be chosen from its outcome:
///
/// * `Bound` / `AlreadyBound` → the row now owns this exact panel; adopt it and
///   do NOT delete (deleting would remove a legitimately-bound panel).
/// * `SkippedPanelAlreadySet(owned)` → the row owns a *different* panel id,
///   observed under the bind's flock. Delete the just-sent duplicate and adopt
///   the row's CURRENT owned panel id (`owned`) — never the pre-bind snapshot,
///   which can be stale when a concurrent writer set the panel between the
///   watcher's snapshot load and this atomic bind (#3077 codex P2 #2). The
///   adoption is still gated on `identity_matches` at the call site, so a
///   replacement turn's panel is not tracked here.
/// * `GuardMismatch` / `Missing` / `IoError` → the bind never happened → the
///   row does NOT reference our panel, so the watcher must not claim ownership
///   of it. Delete the just-sent duplicate and adopt nothing here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TuiStatusPanelBindDecision {
    /// Delete (or enqueue-delete) the just-sent panel message.
    pub(super) delete_sent_panel: bool,
    /// When `true`, adopt the just-sent `panel_msg.id`; when `false`, adopt the
    /// row's owned handle (`owned_panel_id`, only if this is the same turn).
    pub(super) adopt_sent_panel: bool,
    /// On `SkippedPanelAlreadySet`, the row's CURRENT owned (real) panel id as
    /// observed by the bind under its flock. The caller adopts this — gated on
    /// `identity_matches` — instead of re-reading the (possibly stale) pre-bind
    /// snapshot. `None` for every other outcome.
    pub(super) owned_panel_id: Option<u64>,
}

pub(super) fn resolve_tui_status_panel_bind_decision(
    outcome: crate::services::discord::inflight::StatusPanelBindOutcome,
) -> TuiStatusPanelBindDecision {
    use crate::services::discord::inflight::StatusPanelBindOutcome as Outcome;
    match outcome {
        Outcome::Bound | Outcome::AlreadyBound => TuiStatusPanelBindDecision {
            delete_sent_panel: false,
            adopt_sent_panel: true,
            owned_panel_id: None,
        },
        Outcome::SkippedPanelAlreadySet(owned) => TuiStatusPanelBindDecision {
            delete_sent_panel: true,
            adopt_sent_panel: false,
            owned_panel_id: Some(owned),
        },
        Outcome::GuardMismatch | Outcome::Missing | Outcome::IoError => {
            TuiStatusPanelBindDecision {
                delete_sent_panel: true,
                adopt_sent_panel: false,
                owned_panel_id: None,
            }
        }
    }
}

pub(super) fn watcher_persisted_status_panel_msg_id(
    inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> Option<serenity::MessageId> {
    inflight.and_then(|state| {
        if state.tmux_session_name.as_deref() != Some(tmux_session_name) {
            return None;
        }
        crate::services::discord::turn_bridge::normalize_status_panel_message_id(
            state.status_message_id.map(serenity::MessageId::new),
        )
    })
}

/// #3003 (codex P2 r2/r25): is the loaded inflight a TUI-direct/external-input
/// turn that belongs to *this* watcher's `tmux_session_name` AND is owned by the
/// watcher relay (so the watcher — not `turn_bridge` / the session-bound relay —
/// is the status-panel owner)?
///
/// The session guard matters because a same-channel watcher
/// replacement/recovery can load an `ExternalInput`/`ExternalAdopted` inflight
/// for a *different* tmux session; without the match this watcher would publish
/// a status panel the save guard then refuses to persist, leaving an orphan
/// panel for the wrong turn. Mirrors the session guard on the persisted/adoption
/// path (`watcher_persisted_status_panel_msg_id`).
///
/// The relay-owner guard matters because an external-input turn can be routed
/// through the bridge adapter / session-bound relay; its inflight still carries
/// `TurnSource::ExternalInput`, but the watcher is NOT the panel owner. Without
/// this guard the watcher would race `turn_bridge`'s own status-panel-v2
/// creation and leave duplicate/orphan panels (codex P2 r25).
#[allow(dead_code)] // #3034: pure external-input guard pinned by the watcher unit tests.
pub(super) fn watcher_inflight_is_external_input_for_session(
    inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    inflight
        .filter(|state| state.tmux_session_name.as_deref() == Some(tmux_session_name))
        .is_some_and(|state| {
            watcher_inflight_represents_external_input(Some(state))
                && matches!(
                    state.effective_relay_owner_kind(),
                    crate::services::discord::inflight::RelayOwnerKind::Watcher
                )
        })
}

/// status-panel-v2 variant of `watcher_inflight_is_external_input_for_session`.
///
/// Identical session + watcher-relay-owner guards (same orphan-panel reasoning),
/// but gated on the broader `watcher_inflight_is_panel_eligible` predicate so the
/// synthetic monitor/self-paced-loop turns also get a watcher-owned status panel.
/// Used ONLY at the panel-lifecycle sites; the lease/⏳-anchor sites keep the
/// narrower external-input predicate.
pub(super) fn watcher_inflight_is_panel_eligible_for_session(
    inflight: Option<&InflightTurnState>,
    tmux_session_name: &str,
) -> bool {
    inflight
        .filter(|state| state.tmux_session_name.as_deref() == Some(tmux_session_name))
        .is_some_and(|state| {
            watcher_inflight_is_panel_eligible(Some(state))
                && matches!(
                    state.effective_relay_owner_kind(),
                    crate::services::discord::inflight::RelayOwnerKind::Watcher
                )
        })
}

/// #3003 single-chokepoint orphan reclaim: has the in-flight TUI-direct turn
/// been abandoned, so a watcher-created v2 panel can never reach terminal
/// completion?
///
/// True when the inflight row for this channel is gone (a stop/cancel cleared
/// it), has been *replaced* by a different turn on the same channel (codex P2
/// r11 — the original TUI-direct row is just as gone), or a recent turn-stop
/// tombstone covers this turn's byte range. Evaluated at the top of the
/// streaming-interval block and at the terminal chokepoint — before every
/// early-`continue` suppression guard — so no guard can bypass the reclaim,
/// which was the recurring orphan source across the per-guard cleanup attempts.
pub(super) fn watcher_external_input_turn_abandoned(
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    output_path: &str,
    data_start_offset: u64,
    expected_identity: Option<&crate::services::discord::inflight::InflightTurnIdentity>,
) -> bool {
    match crate::services::discord::inflight::load_inflight_state(provider, channel_id.get()) {
        // #3107: inflight-absence alone is NOT abandonment. A live agentic TUI
        // turn can lose its inflight mid-turn (a momentary idle observation
        // commits and clears it) while the pane keeps producing — deleting the
        // status panel here would orphan the live turn (frame_ack MissingTarget).
        // Probe the pane lazily (only on this `None` arm, so the
        // `tmux capture-pane` cost is paid only for an abandonment candidate):
        // if it is actively streaming AND making progress the turn is live →
        // NOT abandoned. A genuinely finished/stopped turn returns to
        // ready-for-input (or its pane freezes), so real orphans (inflight gone
        // AND pane idle/frozen) are still reclaimed.
        None => watcher_inflight_absence_is_abandonment(watcher_pane_live_turn_in_progress(
            tmux_session_name,
            output_path,
        )),
        Some(state) => {
            let replaced = expected_identity.is_some_and(|expected| {
                *expected
                    != crate::services::discord::inflight::InflightTurnIdentity::from_state(&state)
            });
            replaced
                || recent_turn_stop_for_watcher_range(
                    channel_id,
                    tmux_session_name,
                    data_start_offset,
                )
                .is_some()
        }
    }
}

/// #3351: at the orphan-panel reclaim sites, should the same turn's relay
/// placeholder be reclaimed alongside the status panel?
///
/// A message already edited into a real response body (the still-placeholder
/// probe returns false) is NEVER deleted. Turns that DID produce assistant text
/// are excluded (`!has_assistant_response`) so the existing recent-stop /
/// stale-clear arms keep sole ownership of the abandoned-with-response case.
/// A restored placeholder's `last_edit_text` is seeded from
/// `reconstructed_inflight_placeholder_body`, so streamed content also fails
/// the probe and is protected.
pub(super) fn watcher_should_reclaim_orphan_turn_placeholder(
    turn_is_external_input: bool,
    placeholder_msg_id: Option<serenity::MessageId>,
    has_assistant_response: bool,
    last_edit_text: &str,
) -> bool {
    turn_is_external_input
        && placeholder_msg_id.is_some()
        && !has_assistant_response
        && crate::services::discord::placeholder_sweeper::is_message_still_placeholder(
            last_edit_text,
        )
}

/// #3107 (CHANGE 3): pure decision for the `load_inflight_state == None` arm of
/// `watcher_external_input_turn_abandoned`. A missing inflight is abandonment
/// ONLY when the pane is not actively streaming; an actively-streaming pane is a
/// live turn that merely lost its inflight, so its status panel must be
/// preserved (not reclaimed/deleted).
pub(super) fn watcher_inflight_absence_is_abandonment(pane_actively_streaming: bool) -> bool {
    !pane_actively_streaming
}
