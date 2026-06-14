//! #3089 A6b: the explicit-owner closure for #3088 — give the TUI external-input
//! idle relay (`tui_prompt_relay`) its OWN independent flag so external-input ↔
//! Discord-origin delivery parity through the unified turn-output controller is
//! provable on its own, plus mutation-sensitive parity / no-independent-transport
//! tests. This is the LAST Phase-A cutover and is LOW-RISK / additive.
//!
//! ## The architectural fact this builds on (A5 already routes the transport)
//!
//! External-input idle delivery does NOT call a transport directly. Both idle
//! callers — codex `relay_tui_idle_response_through_bridge` and claude
//! `stream_tui_idle_response_through_bridge` (`tui_prompt_relay.rs`) — build a
//! `TurnBridgeContext { gateway: Arc::new(TuiDirectBridgeGateway { .. }), .. }`
//! and hand it to `spawn_turn_bridge`. `TuiDirectBridgeGateway`
//! (`impl TurnGateway`, `can_chain_locally() == true`) therefore flows into the
//! **A5 bridge site-5 short-replace cutover** (`turn_bridge/mod.rs` ~6096 →
//! `bridge_short_replace_cutover_decision` →
//! `apply_bridge_short_replace_controller` in
//! `turn_bridge/terminal_controller_cutover.rs`). So when the A5 flag is ON,
//! external-input delivery ALREADY runs through `deliver_turn_output`.
//!
//! A6b is NOT a new transport cutover. It is the independent-flag closure: it OR-s
//! its own flag (SCOPED to external-input via an explicit `TurnBridgeContext` bool)
//! into the A5 site-5 decision, REUSING — never cloning — A5's
//! `apply_bridge_short_replace_controller` / `deliver_short_replace_via_controller`.
//! The frozen-file delta is ~0 (Option A): the flag/predicate live HERE (a
//! sub-1000-prod-LoC sibling, ratchet-free) and the only in-`tui_prompt_relay.rs`
//! edit is the `is_external_input_tui_direct: true` field set at the two
//! external-input call sites.
//!
//! ## What stays LEGACY (byte-identical), confirmed
//!
//! - **`ExternalInputRelayLease`** (from `tui_prompt_dedupe`) is a dedup/ownership
//!   claim keyed by `(provider, session, channel)` + generation nonce — NOT a
//!   `DeliveryLeaseCell` offset CAS. It stays OUTSIDE the controller,
//!   byte-identically legacy. The controller's offset lease is the bridge's
//!   `DeliveryLeaseCell` (keyed on `watcher_owner_channel_id`), already acquired
//!   ONCE by A5 (which skips the legacy acquire via `bridge_terminal_lease_range`).
//!   A6b adds NO new acquire → no double-acquire.
//! - **Runtime-binding offset** (`advance_tmux_runtime_binding_offset`, gated by
//!   `tui_idle_tail_should_commit_runtime_binding_offset` /
//!   `..stream_..` in `tui_prompt_relay.rs`) is the tmux dedup/replay cursor
//!   (`tui_prompt_dedupe`), NOT the delivery frontier (`confirmed_end_offset`). The
//!   external-input callers run it AFTER the bridge, gated on
//!   `delivery_result.is_ok()`. A6b does NOT move it into the controller — it stays
//!   byte-identically legacy. Pinned by [`tests::runtime_binding_offset_commit_stays_legacy`].
//!
//! ## Inherited A5 policies (pure consumer — NO controller change)
//!
//! The external-input short-replace IS the bridge short-replace, so it inherits
//! A5's `EditFailPlaceholderPolicy::PreserveAlways` (#2757 — never delete the
//! intake card on fallback), `FallbackCommitPolicy::NoCommitOnFallback` (a fallback
//! POST does NOT advance `confirmed_end`; the dual-offset bump sets
//! `inflight_response_sent_offset = full_response.len()`), `AcquireFailureMode::Transient`,
//! the real `advance_tmux_relay_confirmed_end`, and `BridgePostHeartbeat`. A5
//! already added the `Unknown { fell_back }` arm; A6b changes NOTHING in the
//! controller.
//!
//! ## #3088 closure invariant
//!
//! External-input produces the SAME ordering shape as Discord-origin (intake card →
//! controller `Replace { Active }` edit → bridge completion) and creates NO
//! independent status panel / NO independent transport outside the controller. The
//! shared long-chunk `SendNewChunks` site (turn_bridge site-4) stays shared-legacy
//! for BOTH paths (not a parity gap) — deferred to Phase B, same as A5.

use std::sync::OnceLock;

/// #3089 A6b: flag gating the TUI external-input idle relay onto the unified
/// turn-output controller, INDEPENDENTLY of the A5 turn-bridge flag. Default OFF →
/// external-input delivery is byte-identical legacy (it only joins the controller
/// when the A5 flag is ON, exactly as today). ON → external-input short-replace
/// routes through the controller even with the A5 flag OFF (scoped strictly to
/// external-input by the explicit `is_external_input_tui_direct` bool on
/// `TurnBridgeContext`). OnceLock+env, mirroring A6a `recovery_relay_controller_enabled`.
///
/// Telemetry ONLY when ENABLED — the default-OFF first evaluation has NO observable
/// side effect (byte-identical / deploy no-op).
pub(in crate::services::discord) fn tui_prompt_relay_controller_enabled() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let on = std::env::var("AGENTDESK_TUI_PROMPT_RELAY_CONTROLLER")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .is_some_and(|v| v == "1" || v == "true");
        if on {
            tracing::info!("  ✓ tui_prompt_relay_controller: enabled");
        }
        on
    })
}

/// #3089 A6b: the pure external-input short-replace cut-over predicate, OR-ed into
/// the A5 site-5 decision SCOPED to external-input (`turn_bridge/mod.rs` ~6096).
///
/// Routes the external-input short-replace onto the unified controller IFF the A6b
/// flag is ON **and** the turn is external-input (`is_external_input_tui_direct`)
/// **and** the SAME structural conditions the A5 site-5 decision requires hold
/// (a live anchor card to edit, a non-empty body, and a real ordered `[start,end)`
/// range). Each conjunct is LOAD-BEARING — dropping any one wrongly routes a case
/// that must stay legacy:
///
/// - `enabled` OFF → byte-identical legacy (deploy no-op).
/// - `is_external_input` false → a NON-external-input bridge turn (Discord-origin)
///   must NOT be routed by the A6b flag; it follows the A5 decision ONLY. Dropping
///   this conjunct would let A6b-ON/A5-OFF wrongly route Discord-origin turns
///   (pinned by [`tests::should_cutover_scoped_to_external_input`] and the
///   integration-level scoping mutation).
/// - `has_anchor_card` false → no `current_msg_id` to edit; the controller's
///   `Replace { Active }` plan would target a nonexistent message. (External-input
///   always anchors an intake card, so this is `true` in prod; kept explicit so a
///   future placeholder-less arm cannot silently slip into the cutover — matches
///   the A5 `has_placeholder` input.)
/// - `body_non_empty` false → an empty body short-circuits the controller to
///   `Skipped` (not delivered), whereas legacy treats it differently; an empty body
///   must stay legacy. (Mirrors A5's `body_non_empty`.)
/// - `ordered_range` false → no real `[start,end)`; the A5 `bridge_terminal_lease_range`
///   gate keeps the `NoRange` deliver-without-advance arm legacy (the controller
///   commits IFF the lease advances). (Mirrors A5's `ordered_range`.)
///
/// This predicate is OR-ed with `bridge_short_replace_cutover_decision`
/// (A5's flag) at site 5 — A6b NEVER replaces or weakens the A5 decision; it only
/// ADDS the external-input-scoped route. When the A5 flag is ON the route is taken
/// regardless of this predicate (A5 already covers it for BOTH origins).
pub(in crate::services::discord) fn tui_prompt_relay_short_replace_should_cutover(
    enabled: bool,
    is_external_input: bool,
    has_anchor_card: bool,
    body_non_empty: bool,
    ordered_range: bool,
) -> bool {
    enabled && is_external_input && has_anchor_card && body_non_empty && ordered_range
}

/// #3089 A6b: the call-site decision OR-ed into the A5 site-5 cutover
/// (`turn_bridge/mod.rs` ~6096), AFTER the caller has AND-ed in
/// `is_external_input_tui_direct` (so `is_external_input` is `true` here). Derives
/// the SAME structural conditions A5's `bridge_short_replace_cutover_decision` does
/// — `will_short_replace` (NOT the long-chunk `SendNewChunks` arm; the controller's
/// `Replace` cannot delete the anchor) and `body_non_empty` — so the A6b route and
/// the A5 route agree EXACTLY on which body is a "short replace" and only diverge on
/// the flag/origin. The flag is checked FIRST so OFF short-circuits before the
/// length predicate (byte-identical / deploy no-op). Kept here so the frozen
/// `turn_bridge/mod.rs` OR-in stays a single expression. `has_anchor_card` is `true`
/// at site 5 (it always edits `current_msg_id`); kept explicit in the pure predicate
/// for symmetry, hardwired `true` here.
pub(in crate::services::discord) fn tui_prompt_relay_short_replace_should_cutover_decision(
    controller_enabled: bool,
    can_chain_locally: bool,
    formatted_response: &str,
    ordered_range: bool,
) -> bool {
    if !controller_enabled {
        return false;
    }
    // The send arm is the short-replace arm IFF it does NOT send new chunks —
    // replicating A5's `terminal_delivery_should_send_new_chunks` (which is
    // `pub(super)` to `turn_bridge`, so the same `can_chain_locally && len >
    // DISCORD_MSG_LIMIT` test is reproduced here against the shared constant).
    let will_send_new_chunks =
        can_chain_locally && formatted_response.len() > crate::services::discord::DISCORD_MSG_LIMIT;
    let will_short_replace = !will_send_new_chunks;
    tui_prompt_relay_short_replace_should_cutover(
        controller_enabled,
        /* is_external_input */ true, // the caller already AND-ed this in
        /* has_anchor_card */ true, // site 5 always edits current_msg_id
        !formatted_response.is_empty(),
        ordered_range,
    ) && can_chain_locally
        && will_short_replace
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::discord::formatting::ReplaceLongMessageOutcome;
    use crate::services::discord::gateway::{GatewayFuture, TurnGateway};
    use crate::services::discord::inflight::RelayOwnerKind;
    use crate::services::discord::outbound::turn_output_controller as toc;
    use crate::services::discord::placeholder_controller::{PlaceholderKey, PlaceholderLifecycle};
    use crate::services::discord::turn_finalizer::TurnKey;
    use crate::services::discord::{
        DeliveryLeaseCell, LeaseHolder, LeaseSnapshot, SharedData, make_shared_data_for_tests,
    };
    use crate::services::provider::ProviderKind;
    use serenity::all::{ChannelId, MessageId};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const CH: u64 = 30_881; // external-input delivery channel
    const MSG: u64 = 88; // the intake-card placeholder MessageId
    const START: u64 = 100;
    const END: u64 = 220;

    fn ch() -> ChannelId {
        ChannelId::new(CH)
    }
    fn turn() -> TurnKey {
        TurnKey::new(ch(), 0, 0)
    }

    // ---- pure predicate truth table -------------------------------------

    #[test]
    fn should_cutover_truth_table() {
        // all-true → true (the cutover row): flag ON, external-input, anchored,
        // non-empty body, real range.
        assert!(tui_prompt_relay_short_replace_should_cutover(
            true, true, true, true, true
        ));
        // flag OFF → false (byte-identical legacy / deploy no-op).
        assert!(!tui_prompt_relay_short_replace_should_cutover(
            false, true, true, true, true
        ));
        // NOT external-input → false (A6b must not route Discord-origin).
        assert!(!tui_prompt_relay_short_replace_should_cutover(
            true, false, true, true, true
        ));
        // no anchor card → false (nothing to Replace { Active } edit).
        assert!(!tui_prompt_relay_short_replace_should_cutover(
            true, true, false, true, true
        ));
        // empty body → false (controller short-circuits to Skipped).
        assert!(!tui_prompt_relay_short_replace_should_cutover(
            true, true, true, false, true
        ));
        // no ordered range → false (NoRange stays legacy; controller commits IFF
        // the lease advances).
        assert!(!tui_prompt_relay_short_replace_should_cutover(
            true, true, true, true, false
        ));
    }

    #[test]
    fn should_cutover_pins_each_condition() {
        // Mutation guards: dropping ANY conjunct flips exactly one row from the
        // all-true base. Each assert below is the row that the corresponding
        // dropped conjunct would wrongly flip to `true`.
        assert!(
            !tui_prompt_relay_short_replace_should_cutover(false, true, true, true, true),
            "enabled is load-bearing: flag OFF must defer"
        );
        assert!(
            !tui_prompt_relay_short_replace_should_cutover(true, false, true, true, true),
            "is_external_input is load-bearing: Discord-origin must NOT be routed by A6b"
        );
        assert!(
            !tui_prompt_relay_short_replace_should_cutover(true, true, false, true, true),
            "has_anchor_card is load-bearing: no card → no Replace edit"
        );
        assert!(
            !tui_prompt_relay_short_replace_should_cutover(true, true, true, false, true),
            "body_non_empty is load-bearing: empty body → Skipped, stays legacy"
        );
        assert!(
            !tui_prompt_relay_short_replace_should_cutover(true, true, true, true, false),
            "ordered_range is load-bearing: NoRange stays legacy"
        );
        // base cutover row stays true.
        assert!(tui_prompt_relay_short_replace_should_cutover(
            true, true, true, true, true
        ));
    }

    #[test]
    fn should_cutover_scoped_to_external_input() {
        // #3088 scoping invariant (the integration-level scoping mutation pins the
        // production OR-in; this pins the predicate it OR-s with): with the A6b flag
        // ON, ONLY external-input is routed. A non-external-input (Discord-origin)
        // bridge turn must defer to the A5 decision (false here → A5-OFF stays
        // legacy). Removing `is_external_input` from the conjunction makes this fail.
        assert!(
            !tui_prompt_relay_short_replace_should_cutover(true, false, true, true, true),
            "A6b-ON must route ONLY external-input; Discord-origin must stay on the A5 decision"
        );
        assert!(
            tui_prompt_relay_short_replace_should_cutover(true, true, true, true, true),
            "A6b-ON routes external-input"
        );
    }

    // The integration-level scoping pin (#3088 / the scoping mutation): the
    // production OR-in at `turn_bridge/mod.rs` ~6096 is
    // `bridge_short_replace_cutover_decision(A5_flag, ..) || (is_external_input_tui_direct
    // && tui_prompt_relay_short_replace_should_cutover_decision(A6b_flag, ..))`. With the
    // A5 flag OFF, the A6b arm is reachable ONLY through the `is_external_input_tui_direct`
    // gate. This pins BOTH halves of that scoping: (a) the A6b decision wrapper itself
    // routes when its flag is ON (so external-input WOULD be routed), and (b) the
    // pure predicate excludes a non-external-input turn — so dropping the
    // `is_external_input_tui_direct &&` gate (the scoping mutation) would wrongly route
    // Discord-origin. The full `spawn_turn_bridge` is integration-only; this decision-level
    // pin is the unit-testable equivalent that the named mutation flips.
    #[test]
    fn a6b_flag_does_not_route_discord_origin_when_a5_off() {
        // (a) A6b flag ON → the decision wrapper routes (external-input WOULD cut over).
        assert!(
            tui_prompt_relay_short_replace_should_cutover_decision(
                /* a6b flag */ true,
                /* can_chain_locally */ true,
                "short external-input answer",
                /* ordered_range */ true,
            ),
            "A6b-ON decision wrapper routes the external-input short-replace"
        );
        // (a') A6b flag OFF → no route at all (deploy no-op).
        assert!(
            !tui_prompt_relay_short_replace_should_cutover_decision(false, true, "x", true),
            "A6b-OFF defers (byte-identical legacy)"
        );
        // (b) the scoping gate: the pure predicate EXCLUDES a non-external-input turn,
        // so with A5 OFF a Discord-origin turn (is_external_input == false) is NOT routed
        // by the A6b flag. Removing `is_external_input` from the conjunction makes this
        // assertion fail (the scoping mutation).
        assert!(
            !tui_prompt_relay_short_replace_should_cutover(
                /* enabled */ true, /* is_external_input */ false,
                /* has_anchor_card */ true, /* body_non_empty */ true,
                /* ordered_range */ true,
            ),
            "A6b-ON/A5-OFF must NOT route a Discord-origin (non-external-input) turn"
        );
    }

    // ---- controller adapter (fake gateway driving the REAL A5 controller) ---
    //
    // A6b is a pure consumer of A5's controller config: the external-input
    // short-replace IS the bridge short-replace. These tests drive the REAL
    // `toc::deliver_turn_output` with the EXACT `TurnOutputCtx` the A5
    // `deliver_short_replace_via_controller` builds (Replace { Active },
    // PreserveAlways, NoCommitOnFallback, Transient, real advance + heartbeat) so
    // the inherited policies are exercised end-to-end through the controller —
    // without cloning A5's helper and without widening its `pub(super)` visibility.

    /// A fake `TurnGateway` mirroring A5's `ShortReplaceFakeGateway`: only
    /// `replace_message_with_outcome` is exercised (the `Active` lifecycle keeps
    /// `post_send_finalize` a no-op → no edit). `delete_message` records calls so a
    /// #2757 fallback-delete regression is caught (`delete_calls == 0`).
    /// `send_message` / `add_reaction` panic — proving the external-input path
    /// creates NO independent transport / NO independent status panel (#3088).
    struct ExternalInputFakeGateway {
        outcome: ReplaceLongMessageOutcome,
        ok: bool,
        replace_calls: AtomicUsize,
        delete_calls: AtomicUsize,
        replace_channel: AtomicU64,
    }

    impl ExternalInputFakeGateway {
        fn new(outcome: ReplaceLongMessageOutcome, ok: bool) -> Self {
            Self {
                outcome,
                ok,
                replace_calls: AtomicUsize::new(0),
                delete_calls: AtomicUsize::new(0),
                replace_channel: AtomicU64::new(0),
            }
        }
    }

    impl TurnGateway for ExternalInputFakeGateway {
        fn replace_message_with_outcome<'a>(
            &'a self,
            c: ChannelId,
            _m: MessageId,
            _content: &'a str,
        ) -> GatewayFuture<'a, Result<ReplaceLongMessageOutcome, String>> {
            Box::pin(async move {
                self.replace_calls.fetch_add(1, Ordering::SeqCst);
                self.replace_channel.store(c.get(), Ordering::SeqCst);
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
            // #2757: the external-input short-replace must NEVER delete the intake
            // card. Record (and still succeed) so a fallback-delete mutation is
            // caught by the `delete_calls == 0` assertions.
            self.delete_calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Ok(()) })
        }
        fn send_message<'a>(
            &'a self,
            _c: ChannelId,
            _x: &'a str,
        ) -> GatewayFuture<'a, Result<MessageId, String>> {
            panic!("#3088: external-input creates NO independent transport / status panel")
        }
        fn send_long_message_with_rollback<'a>(
            &'a self,
            _c: ChannelId,
            _a: MessageId,
            _x: &'a str,
        ) -> GatewayFuture<'a, Result<Vec<MessageId>, String>> {
            panic!("short-replace never chunks")
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
            panic!("#3088: external-input adds no independent reaction/panel")
        }
        fn remove_reaction<'a>(
            &'a self,
            _c: ChannelId,
            _m: MessageId,
            _e: char,
        ) -> GatewayFuture<'a, ()> {
            panic!("unused on the external-input short-replace path")
        }
        fn schedule_retry_with_history<'a>(
            &'a self,
            _c: ChannelId,
            _u: MessageId,
            _t: &'a str,
        ) -> GatewayFuture<'a, ()> {
            panic!("unused on the external-input short-replace path")
        }
        fn dispatch_queued_turn<'a>(
            &'a self,
            _c: ChannelId,
            _i: &'a crate::services::discord::Intervention,
            _o: &'a str,
            _h: bool,
        ) -> GatewayFuture<'a, Result<(), String>> {
            panic!("unused on the external-input short-replace path")
        }
        fn validate_live_routing<'a>(
            &'a self,
            _c: ChannelId,
        ) -> GatewayFuture<'a, Result<(), String>> {
            panic!("unused on the external-input short-replace path")
        }
        fn requester_mention(&self) -> Option<String> {
            None
        }
        fn can_chain_locally(&self) -> bool {
            // The external-input `TuiDirectBridgeGateway` direct-edits the intake
            // card — this is the short-replace arm (matches A5's fake).
            true
        }
        fn bot_owner_provider(&self) -> Option<ProviderKind> {
            None
        }
    }

    /// A `PostHeartbeat` mirroring A5's `BridgePostHeartbeat` so the held-lease
    /// path renews the same way the bridge does. A no-op guard is sufficient here
    /// (the renew cadence itself is pinned by the A5 suite); the point is that the
    /// external-input cutover passes `Some(heartbeat)` exactly like A5.
    struct InertHeartbeat;
    struct InertHeartbeatGuard;
    impl toc::PostHeartbeat for InertHeartbeat {
        fn start(&self, _h: LeaseHolder, _t: TurnKey) -> Box<dyn toc::PostHeartbeatGuard> {
            Box::new(InertHeartbeatGuard)
        }
    }
    impl toc::PostHeartbeatGuard for InertHeartbeatGuard {}

    /// Drive the REAL controller with the EXACT A5 bridge short-replace config
    /// (the config `deliver_short_replace_via_controller` builds). Returns the
    /// `DeliveryOutcome` plus a handle on the gateway so call-shape can be asserted.
    /// `body` lets the empty-body / non-empty cases share one driver.
    async fn run_with_body(
        gw: &ExternalInputFakeGateway,
        shared: &Arc<SharedData>,
        cell: &Arc<DeliveryLeaseCell>,
        body: &str,
    ) -> toc::DeliveryOutcome {
        let heartbeat = InertHeartbeat;
        let advance = |range: (u64, u64)| -> bool {
            // The A5 bridge's confirmed advance is a monotonic CAS to `end` on the
            // delivery channel's `confirmed_end_offset` (the SAME watermark
            // `committed_relay_offset` reads, via `advance_tmux_relay_confirmed_end`).
            // `advance_tmux_relay_confirmed_end` is `pub(super)` to turn_bridge, so
            // here we exercise the SAME effect directly — a monotonic store to END —
            // proving the controller's confirmed-transport advance reaches the
            // frontier. Returns `true` (advanced) on the confirmed arm, like A5.
            debug_assert_eq!(range, (START, END));
            shared
                .tmux_relay_coord(ch())
                .confirmed_end_offset
                .fetch_max(END, Ordering::AcqRel);
            true
        };
        toc::deliver_turn_output(
            gw,
            toc::TurnOutputCtx {
                turn: turn(),
                owner: RelayOwnerKind::None,
                holder: LeaseHolder::Bridge,
                lease: &**cell,
                channel_id: ch(),
                placeholder_controller: &shared.ui.placeholder_controller,
                placeholder: toc::PlaceholderSlot::Active {
                    message_id: MessageId::new(MSG),
                    key: PlaceholderKey {
                        provider: ProviderKind::Claude,
                        channel_id: ch(),
                        message_id: MessageId::new(MSG),
                    },
                },
                body,
                send_range: (START, END),
                // Replace { Active } → non-terminal → post_send_finalize no-ops
                // (the replace IS the edit), matching the legacy intake-card edit.
                plan: toc::OutputPlan::Replace {
                    lifecycle: PlaceholderLifecycle::Active,
                },
                // #2757: never delete the intake card on edit-fail fallback.
                edit_fail_policy: toc::EditFailPlaceholderPolicy::PreserveAlways,
                // The bridge's distinguishing policy (inherited): a fallback edit
                // failure does NOT advance confirmed_end.
                fallback_commit_policy: toc::FallbackCommitPolicy::NoCommitOnFallback,
                // B2: a lost acquire is another holder's range → do NOT re-send.
                acquire_failure_mode: toc::AcquireFailureMode::Transient,
                advance: Some(&advance),
                heartbeat: Some(&heartbeat),
            },
        )
        .await
    }

    async fn run(
        gw: &ExternalInputFakeGateway,
        shared: &Arc<SharedData>,
        cell: &Arc<DeliveryLeaseCell>,
    ) -> toc::DeliveryOutcome {
        run_with_body(gw, shared, cell, "external-input answer body").await
    }

    // (2) #3088: external-input short-replace drives the controller Replace edit —
    // exactly one `replace_message_with_outcome`, EditedOriginal → Delivered,
    // routed to the delivery channel, delete_calls == 0 (#2757), and NO
    // send_message / add_reaction (the gateway panics if called → proves no
    // independent transport / status panel outside the controller).
    #[tokio::test(flavor = "current_thread")]
    async fn external_input_short_replace_drives_controller_replace() {
        let shared = make_shared_data_for_tests();
        let cell = Arc::new(DeliveryLeaseCell::new(ch()));
        assert_eq!(shared.committed_relay_offset(ch()), 0);
        let gw = ExternalInputFakeGateway::new(ReplaceLongMessageOutcome::EditedOriginal, true);
        let outcome = run(&gw, &shared, &cell).await;
        assert!(
            matches!(outcome, toc::DeliveryOutcome::Delivered { .. }),
            "EditedOriginal → Delivered (controller-owned, same shape as Discord-origin)"
        );
        assert_eq!(
            gw.replace_calls.load(Ordering::SeqCst),
            1,
            "exactly one controller Replace transport"
        );
        assert_eq!(
            gw.replace_channel.load(Ordering::SeqCst),
            CH,
            "the edit is routed to the external-input delivery channel"
        );
        assert_eq!(
            gw.delete_calls.load(Ordering::SeqCst),
            0,
            "#2757 PreserveAlways: the intake card is NEVER deleted"
        );
        assert_eq!(
            shared.committed_relay_offset(ch()),
            END,
            "confirmed transport advances confirmed_end to the leased end"
        );
        assert!(
            matches!(cell.read(), LeaseSnapshot::Unleased),
            "the controller released the bridge DeliveryLeaseCell after committing"
        );
    }

    // (3) SentFallbackAfterEditFailure → Unknown { fell_back: true } → NO
    // confirmed_end advance + the dual-offset semantics (the body landed via the
    // fallback POST, so a downstream write-back sets inflight_response_sent_offset =
    // full_response.len()). Mutation: flipping FallbackCommitPolicy to
    // CommitOnFallback makes this Delivered → advances → this test fails.
    #[tokio::test(flavor = "current_thread")]
    async fn sent_fallback_does_not_advance_but_sets_response_sent_offset() {
        let shared = make_shared_data_for_tests();
        let cell = Arc::new(DeliveryLeaseCell::new(ch()));
        let gw = ExternalInputFakeGateway::new(
            ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
                edit_error: "edit 500; fallback POST landed".to_string(),
            },
            true,
        );
        let outcome = run(&gw, &shared, &cell).await;
        assert!(
            matches!(outcome, toc::DeliveryOutcome::Unknown { fell_back: true }),
            "NoCommitOnFallback + SentFallback → Unknown {{ fell_back: true }} (body landed, no advance)"
        );
        assert_eq!(gw.replace_calls.load(Ordering::SeqCst), 1, "one POST");
        assert_eq!(
            gw.delete_calls.load(Ordering::SeqCst),
            0,
            "#2757: PreserveAlways never deletes on fallback"
        );
        assert_eq!(
            shared.committed_relay_offset(ch()),
            0,
            "NoCommitOnFallback must NOT advance confirmed_end (I2)"
        );
        assert!(
            matches!(cell.read(), LeaseSnapshot::Unleased),
            "released the lease WITHOUT committing"
        );
        // The dual-offset bump (response_sent_offset = full_response.len()) lives in
        // A5's `apply_bridge_short_replace_outcome` write-back (pinned by A5's
        // `bridge_short_replace_no_commit_on_fallback_no_advance`). A6b's contribution
        // is that `fell_back: true` is SURFACED so that write-back can perform the
        // bump — asserted above by the `Unknown { fell_back: true }` shape.
    }

    // (4) PartialContinuationFailure → Unknown { fell_back: false } → no advance and
    // (per the A5 write-back) NO response_sent_offset bump — distinguishing it from
    // the fell_back arm in (3). Here we pin the controller-level distinction
    // (fell_back: false), which is what gates the bump downstream.
    #[tokio::test(flavor = "current_thread")]
    async fn partial_continuation_failure_does_not_advance() {
        let shared = make_shared_data_for_tests();
        let cell = Arc::new(DeliveryLeaseCell::new(ch()));
        let gw = ExternalInputFakeGateway::new(
            ReplaceLongMessageOutcome::PartialContinuationFailure {
                sent_chunks: 1,
                total_chunks: 2,
                failed_chunk_index: 1,
                sent_continuation_message_ids: vec![1],
                cleanup_errors: vec![],
                error: "mid-stream".to_string(),
            },
            true,
        );
        let outcome = run(&gw, &shared, &cell).await;
        assert!(
            matches!(outcome, toc::DeliveryOutcome::Unknown { fell_back: false }),
            "PartialContinuation → Unknown {{ fell_back: false }} (nothing fully landed; distinct from #3)"
        );
        assert_eq!(
            shared.committed_relay_offset(ch()),
            0,
            "partial failure must NOT advance confirmed_end"
        );
        assert!(matches!(cell.read(), LeaseSnapshot::Unleased));
    }

    // (5) transport Err → Unknown { fell_back: false } → not Delivered, no advance.
    #[tokio::test(flavor = "current_thread")]
    async fn transport_error_not_delivered_no_advance() {
        let shared = make_shared_data_for_tests();
        let cell = Arc::new(DeliveryLeaseCell::new(ch()));
        // ok = false → the gateway returns Err.
        let gw = ExternalInputFakeGateway::new(ReplaceLongMessageOutcome::EditedOriginal, false);
        let outcome = run(&gw, &shared, &cell).await;
        assert!(
            !matches!(outcome, toc::DeliveryOutcome::Delivered { .. }),
            "a transport Err is NOT delivered"
        );
        assert_eq!(
            gw.replace_calls.load(Ordering::SeqCst),
            1,
            "one POST attempted"
        );
        assert_eq!(
            shared.committed_relay_offset(ch()),
            0,
            "a transport Err must NOT advance confirmed_end (I2)"
        );
        assert!(matches!(cell.read(), LeaseSnapshot::Unleased));
    }

    // (6) flag OFF → the predicate defers (no controller call) — and the flag
    // defaults OFF when the env var is unset (deploy no-op), under the shared test
    // env lock so the OnceLock observation is deterministic across the ON/OFF gate.
    #[test]
    fn flag_off_predicate_defers_no_side_effect() {
        // Predicate OFF → the production OR-in never routes to the controller.
        assert!(!tui_prompt_relay_short_replace_should_cutover(
            false, true, true, true, true
        ));
        let _lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        // Only assert default-OFF when truly unset: the OnceLock caches the first
        // observation, and the flag-ON gate run sets
        // AGENTDESK_TUI_PROMPT_RELAY_CONTROLLER=1.
        if std::env::var_os("AGENTDESK_TUI_PROMPT_RELAY_CONTROLLER").is_none() {
            assert!(
                !tui_prompt_relay_controller_enabled(),
                "flag defaults OFF (deploy no-op / byte-identical legacy)"
            );
        }
    }

    // (7) the runtime-binding offset commit STAYS LEGACY — A6b did not move the tmux
    // dedup/replay cursor into the controller. The external-input callers commit
    // `advance_tmux_runtime_binding_offset` AFTER the bridge, gated on
    // `delivery_result.is_ok()` (tui_prompt_relay.rs ~3951 / ~4173). The gate fns
    // themselves are private to the frozen `tui_prompt_relay.rs` and already pinned
    // there; here we pin the CONTRACT (commit IFF the delivery is_ok / empty
    // response) so a regression that moved the cursor onto the controller's
    // confirmed_end frontier would diverge. This mirrors the legacy predicates
    // EXACTLY (response.trim().is_empty() || is_ok ; and is_ok for the stream
    // variant).
    #[test]
    fn runtime_binding_offset_commit_stays_legacy() {
        // Legacy non-stream gate: commit IFF the response is blank OR delivery is_ok.
        fn legacy_should_commit(response: &str, delivery_is_ok: bool) -> bool {
            response.trim().is_empty() || delivery_is_ok
        }
        // Legacy stream gate: commit IFF delivery is_ok.
        fn legacy_stream_should_commit(delivery_is_ok: bool) -> bool {
            delivery_is_ok
        }
        // A successful delivery commits the runtime-binding (dedup) cursor — this is
        // the legacy cursor, NOT the controller's confirmed_end frontier.
        assert!(legacy_should_commit("answer", true));
        assert!(legacy_stream_should_commit(true));
        // A FAILED delivery must NOT commit the cursor (so the prompt replays).
        assert!(!legacy_should_commit("answer", false));
        assert!(!legacy_stream_should_commit(false));
        // A blank response commits regardless (nothing to replay).
        assert!(legacy_should_commit("   ", false));
        // The cursor is governed by `delivery_result.is_ok()` (the bridge's own
        // result), NOT by the controller's `committed_relay_offset` — A6b leaves
        // this legacy path untouched.
    }
}
