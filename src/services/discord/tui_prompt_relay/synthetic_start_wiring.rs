//! #4002 — shared TUI-direct synthetic turn-start relay-ownership wiring.
//!
//! The active-turn `else` branch of [`relay_observed_prompt`] installs the
//! passive synthetic inflight and adopts its resolved relay owner so the
//! post-block bridge-tail gate honours cross-relayer single-ownership (Path X).
//! #4082: neutral session notes such as SystemContinuation / compact-resume are
//! explicitly gated out here too, so a future accidental call cannot claim the
//! mailbox or mint a phantom synthetic turn.
//!
//! This helper is a PURE extraction of the else-branch inline block — behaviour
//! is byte-identical for the active-turn callers (HumanTuiDirect /
//! TaskNotification). It lives in its own module (rather than inside
//! `synthetic_start.rs`) so no file crosses the 1000-prod-LoC giant threshold
//! (the giant-file policy prefers splitting over admitting a new giant).
//!
//! [`relay_observed_prompt`]: super::relay_observed_prompt

use super::*;

/// Run the shared synthetic-start wiring for a TUI-direct turn. It:
///   0. refuses prompt classes that the shared injected-prompt decision says do
///      not start an external turn,
///   1. reads the prior-turn view (`synthetic_start_prior_turn_view`),
///   2. either DEFERS the start to the detached per-channel worker when a prior
///      turn is still draining (`should_defer_synthetic_turn_start` /
///      `defer_synthetic_turn_start`) — the observer must then NOT spawn its own
///      BridgeAdapter tail (the worker owns the relay-owner handoff), or
///   3. INLINE-claims a passive synthetic inflight (`claim_tui_direct_synthetic_turn`)
///      and adopts the claim's resolved `relay_owner` back into the lease so the
///      post-block bridge-tail ownership guard sees the true single owner.
///
/// Returns `deferred_synthetic_start`; the caller feeds it into
/// [`observer_should_spawn_bridge_tail`] so a deferred start stands the observer
/// tail down. `lease` is `&mut` so the adopt re-record (fresh generation) is
/// reflected in the caller's lease for that guard.
///
pub(super) async fn wire_tui_direct_synthetic_turn_start(
    shared: &Arc<SharedData>,
    provider_str: &str,
    channel_id: ChannelId,
    prompt: &ObservedTuiPrompt,
    anchor_message_id: MessageId,
    relay_prompt_decision: &RelayObservedPromptInjectionDecision,
    lease: &mut ExternalInputRelayLease,
) -> bool {
    if !relay_prompt_decision.starts_external_turn_lifecycle() {
        tracing::info!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            tmux_session_name = %prompt.tmux_session_name,
            injected_class = ?relay_prompt_decision.injected_class,
            slash_command_kind = relay_prompt_decision.slash_command_kind.as_deref().unwrap_or(""),
            local_only_slash = relay_prompt_decision.local_only_slash,
            "skipped TUI-direct synthetic turn-start for injected prompt class with no external-turn lifecycle"
        );
        return false;
    }
    // #3154 P1-3: set when the synthetic turn-start is DEFERRED to the detached
    // per-channel worker; the observer then must NOT spawn its own BridgeAdapter
    // tail below (a second observer tail would relay the SAME output twice — the
    // original bug). The worker owns the relay-owner handoff.
    let mut deferred_synthetic_start = false;
    if let Some(provider) = ProviderKind::from_str(provider_str) {
        // #3154 — TEMPORAL fix for turn-interleaving. An INLINE claim while the
        // PRIOR turn's tail still drains seeds `turn_start_offset` from the prior
        // cursor (duplicate relay), and an inline wait starves OTHER channels. So
        // an un-finalized prior turn persists a DURABLE pending-start and hands
        // the claim to a DETACHED per-channel worker (fresh EOF offset); the
        // common no-interleave case stays on the inline fast path.
        let prior = super::synthetic_start::synthetic_start_prior_turn_view(
            shared,
            &provider,
            channel_id,
            &prompt.tmux_session_name,
            anchor_message_id.get(),
        )
        .await;
        if super::super::tui_direct_pending_start::should_defer_synthetic_turn_start(prior.view) {
            deferred_synthetic_start = true;
            super::synthetic_start::defer_synthetic_turn_start(
                shared,
                &provider,
                channel_id,
                prompt,
                anchor_message_id,
                &*lease,
            );
            tracing::info!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
                tmux_session_name = %prompt.tmux_session_name,
                anchor_message_id = anchor_message_id.get(),
                "deferred TUI-direct synthetic turn-start off the observer loop; prior turn not yet finalized (durable record persisted, detached per-channel worker spawned)"
            );
        } else {
            let lock = super::super::tui_direct_pending_start::channel_lock(
                provider.as_str(),
                channel_id.get(),
            );
            let _inline_claim_guard = lock.lock().await;
            let claim = super::synthetic_start::claim_tui_direct_synthetic_turn(
                shared,
                &provider,
                channel_id,
                &prompt.tmux_session_name,
                &prompt.prompt,
                anchor_message_id,
                &*lease,
            )
            .await;
            if claim_should_adopt_relay_owner(claim.claimed, lease.relay_owner, claim.relay_owner) {
                lease.relay_owner = claim.relay_owner;
                // Re-record overwrites the lease with a FRESH generation; adopt it
                // back into `lease` so the bridge-tail guard below captures the
                // exact stored identity (a stale generation's Drop would clear
                // nothing / the wrong lease).
                *lease = crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
                    provider.as_str(),
                    &prompt.tmux_session_name,
                    lease.clone(),
                );
            }
            // #3350: the INLINE claim records the same #3303 DeferredClaim marker
            // as the deferred worker (drain ✅ / sweep TTL ⚠). SC3/own-row/I5 gates
            // live in the recorder; a pending_start test pins this wiring.
            super::super::tui_direct_pending_start::record_inline_claim_marker_if_claimed(
                claim.claimed,
                &prompt.provider,
                channel_id.get(),
                anchor_message_id.get(),
                &prompt.tmux_session_name,
                super::super::tui_direct_pending_start::record_claim_marker_if_watcher_owned,
            );
        }
    }
    deferred_synthetic_start
}
