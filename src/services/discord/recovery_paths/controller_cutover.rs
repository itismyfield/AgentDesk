//! #3089 A6a: route the recovery engine's anchored short-replace delivery
//! through the unified turn-output controller (`deliver_turn_output`), behind a
//! flag (default OFF). This is an A3-clone — recovery's anchored relay, like
//! standby, is TRANSPORT-ONLY: it never held a `DeliveryLeaseCell`, never
//! advanced an offset, and ran no heartbeat — so the controller is driven with
//! [`toc::NoLease`] (acquire always fails → `ProceedMarkerless`: no lease held,
//! `heartbeat = None`) and `advance = None` (no offset authority → the confirmed
//! transport unconditionally counts as advanced → `Delivered`).
//!
//! Frozen-file LoC strategy: `recovery_engine.rs` is at its 4090 baseline with
//! near-zero headroom, so the cutover body lives HERE (a sub-1000-prod-LoC
//! sibling, ratchet-free) and the only in-file edit is the gate in the `Some`
//! arm of `relay_recovered_terminal_text_to_placeholder`. The deferred branches
//! (`placeholder == None` TUI-direct fresh-send, and the eventual long-chunk
//! split) stay byte-identically legacy.
//!
//! ## The recovery wrinkle the standby clone does NOT have
//!
//! Standby's delivery returns a bare `bool`; recovery returns the 3-state
//! [`RecoveryRelayOutcome`] (`Delivered` / `PermanentFailure` / `TransientFailure`)
//! and, on a transient-looking failure, runs a post-failure channel-liveness
//! PROBE (#3297 finding 2) — the real permanent detector, because the anchored
//! `replace_long_message_raw` path flattens its error chain into `String`s so the
//! typed 404/403/410 classification is unreachable there. The controller cannot
//! model that probe, so this adapter maps `DeliveryOutcome` → `RecoveryRelayOutcome`
//! and, for ANY non-`Delivered` controller verdict, re-runs the LEGACY
//! [`escalate_transient_relay_outcome_with_probe`] with the SAME
//! [`probe_channel_liveness`] closure the legacy `Err` arm used — so a
//! permanently-gone channel still escalates `TransientFailure → PermanentFailure`
//! exactly as legacy. Recovery uses `CommitOnFallback`, so it never observes
//! `Unknown { fell_back: true }` — `fell_back` is ignored (`..`).
//!
//! ## Policy mapping (mirrors A3 standby; transport-only)
//! - `NoLease` + `AcquireFailureMode::ProceedMarkerless` + `advance: None` +
//!   `heartbeat: None` — no lease/heartbeat/double-acquire (transport-only).
//! - [`EditFailPlaceholderPolicy::PreserveAlways`] — #2757: the original
//!   placeholder is NEVER deleted on edit-fail fallback. This matches the legacy
//!   recovery path: `replace_long_message_raw` maps BOTH `EditedOriginal` and
//!   `SentFallbackAfterEditFailure` to `Ok(())` and never deletes
//!   (`formatting.rs` `replace_long_message_outcome_to_result`).
//! - [`FallbackCommitPolicy::CommitOnFallback`] — legacy `Ok(())` on fallback ⇒
//!   `Delivered`. (Flipping to `NoCommitOnFallback` would make a fallback
//!   `Unknown` → non-delivered → wrong; pinned by a test.)
//! - `plan = Replace { Active }` → non-terminal → `post_send_finalize` no-ops
//!   (the replace IS the edit), so `I1`/`I2` are controller-owned.
//!
//! `forget_completion_footer_for_recovery_takeover` stays in the legacy wrapper
//! BEFORE this call — the controller has no footer-forget hook, and the
//! pre-send ordering must be preserved. The dispatch-flow `continue` gate
//! `should_advance_recovery_dispatch_after_relay` is NOT an offset commit and is
//! left untouched. #3016/#3419 (inflight finalizer reregister) and #3418 D1
//! (`recovery_text.rs`) are not delivery surfaces and are untouched.

use std::sync::Arc;
use std::sync::OnceLock;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId};

use super::super::SharedData;
use super::super::gateway::TurnGateway;
use super::super::inflight::RelayOwnerKind;
use super::super::outbound::turn_output_controller as toc;
use super::super::placeholder_controller::{PlaceholderKey, PlaceholderLifecycle};
use super::restart::probe_channel_liveness;
use super::shared::{RecoveryRelayOutcome, escalate_transient_relay_outcome_with_probe};
use crate::services::provider::ProviderKind;

/// #3089 A6a: flag gating ONLY the recovery anchored short-replace branch onto
/// the turn-output controller (`deliver_turn_output`). Default OFF → the legacy
/// `formatting::replace_long_message_raw` path runs byte-identically (including
/// the #3297 probe escalation); ON → that branch routes through the controller
/// as a transport-only `ProceedMarkerless` delivery (recovery holds no lease,
/// advances no offset, runs no heartbeat — see [`toc::NoLease`]). OnceLock+env,
/// mirroring `standby_relay_controller_enabled`. The `None` (TUI-direct
/// fresh-send) branch stays legacy.
pub(in crate::services::discord) fn recovery_relay_controller_enabled() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let on = std::env::var("AGENTDESK_RECOVERY_RELAY_CONTROLLER")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .is_some_and(|v| v == "1" || v == "true");
        // Telemetry ONLY when ENABLED — the default-OFF first evaluation must
        // have NO observable side effect (byte-identical / deploy no-op),
        // matching the A3 standby cutover.
        if on {
            tracing::info!("  ✓ recovery_relay_controller: enabled");
        }
        on
    })
}

/// #3089 A6a: pure short-replace cut-over decision. Routes the recovery anchored
/// short-replace branch onto the unified controller IFF the flag is ON **and**
/// an anchored placeholder exists **and** the body is non-empty.
///
/// Each exclusion is LOAD-BEARING (a controller divergence the gate guards):
/// - `enabled` OFF → byte-identical legacy (deploy no-op).
/// - `has_placeholder` false (`placeholder == None`) → the legacy `None` arm
///   sends a NEW message via `send_long_message_raw`; the controller's
///   `Replace { Active }` plan edits the anchor `MessageId` in place. With no
///   anchor there is nothing to edit, so the `None` branch MUST stay legacy
///   (TUI-direct fresh-send, deferred). Routing it through the controller would
///   wrongly attempt an edit of a non-existent message.
/// - `!body.is_empty()` → legacy `replace_long_message_raw` treats a zero-chunk
///   (empty) body as `EditedOriginal` → `Ok(())` → `Delivered` (no network),
///   whereas the controller short-circuits an empty body to `Skipped` →
///   non-delivered. Dropping this half would flip an empty body
///   `Delivered → TransientFailure`. Pinned by the truth-table test.
pub(in crate::services::discord) fn recovery_short_replace_should_cutover(
    enabled: bool,
    has_placeholder: bool,
    body: &str,
) -> bool {
    enabled && has_placeholder && !body.is_empty()
}

/// #3089 A6a: deliver the recovered terminal text via the unified controller,
/// returning the 3-state [`RecoveryRelayOutcome`] the recovery flow consumes.
///
/// Behaviourally equal to the legacy anchored path
/// (`replace_long_message_raw` + `classify_recovery_relay_error` +
/// `escalate_transient_relay_outcome_with_probe(.., probe_channel_liveness)`):
///
/// | controller `DeliveryOutcome`        | legacy equivalent            | mapped to |
/// |-------------------------------------|------------------------------|-----------|
/// | `Delivered` (EditedOriginal)        | `Ok(())` → `Delivered`       | `Delivered` |
/// | `Delivered` (CommitOnFallback)      | `Ok(())` (fallback) → `Delivered` | `Delivered` |
/// | `Unknown` (PartialContinuation)     | `Err` → classified + probe   | probe re-run |
/// | `Unknown` (transport Err)           | `Err` → classified + probe   | probe re-run |
/// | `Transient` / `NotDelivered` / `Skipped` | (cannot arise here)     | probe re-run |
///
/// For ANY non-`Delivered` verdict the LEGACY probe escalation is re-run with the
/// SAME [`probe_channel_liveness`] closure: a permanently-gone channel
/// (`ChannelProbeVerdict::Gone`) still becomes `PermanentFailure`, preserving the
/// #3297 permanent detector; otherwise it stays `TransientFailure` (retryable).
/// The probe (not the typed controller error) is the real permanent detector,
/// so it is preserved verbatim. `http` is threaded so the probe runs the live
/// `get_channel`; tests inject the probe verdict directly via the seam.
///
/// `gateway` is a seam: the live path builds the real `DiscordGateway`; tests
/// inject a fake driving the REAL controller. `Replace { Active }` keeps
/// `post_send_finalize` a no-op (the replace IS the edit); `NoLease` means no
/// lease/heartbeat/double-acquire (transport-only).
pub(in crate::services::discord) async fn deliver_recovery_replace_via_controller<G>(
    gateway: &G,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    http: &Arc<serenity::Http>,
    channel_id: ChannelId,
    placeholder: MessageId,
    body: &str,
) -> RecoveryRelayOutcome
where
    G: TurnGateway + ?Sized,
{
    let outcome = deliver_recovery_replace_via_controller_with_probe(
        gateway,
        shared,
        provider,
        channel_id,
        placeholder,
        body,
        || probe_channel_liveness(http, channel_id),
    )
    .await;
    let ts = chrono::Local::now().format("%H:%M:%S");
    match outcome {
        RecoveryRelayOutcome::Delivered => tracing::info!(
            "  [{ts}] ♻ recovery_engine ✓ delivered terminal response via controller channel {} msg {} (#3089 A6a)",
            channel_id.get(),
            placeholder.get()
        ),
        RecoveryRelayOutcome::PermanentFailure => tracing::warn!(
            "  [{ts}] ⚠ recovery_engine controller delivery permanently failed (channel gone) channel {} msg {} (#3089 A6a)",
            channel_id.get(),
            placeholder.get()
        ),
        RecoveryRelayOutcome::TransientFailure => tracing::warn!(
            "  [{ts}] ⚠ recovery_engine controller delivery not confirmed (transient) channel {} msg {} (#3089 A6a)",
            channel_id.get(),
            placeholder.get()
        ),
    }
    outcome
}

/// Probe-injectable core of [`deliver_recovery_replace_via_controller`]. The live
/// wrapper passes `|| probe_channel_liveness(http, channel_id)`; tests pass a
/// closure yielding a fixed [`super::shared::ChannelProbeVerdict`] so the
/// `DeliveryOutcome → RecoveryRelayOutcome` map (incl. the probe escalation) is
/// exercised through the REAL controller without a live Discord client.
async fn deliver_recovery_replace_via_controller_with_probe<G, F, Fut>(
    gateway: &G,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    placeholder: MessageId,
    body: &str,
    probe: F,
) -> RecoveryRelayOutcome
where
    G: TurnGateway + ?Sized,
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = super::shared::ChannelProbeVerdict>,
{
    // Recovery has no turn binding here; the `TurnKey` is COSMETIC on the
    // markerless + `NoLease` path (acquire always fails ⇒ the lease/turn
    // identity never gates anything). `user_msg_id = 0` is a defensible
    // degenerate value (mirrors A3 standby).
    let turn = super::super::turn_finalizer::TurnKey::new(
        channel_id,
        0,
        shared.restart.current_generation,
    );
    let no_lease = toc::NoLease;
    let delivery = toc::deliver_turn_output(
        gateway,
        toc::TurnOutputCtx {
            turn,
            // No `Recovery` owner variant exists and the enum lives in a frozen
            // baseline; `StandbyRelay` is cosmetic on the markerless path (no
            // owner-scoped routing fires), so it is reused as the closest
            // transport-only relay owner.
            owner: RelayOwnerKind::StandbyRelay,
            // Cosmetic on the markerless path (acquire always fails ⇒ no marker).
            holder: super::super::LeaseHolder::Sink,
            lease: &no_lease,
            channel_id,
            placeholder_controller: &shared.ui.placeholder_controller,
            placeholder: toc::PlaceholderSlot::Active {
                message_id: placeholder,
                key: PlaceholderKey {
                    provider: provider.clone(),
                    channel_id,
                    message_id: placeholder,
                },
            },
            body,
            // Recovery has no offsets and never commits (NoLease) — inert range.
            send_range: (0, 0),
            // `Replace { Active }` → non-terminal → `post_send_finalize` no-ops,
            // matching the legacy edit-in-place `replace_long_message_raw`.
            plan: toc::OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Active,
            },
            // #2757: never delete the original on edit-fail fallback (legacy
            // recovery `replace_long_message_raw` never deletes).
            edit_fail_policy: toc::EditFailPlaceholderPolicy::PreserveAlways,
            // Legacy maps `SentFallbackAfterEditFailure` → `Ok(())` → Delivered.
            fallback_commit_policy: toc::FallbackCommitPolicy::CommitOnFallback,
            // Transport-only: a (failed) acquire still POSTs, markerless.
            acquire_failure_mode: toc::AcquireFailureMode::ProceedMarkerless,
            // No offset authority → unconditional advance (A1 semantics).
            advance: None,
            // No heartbeat (no lease to renew).
            heartbeat: None,
        },
    )
    .await;

    match delivery {
        // Confirmed POST (EditedOriginal OR #2757 CommitOnFallback fallback): the
        // controller ran the (no-op markerless) commit; legacy returned `Ok(())`
        // → `Delivered` for both. `fell_back` is ignored — CommitOnFallback never
        // yields `Unknown { fell_back: true }`.
        toc::DeliveryOutcome::Delivered { .. } => RecoveryRelayOutcome::Delivered,
        // Anything else (`Unknown` from PartialContinuation/transport Err, or the
        // dormant `Transient`/`NotDelivered`/`Skipped`) is the legacy `Err` arm:
        // re-run the LEGACY probe escalation so a permanently-gone channel still
        // escalates `TransientFailure → PermanentFailure` (#3297), else stays
        // retryable. This preserves the probe-as-permanent-detector verbatim.
        _ => {
            escalate_transient_relay_outcome_with_probe(
                RecoveryRelayOutcome::TransientFailure,
                probe,
            )
            .await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
    use crate::services::discord::make_shared_data_for_tests;
    use crate::services::discord::recovery_paths::shared::ChannelProbeVerdict;
    use crate::services::provider::ProviderKind;
    use poise::serenity_prelude::{ChannelId, MessageId};
    use std::sync::atomic::{AtomicUsize, Ordering};

    // ---- pure predicate truth table -------------------------------------

    #[test]
    fn should_cutover_truth_table() {
        // (false, *, *) → false (flag OFF defers, byte-identical legacy).
        assert!(!recovery_short_replace_should_cutover(false, true, "x"));
        assert!(!recovery_short_replace_should_cutover(false, false, ""));
        // (true, false, _) → false: None defers to the legacy fresh-send branch.
        assert!(!recovery_short_replace_should_cutover(true, false, "x"));
        // (true, true, "") → false: empty body defers (legacy zero-chunk →
        // EditedOriginal → Delivered; controller → Skipped → not delivered).
        assert!(!recovery_short_replace_should_cutover(true, true, ""));
        // (true, true, "x") → true: the cutover row.
        assert!(recovery_short_replace_should_cutover(true, true, "x"));
    }

    #[test]
    fn should_cutover_pins_each_condition() {
        // Mutation guards: dropping `has_placeholder` flips the None row;
        // dropping `!body.is_empty()` flips the empty row; both true→false
        // under their respective mutations.
        assert!(
            !recovery_short_replace_should_cutover(true, false, "x"),
            "has_placeholder is load-bearing: None must defer"
        );
        assert!(
            !recovery_short_replace_should_cutover(true, true, ""),
            "!body.is_empty() is load-bearing: empty must defer"
        );
        assert!(recovery_short_replace_should_cutover(true, true, "x"));
    }

    // ---- controller adapter (fake gateway driving the REAL controller) --

    // Minimal `TurnGateway` fake for the recovery short-replace controller path.
    // Only `replace_message_with_outcome` is exercised (`Replace { Active }`
    // transport); the non-terminal `Active` lifecycle makes `post_send_finalize`
    // a no-op, so no edit/delete fires. `delete_message` records calls so a #2757
    // regression (fallback delete) is caught; every other method `panic!`s.
    struct RecoveryFakeGateway {
        outcome: ReplaceLongMessageOutcome,
        ok: bool,
        replace_calls: AtomicUsize,
        delete_calls: AtomicUsize,
    }

    impl RecoveryFakeGateway {
        fn new(outcome: ReplaceLongMessageOutcome, ok: bool) -> Self {
            Self {
                outcome,
                ok,
                replace_calls: AtomicUsize::new(0),
                delete_calls: AtomicUsize::new(0),
            }
        }
    }

    impl TurnGateway for RecoveryFakeGateway {
        fn replace_message_with_outcome<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async move {
                self.replace_calls.fetch_add(1, Ordering::SeqCst);
                if self.ok {
                    Ok(self.outcome.clone())
                } else {
                    Err("fake transport failure".to_string())
                }
            })
        }
        fn delete_message<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            // #2757: the recovery short-replace path must NEVER delete the
            // original. Record (and still succeed) so a fallback-delete mutation
            // is caught by the `delete_calls == 0` assertions.
            self.delete_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(()) })
        }
        fn send_message<'a>(
            &'a self,
            _c: ChannelId,
            _x: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            panic!("recovery short-replace path never sends a new message")
        }
        fn edit_message<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _x: &'a str,
        ) -> GatewayFuture<'a, Result<(), String>> {
            panic!("Active lifecycle → post_send_finalize no-op → no edit")
        }
        fn add_reaction<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _e: char,
        ) -> GatewayFuture<'a, ()> {
            panic!("unused TurnGateway method on the recovery short-replace path")
        }
        fn remove_reaction<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _e: char,
        ) -> GatewayFuture<'a, ()> {
            panic!("unused TurnGateway method on the recovery short-replace path")
        }
        fn schedule_retry_with_history<'a>(
            &'a self,
            _c: ChannelId,
            _u: MessageId,
            _t: &'a str,
        ) -> GatewayFuture<'a, ()> {
            panic!("unused TurnGateway method on the recovery short-replace path")
        }
        fn dispatch_queued_turn<'a>(
            &'a self,
            _c: ChannelId,
            _i: &'a crate::services::discord::Intervention,
            _o: &'a str,
            _h: bool,
        ) -> GatewayFuture<'a, Result<(), String>> {
            panic!("unused TurnGateway method on the recovery short-replace path")
        }
        fn validate_live_routing<'a>(
            &'a self,
            _c: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            panic!("unused TurnGateway method on the recovery short-replace path")
        }
        fn requester_mention(&self) -> Option<String> {
            None
        }
        fn can_chain_locally(&self) -> bool {
            false
        }
        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            None
        }
    }

    fn run(
        outcome: ReplaceLongMessageOutcome,
        ok: bool,
        probe: ChannelProbeVerdict,
    ) -> (RecoveryRelayOutcome, usize, usize) {
        let shared = make_shared_data_for_tests();
        let provider = ProviderKind::Claude;
        let channel = ChannelId::new(9_061);
        let gateway = RecoveryFakeGateway::new(outcome, ok);
        let result =
            futures::executor::block_on(deliver_recovery_replace_via_controller_with_probe(
                &gateway,
                &shared,
                &provider,
                channel,
                MessageId::new(77),
                "answer",
                || async move { probe },
            ));
        (
            result,
            gateway.replace_calls.load(Ordering::SeqCst),
            gateway.delete_calls.load(Ordering::SeqCst),
        )
    }

    #[test]
    fn edited_original_maps_to_delivered() {
        // EditedOriginal → controller Delivered → RecoveryRelayOutcome::Delivered,
        // original NOT deleted. Probe must NOT run on the Delivered arm.
        let (outcome, replace_calls, delete_calls) = run(
            ReplaceLongMessageOutcome::EditedOriginal,
            true,
            ChannelProbeVerdict::Gone,
        );
        assert_eq!(outcome, RecoveryRelayOutcome::Delivered);
        assert!(outcome.delivered(), ".delivered() == true");
        assert_eq!(replace_calls, 1, "exactly one transport POST");
        assert_eq!(delete_calls, 0, "the original placeholder is never deleted");
    }

    #[test]
    fn sent_fallback_maps_to_delivered_commit_on_fallback() {
        // #2757 PreserveAlways + CommitOnFallback: SentFallbackAfterEditFailure →
        // controller Delivered → RecoveryRelayOutcome::Delivered, original NOT
        // deleted. Flipping FallbackCommitPolicy to NoCommitOnFallback makes this
        // a non-Delivered Unknown → probe → not Delivered → this test fails.
        let (outcome, replace_calls, delete_calls) = run(
            ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                edit_error: "edit failed".to_string(),
            },
            true,
            // Probe verdict is irrelevant on the Delivered arm; supply Gone to
            // also prove the probe is NOT consulted when Delivered.
            ChannelProbeVerdict::Gone,
        );
        assert_eq!(
            outcome,
            RecoveryRelayOutcome::Delivered,
            "CommitOnFallback → Delivered (legacy Ok(()) parity)"
        );
        assert_eq!(replace_calls, 1, "exactly one transport POST");
        assert_eq!(
            delete_calls, 0,
            "#2757: PreserveAlways must never delete the original on fallback"
        );
    }

    #[test]
    fn partial_failure_runs_probe_transient() {
        // PartialContinuationFailure → controller Unknown → probe re-run; an
        // Inconclusive probe keeps the conservative TransientFailure (retryable).
        let (outcome, replace_calls, _delete) = run(
            ReplaceLongMessageOutcome::PartialContinuationFailure {
                sent_chunks: 1,
                total_chunks: 2,
                failed_chunk_index: 1,
                sent_continuation_message_ids: vec![1],
                cleanup_errors: vec![],
                error: "mid-stream".to_string(),
            },
            true,
            ChannelProbeVerdict::Inconclusive,
        );
        assert_eq!(outcome, RecoveryRelayOutcome::TransientFailure);
        assert!(!outcome.delivered(), ".delivered() == false");
        assert_eq!(replace_calls, 1, "exactly one transport POST attempt");
    }

    #[test]
    fn non_delivered_gone_probe_escalates_permanent() {
        // #3297: controller Unknown (transport Err here) + an authoritative Gone
        // probe escalates TransientFailure → PermanentFailure. A mutation that
        // maps non-Delivered → TransientFailure ALWAYS (skipping the probe re-run)
        // makes this Gone→Permanent assertion fail.
        let (outcome, replace_calls, _delete) = run(
            ReplaceLongMessageOutcome::EditedOriginal, // ignored: ok=false → Err
            false,
            ChannelProbeVerdict::Gone,
        );
        assert_eq!(
            outcome,
            RecoveryRelayOutcome::PermanentFailure,
            "Gone probe escalates the transient controller failure to Permanent (#3297)"
        );
        assert!(!outcome.delivered());
        assert_eq!(replace_calls, 1, "the single POST was attempted and failed");
    }

    #[test]
    fn flag_off_predicate_defers_no_side_effect() {
        // Predicate OFF → the production gate never calls the controller adapter,
        // so the legacy path runs with no telemetry side-effect.
        assert!(!recovery_short_replace_should_cutover(false, true, "x"));
        // Default OFF when the env var is unset (deploy no-op). Only assert when
        // truly unset, since `OnceLock` caches the first observation and the
        // flag-ON gate run sets `AGENTDESK_RECOVERY_RELAY_CONTROLLER=1`.
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if std::env::var_os("AGENTDESK_RECOVERY_RELAY_CONTROLLER").is_none() {
            assert!(
                !recovery_relay_controller_enabled(),
                "flag defaults OFF (deploy no-op / byte-identical legacy)"
            );
        }
    }
}
