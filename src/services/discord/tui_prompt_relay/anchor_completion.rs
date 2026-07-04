//! #3479: the TUI-direct prompt anchor COMPLETION lifecycle (`⏳ → ✅`).
//!
//! Behavior-preserving extraction of the live-relay anchor-completion cluster
//! from the `tui_prompt_relay` parent module: the visibility gate, the deferred
//! `⏳`-completion drain decision, and the reaction-swap completers (shared-slot
//! and pinned-injected-message paths). These run the production reaction swap
//! against the real shared dedupe state, so the move MUST stay byte-identical.
//!
//! Every dependency is reached via the `use super::*;` glob, EXCEPT the
//! `super::formatting` module path (which the glob does not re-export): from
//! this child the parent's `super::formatting` is `super::super::formatting`.
//! The externally-called helpers are re-exported by the parent via
//! `pub(in crate::services::discord) use self::anchor_completion::{...}` so the
//! `crate::services::discord::tui_prompt_relay::{...}` call sites stay
//! byte-identical.

use super::*;

pub(in crate::services::discord) fn should_complete_tui_direct_anchor_lifecycle(
    terminal_output_committed: bool,
    terminal_body_visible: bool,
    anchor_or_lease_present: bool,
    lifecycle_stage_paused: bool,
    inflight_present: bool,
) -> bool {
    terminal_output_committed
        && terminal_body_visible
        && anchor_or_lease_present
        && (lifecycle_stage_paused || !inflight_present)
}

/// #3174: outcome of the relay's deferred ⏳-completion drain decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum DeferredAnchorCompletionDrain {
    /// No matching marker for this turn — common path, do nothing.
    NoMarker,
    /// A matching marker was present AND command_http is available: the marker
    /// was CONSUMED and the caller must deliver the ⏳ → ✅ swap.
    Complete,
    /// A matching marker was present but command_http is unavailable: the marker
    /// was LEFT INTACT (fail-open) so a later attempt can still reconcile it.
    LeftIntactHttpUnavailable,
}

/// #3174 codex P1+P2: decide (and, where safe, perform) the deferred
/// ⏳-completion drain for THIS turn, then tell the caller what to do.
///
/// Turn identity (P1): only a marker stamped with `turn_lease_generation` — the
/// generation of the lease THIS relay invocation recorded — is considered; a
/// marker for a different (newer or older) same-(provider,tmux) turn is ignored.
///
/// HTTP fail-open (P2): the marker is PEEKED first. It is only consumed
/// (`take_...`) when `command_http_available` is `true`, i.e. when the caller can
/// actually deliver the ⏳ → ✅ swap. When HTTP is unavailable the marker is left
/// in place rather than silently dropped (mirrors the #3164 ⏳-add fail-open).
///
/// This decision runs against the real shared dedupe state, so an
/// integration-style test that records a matching marker and calls this function
/// observes the production consume/fail-open behaviour directly.
pub(super) fn decide_deferred_anchor_completion_drain(
    provider: &str,
    tmux_session_name: &str,
    turn_lease_generation: u64,
    command_http_available: bool,
) -> DeferredAnchorCompletionDrain {
    if !crate::services::tui_prompt_dedupe::deferred_anchor_completion_present_for_turn(
        provider,
        tmux_session_name,
        turn_lease_generation,
    ) {
        return DeferredAnchorCompletionDrain::NoMarker;
    }
    if !command_http_available {
        return DeferredAnchorCompletionDrain::LeftIntactHttpUnavailable;
    }
    // HTTP available — consume the marker; the caller delivers the swap.
    crate::services::tui_prompt_dedupe::take_deferred_anchor_completion(
        provider,
        tmux_session_name,
        turn_lease_generation,
    );
    DeferredAnchorCompletionDrain::Complete
}

pub(in crate::services::discord) async fn complete_tui_direct_prompt_anchor_lifecycle_if_present(
    shared: &std::sync::Arc<super::super::SharedData>,
    provider: &str,
    tmux_session_name: &str,
    channel_id: ChannelId,
    generation: u64,
    reason: &'static str,
) -> Option<crate::services::tui_prompt_dedupe::TuiPromptAnchor> {
    let anchor = crate::services::tui_prompt_dedupe::prompt_anchor_for_response(
        provider,
        tmux_session_name,
        channel_id.get(),
    )?;
    let anchor_channel_id = ChannelId::new(anchor.channel_id);
    let anchor_message_id = MessageId::new(anchor.message_id);
    if !super::super::turn_view_reconciler::note_tui_anchor_completed(
        shared,
        anchor_channel_id,
        anchor_message_id,
        generation,
        reason,
    )
    .await
    {
        tracing::warn!(
            provider = %provider,
            channel_id = anchor.channel_id,
            tmux_session_name = %tmux_session_name,
            anchor_message_id = anchor.message_id,
            reason,
            "failed to complete TUI-direct prompt anchor reaction lifecycle; keeping anchor for retry"
        );
        return None;
    }
    crate::services::tui_prompt_dedupe::clear_prompt_anchor_for_response(
        provider,
        tmux_session_name,
        anchor,
    );
    tracing::info!(
        provider = %provider,
        channel_id = anchor.channel_id,
        tmux_session_name = %tmux_session_name,
        anchor_message_id = anchor.message_id,
        reason,
        "completed TUI-direct prompt anchor reaction lifecycle"
    );
    Some(anchor)
}

/// #3099 codex re-review (P2): complete the `⏳ → ✅` lifecycle for a turn that
/// finished with `user_msg_id == 0`, targeting THIS turn's own injected message.
///
/// The shared prompt-anchor slot (`prompt_anchor_by_tmux`) holds a single value
/// per provider/tmux and is overwritten by each new injection. Reading it at
/// completion time is unsafe under rapid/parallel injection: turn A's completion
/// would land the `✅` on turn B's still-running message (and A's `⏳` would never
/// clear). When the inflight row carries its own pinned `injected_prompt_message_id`
/// (recorded when the synthetic turn claimed `⏳`), we swap the reaction on that
/// exact message instead, and only clear the shared slot when it still points at
/// the same message. Falls back to the shared-slot behaviour for legacy inflight
/// rows that pre-date the pinned id.
/// Pure selector for which injected message id the `user_msg_id == 0` completion
/// cleanup should target. Returns `Some(id)` to swap reactions on that exact
/// pinned message (this turn's own id), or `None` to fall back to the legacy
/// shared prompt-anchor slot. A zero pinned id is treated as absent.
pub(super) fn pinned_anchor_cleanup_target(pinned_injected_message_id: Option<u64>) -> Option<u64> {
    pinned_injected_message_id.filter(|id| *id != 0)
}

pub(in crate::services::discord) async fn complete_tui_direct_anchor_lifecycle_for_inflight(
    shared: &std::sync::Arc<super::super::SharedData>,
    provider: &str,
    tmux_session_name: &str,
    channel_id: ChannelId,
    pinned_injected_message_id: Option<u64>,
    generation: u64,
    reason: &'static str,
) -> Option<crate::services::tui_prompt_dedupe::TuiPromptAnchor> {
    let Some(message_id) = pinned_anchor_cleanup_target(pinned_injected_message_id) else {
        // Legacy row without a pinned id — preserve the prior shared-slot path.
        return complete_tui_direct_prompt_anchor_lifecycle_if_present(
            shared,
            provider,
            tmux_session_name,
            channel_id,
            generation,
            reason,
        )
        .await;
    };
    let anchor = crate::services::tui_prompt_dedupe::TuiPromptAnchor {
        channel_id: channel_id.get(),
        message_id,
    };
    let anchor_channel_id = ChannelId::new(anchor.channel_id);
    let anchor_message_id = MessageId::new(anchor.message_id);
    if !super::super::turn_view_reconciler::note_tui_anchor_completed(
        shared,
        anchor_channel_id,
        anchor_message_id,
        generation,
        reason,
    )
    .await
    {
        tracing::warn!(
            provider = %provider,
            channel_id = anchor.channel_id,
            tmux_session_name = %tmux_session_name,
            anchor_message_id = anchor.message_id,
            reason,
            "failed to complete pinned TUI-direct injected-message reaction lifecycle; keeping for retry"
        );
        return None;
    }
    // Only clear the shared slot if it still points at THIS turn's message — a
    // later injection may already own it and must keep its own `⏳`.
    crate::services::tui_prompt_dedupe::clear_prompt_anchor_for_response(
        provider,
        tmux_session_name,
        anchor,
    );
    tracing::info!(
        provider = %provider,
        channel_id = anchor.channel_id,
        tmux_session_name = %tmux_session_name,
        anchor_message_id = anchor.message_id,
        reason,
        "completed pinned TUI-direct injected-message reaction lifecycle"
    );
    Some(anchor)
}

#[cfg(test)]
mod tests {
    use super::*;

    // #3099 codex re-review (P2): the `user_msg_id == 0` completion cleanup must
    // target THIS turn's pinned injected message id, not whatever the single
    // shared prompt-anchor slot currently holds. A non-zero pinned id selects the
    // pinned-message path; an absent / zero pinned id falls back to the slot.
    #[test]
    fn pinned_anchor_cleanup_target_prefers_pinned_id_over_shared_slot() {
        assert_eq!(pinned_anchor_cleanup_target(Some(9001)), Some(9001));
        assert_eq!(pinned_anchor_cleanup_target(Some(0)), None);
        assert_eq!(pinned_anchor_cleanup_target(None), None);
    }

    // #3099 codex re-review (P2): the cross-turn A/B race. Injection A records the
    // shared anchor slot, then injection B overwrites it. When A completes
    // (`user_msg_id == 0`) it must clean up A's OWN message — the shared slot now
    // belongs to B, so reading it would `✅` B's still-running message and leave
    // A's `⏳` stale. The pinned-id cleanup clears the shared slot ONLY if it
    // still matches this turn, so B's anchor survives until B completes.
    #[test]
    fn cross_turn_anchor_cleanup_targets_own_message_and_preserves_later_turn() {
        use crate::services::tui_prompt_dedupe::{
            TuiPromptAnchor, clear_prompt_anchor_for_response, prompt_anchor_for_response,
            record_prompt_anchor,
        };
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();

        let tmux = "AgentDesk-claude-anchor-race";
        let channel = 42_u64;
        let msg_a = 1001_u64;
        let msg_b = 2002_u64;

        // A injected: records the shared slot. A's inflight pins msg_a.
        record_prompt_anchor("claude", tmux, channel, msg_a);
        // B injected: overwrites the single shared slot with msg_b. B pins msg_b.
        record_prompt_anchor("claude", tmux, channel, msg_b);
        assert_eq!(
            prompt_anchor_for_response("claude", tmux, channel),
            Some(TuiPromptAnchor {
                channel_id: channel,
                message_id: msg_b,
            }),
            "shared slot now holds B (the latest injection)"
        );

        // A completes first. The cleanup selects A's pinned id (not the slot).
        assert_eq!(pinned_anchor_cleanup_target(Some(msg_a)), Some(msg_a));
        // The pinned-id path attempts to clear the shared slot for A's anchor; the
        // slot holds B, so the match guard refuses to clear it — B keeps its `⏳`.
        assert!(
            !clear_prompt_anchor_for_response(
                "claude",
                tmux,
                TuiPromptAnchor {
                    channel_id: channel,
                    message_id: msg_a,
                },
            ),
            "A's cleanup must NOT clear B's shared slot"
        );
        assert_eq!(
            prompt_anchor_for_response("claude", tmux, channel),
            Some(TuiPromptAnchor {
                channel_id: channel,
                message_id: msg_b,
            }),
            "B's anchor survives A's completion"
        );

        // B completes: it owns the slot, so its pinned cleanup clears it.
        assert_eq!(pinned_anchor_cleanup_target(Some(msg_b)), Some(msg_b));
        assert!(clear_prompt_anchor_for_response(
            "claude",
            tmux,
            TuiPromptAnchor {
                channel_id: channel,
                message_id: msg_b,
            },
        ));
        assert_eq!(prompt_anchor_for_response("claude", tmux, channel), None);
    }

    #[test]
    fn direct_anchor_lifecycle_requires_visible_terminal_body() {
        assert!(!should_complete_tui_direct_anchor_lifecycle(
            true, false, true, true, false,
        ));
        assert!(!should_complete_tui_direct_anchor_lifecycle(
            false, true, true, true, false,
        ));
        assert!(!should_complete_tui_direct_anchor_lifecycle(
            true, true, false, true, false,
        ));
    }

    #[test]
    fn direct_anchor_lifecycle_uses_bridge_for_active_inflight_unless_paused() {
        assert!(!should_complete_tui_direct_anchor_lifecycle(
            true, true, true, false, true,
        ));
        assert!(should_complete_tui_direct_anchor_lifecycle(
            true, true, true, true, true,
        ));
        assert!(should_complete_tui_direct_anchor_lifecycle(
            true, true, true, false, false,
        ));
    }

    #[test]
    fn direct_anchor_lifecycle_does_not_complete_preserved_cleanup_retry() {
        assert!(!should_complete_tui_direct_anchor_lifecycle(
            false, true, true, false, true,
        ));
        assert!(!should_complete_tui_direct_anchor_lifecycle(
            true, false, true, false, true,
        ));
    }

    // #3174 codex P2 (test-gap a): drive the PRODUCTION relay drain decision
    // (`decide_deferred_anchor_completion_drain`) — the exact function the relay
    // calls after `record_prompt_anchor` — against the real shared dedupe state.
    //
    // Reproduces the ordering race: the watcher's lease-gated completion fired
    // BEFORE this turn's anchor and recorded a deferred marker stamped with this
    // turn's lease generation. When the relay's drain runs (HTTP available) it
    // MUST report `Complete` AND consume the marker. Neutralizing the production
    // decision (e.g. making it always return `NoMarker`, or dropping the
    // `take_...`) makes this test fail (RED) — it is NOT satisfiable by the
    // record/take helpers alone.
    #[test]
    fn relay_drain_decision_completes_and_consumes_matching_turn_marker() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();

        let provider = "claude";
        let tmux = "AgentDesk-claude-deferred-drain";
        let channel = 42_u64;
        let turn_gen = 4242_u64;

        // Watcher anchor-less completion: records the deferred marker for THIS turn.
        crate::services::tui_prompt_dedupe::record_deferred_anchor_completion(
            provider, tmux, channel, turn_gen,
        );

        // Production relay drain decision, HTTP available → Complete + consume.
        assert_eq!(
            decide_deferred_anchor_completion_drain(provider, tmux, turn_gen, true),
            DeferredAnchorCompletionDrain::Complete,
            "matching marker with HTTP available must drive the completion"
        );
        // The marker was consumed: a second drain finds nothing.
        assert_eq!(
            decide_deferred_anchor_completion_drain(provider, tmux, turn_gen, true),
            DeferredAnchorCompletionDrain::NoMarker,
            "the marker must be consumed exactly once"
        );
    }

    // #3174 codex P2 (HTTP fail-open): when command_http is unavailable the drain
    // decision must NOT consume the marker — it returns
    // `LeftIntactHttpUnavailable` and a later attempt (HTTP back) still completes.
    // Proves the swap is not silently lost.
    #[test]
    fn relay_drain_decision_fails_open_when_http_unavailable() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();

        let provider = "claude";
        let tmux = "AgentDesk-claude-deferred-failopen";
        let channel = 42_u64;
        let turn_gen = 9001_u64;

        crate::services::tui_prompt_dedupe::record_deferred_anchor_completion(
            provider, tmux, channel, turn_gen,
        );

        // HTTP unavailable → marker LEFT INTACT (not consumed).
        assert_eq!(
            decide_deferred_anchor_completion_drain(provider, tmux, turn_gen, false),
            DeferredAnchorCompletionDrain::LeftIntactHttpUnavailable,
            "no HTTP must leave the marker claimable, not drop it"
        );
        // HTTP now available → the surviving marker still completes (not lost).
        assert_eq!(
            decide_deferred_anchor_completion_drain(provider, tmux, turn_gen, true),
            DeferredAnchorCompletionDrain::Complete,
            "the fail-open marker must remain drainable by a later HTTP-available attempt"
        );
    }

    // #3174 codex P1 (test-gap b): same-key / different-turn isolation through the
    // PRODUCTION relay drain decision. A marker stamped with turn A's generation
    // must NOT be cross-consumed when turn B (same provider/tmux, different
    // generation) drives the drain.
    #[test]
    fn relay_drain_decision_does_not_cross_consume_other_turn_marker() {
        let _dedupe_guard = crate::services::tui_prompt_dedupe::TEST_LOCK
            .lock()
            .unwrap();
        crate::services::tui_prompt_dedupe::reset_state_for_tests();

        let provider = "claude";
        let tmux = "AgentDesk-claude-deferred-isolation";
        let channel = 42_u64;
        let turn_a_gen = 100_u64;
        let turn_b_gen = 101_u64;

        // Turn A's watcher completion records a marker stamped with A's generation.
        crate::services::tui_prompt_dedupe::record_deferred_anchor_completion(
            provider, tmux, channel, turn_a_gen,
        );

        // Turn B (same key, newer generation) drives its drain → must NOT consume A.
        assert_eq!(
            decide_deferred_anchor_completion_drain(provider, tmux, turn_b_gen, true),
            DeferredAnchorCompletionDrain::NoMarker,
            "a different turn's drain must not cross-consume the marker"
        );

        // Turn A's own drain still completes its OWN marker.
        assert_eq!(
            decide_deferred_anchor_completion_drain(provider, tmux, turn_a_gen, true),
            DeferredAnchorCompletionDrain::Complete,
            "the owning turn's drain must still complete its own marker"
        );
    }
}
