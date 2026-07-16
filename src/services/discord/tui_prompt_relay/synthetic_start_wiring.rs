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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct SyntheticLifecycleAnchor {
    pub(super) message_id: MessageId,
    pub(super) owned_placeholder: bool,
}

pub(super) fn synthetic_lifecycle_anchor_from_placeholder_result(
    notification_anchor_message_id: MessageId,
    placeholder_result: &Result<MessageId, String>,
) -> SyntheticLifecycleAnchor {
    match placeholder_result {
        Ok(message_id) => SyntheticLifecycleAnchor {
            message_id: *message_id,
            owned_placeholder: true,
        },
        Err(_) => SyntheticLifecycleAnchor {
            message_id: notification_anchor_message_id,
            owned_placeholder: false,
        },
    }
}

pub(super) fn failed_synthetic_placeholder_cleanup_target(
    anchor_message_id: MessageId,
    anchor_is_owned_placeholder: bool,
) -> Option<MessageId> {
    anchor_is_owned_placeholder.then_some(anchor_message_id)
}

pub(super) async fn delete_failed_synthetic_owned_placeholder(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    anchor_message_id: MessageId,
    anchor_is_owned_placeholder: bool,
) {
    let Some(anchor_message_id) =
        failed_synthetic_placeholder_cleanup_target(anchor_message_id, anchor_is_owned_placeholder)
    else {
        return;
    };
    let operation_kind =
        super::super::placeholder_cleanup::PlaceholderCleanupOperation::DeleteNonterminal.as_str();
    if let Some(protection) = super::super::placeholder_cleanup::terminal_cleanup_protects_delete(
        &shared.ui.placeholder_cleanup,
        provider,
        channel_id,
        anchor_message_id,
    ) {
        crate::services::observability::emit_relay_delete(
            provider.as_str(),
            channel_id.get(),
            anchor_message_id.get(),
            None,
            None,
            "tui_direct_rejected_synthetic_placeholder_cleanup",
            operation_kind,
            protection.relay_delete_outcome(),
            None,
        );
        return;
    }
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            anchor_message_id = anchor_message_id.get(),
            "failed to delete rejected synthetic placeholder; provider serenity HTTP unavailable"
        );
        return;
    };
    let result = channel_id.delete_message(&http, anchor_message_id).await;
    crate::services::observability::emit_relay_delete_result(
        provider.as_str(),
        channel_id.get(),
        anchor_message_id.get(),
        "tui_direct_rejected_synthetic_placeholder_cleanup",
        operation_kind,
        &result,
    );
    if let Err(error) = result {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = channel_id.get(),
            anchor_message_id = anchor_message_id.get(),
            error = %error,
            "failed to delete rejected synthetic placeholder"
        );
    }
}

/// Post the command-bot-owned streaming placeholder before synthetic lifecycle
/// state becomes visible. Delivery failures fail open to the existing
/// notification/task-card anchor so observation still starts a relay turn.
pub(super) async fn resolve_tui_direct_synthetic_lifecycle_anchor(
    shared: &Arc<SharedData>,
    command_http: Option<Arc<serenity::Http>>,
    channel_id: ChannelId,
    prompt: &ObservedTuiPrompt,
    notification_anchor_message_id: MessageId,
    relay_prompt_decision: &RelayObservedPromptInjectionDecision,
) -> SyntheticLifecycleAnchor {
    if !relay_prompt_decision.starts_external_turn_lifecycle() {
        return SyntheticLifecycleAnchor {
            message_id: notification_anchor_message_id,
            owned_placeholder: false,
        };
    }

    let Some(http) = command_http else {
        let error = "command-bot HTTP unavailable";
        tracing::warn!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            tmux_session_name = %prompt.tmux_session_name,
            notification_anchor_message_id = notification_anchor_message_id.get(),
            "command-bot HTTP unavailable; using the existing notification anchor for the synthetic lifecycle"
        );
        crate::services::observability::emit_intake_placeholder_post_failed(
            &prompt.provider,
            channel_id.get(),
            None,
            "synthetic_anchor_before_lifecycle",
            "notification_anchor_fallback",
            error,
        );
        return SyntheticLifecycleAnchor {
            message_id: notification_anchor_message_id,
            owned_placeholder: false,
        };
    };

    let placeholder_result = super::super::gateway::send_intake_placeholder(
        http,
        shared.clone(),
        channel_id,
        Some((channel_id, notification_anchor_message_id)),
        false,
    )
    .await;
    let anchor = synthetic_lifecycle_anchor_from_placeholder_result(
        notification_anchor_message_id,
        &placeholder_result,
    );
    match placeholder_result {
        Ok(placeholder_message_id) => tracing::info!(
            provider = %prompt.provider,
            channel_id = channel_id.get(),
            tmux_session_name = %prompt.tmux_session_name,
            notification_anchor_message_id = notification_anchor_message_id.get(),
            placeholder_message_id = placeholder_message_id.get(),
            "posted command-bot-owned placeholder for the synthetic lifecycle anchor"
        ),
        Err(error) => {
            tracing::warn!(
                provider = %prompt.provider,
                channel_id = channel_id.get(),
                tmux_session_name = %prompt.tmux_session_name,
                notification_anchor_message_id = notification_anchor_message_id.get(),
                error = %error,
                "failed to post command-bot-owned synthetic placeholder; using the existing notification anchor"
            );
            crate::services::observability::emit_intake_placeholder_post_failed(
                &prompt.provider,
                channel_id.get(),
                None,
                "synthetic_anchor_before_lifecycle",
                "notification_anchor_fallback",
                &error,
            );
        }
    }
    anchor
}

pub(super) async fn establish_tui_direct_synthetic_lifecycle_anchor(
    shared: &Arc<SharedData>,
    command_http: Option<Arc<serenity::Http>>,
    channel_id: ChannelId,
    prompt: &ObservedTuiPrompt,
    notification_anchor_message_id: MessageId,
    relay_prompt_decision: &RelayObservedPromptInjectionDecision,
    lease_generation: u64,
) -> SyntheticLifecycleAnchor {
    let anchor = resolve_tui_direct_synthetic_lifecycle_anchor(
        shared,
        command_http,
        channel_id,
        prompt,
        notification_anchor_message_id,
        relay_prompt_decision,
    )
    .await;
    started(
        shared,
        channel_id,
        anchor.message_id,
        lease_generation,
        "tui_anchor_start",
    )
    .await;
    crate::services::tui_prompt_dedupe::record_prompt_anchor(
        &prompt.provider,
        &prompt.tmux_session_name,
        channel_id.get(),
        anchor.message_id.get(),
    );
    anchor
}

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
    anchor_is_owned_placeholder: bool,
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
            if !claim.claimed {
                delete_failed_synthetic_owned_placeholder(
                    shared,
                    &provider,
                    channel_id,
                    anchor_message_id,
                    anchor_is_owned_placeholder,
                )
                .await;
            }
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
