//! #3089 A4 watcher terminal short-replace cutover to the unified
//! turn-output controller (flag-gated, default OFF).
//!
//! This sibling module (mirroring `tmux_watcher/{liveness,commit_decisions,..}.rs`)
//! holds the A4 cutover surface so the FROZEN `tmux_watcher.rs` giant-file ratchet
//! (8223) absorbs only the small gate `if` + `DiscordGateway::new` construction +
//! the `mod terminal_send;` line. The flag helper, the `WatcherPostHeartbeat`
//! adapter, the gateway-generic `deliver_short_replace_via_controller`, and the
//! pure `watcher_terminal_lease_range` gate all live here.

use std::sync::Arc;
use std::sync::OnceLock;

use super::*;

use crate::services::discord::gateway::TurnGateway;
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::discord::outbound::turn_output_controller as toc;
use crate::services::discord::placeholder_controller::{PlaceholderKey, PlaceholderLifecycle};
use crate::services::discord::turn_finalizer::TurnKey;
use crate::services::discord::{
    DeliveryLeaseCell, DeliveryLeaseHeartbeat, LeaseHolder, SharedData, lease_now_ms,
};
use crate::services::provider::ProviderKind;

/// #3089 A4: flag gating ONLY the watcher's short-replace terminal delivery branch
/// (`replace_long_message_raw_with_outcome`) onto the unified
/// [`toc::deliver_turn_output`]. Default OFF → the legacy short-replace arm runs
/// byte-identically; ON → the controller drives acquire→POST→commit→advance→release
/// on the SAME `(channel, turn, [start,end))` lease as `LeaseHolder::Watcher`.
/// OnceLock+env, mirroring `sink_short_replace_controller_enabled` (A2b) /
/// `standby_relay_controller_enabled` (A3).
pub(in crate::services::discord) fn watcher_terminal_controller_enabled() -> bool {
    static CACHED: OnceLock<bool> = OnceLock::new();
    *CACHED.get_or_init(|| {
        let on = std::env::var("AGENTDESK_WATCHER_TERMINAL_CONTROLLER")
            .ok()
            .map(|v| v.trim().to_ascii_lowercase())
            .is_some_and(|v| v == "1" || v == "true");
        // Telemetry ONLY when ENABLED — the default-OFF first evaluation must have
        // NO observable side effect (byte-identical / deploy no-op), matching A2b/A3.
        if on {
            tracing::info!("  ✓ watcher_terminal_controller: enabled");
        }
        on
    })
}

/// #3089 A4: the watcher short-replace cut-over decision. Computed at the lease
/// acquire site (tmux_watcher.rs ~5944) so the watcher's own acquire/heartbeat/
/// commit/advance/release can be gated behind `!cutover` (the controller owns the
/// single lease when cut over — no double-acquire).
///
/// The flag is checked FIRST so OFF short-circuits before any work (the `formatted`
/// body is only computed by the caller's flag-gated closure) — byte-identical /
/// deploy no-op on the default-OFF path.
///
/// Terms (mirroring the legacy short-replace branch arm at tmux_watcher.rs:6153-6394):
/// - `will_direct_send` — the watcher will run the direct-send arm
///   (`watcher_direct_fallback_after_session_bound_ack && has_direct_terminal_response`,
///   tmux_watcher.rs:5942/6155).
/// - `ordered_range` — `watcher_lease_end > watcher_lease_start` (a real `[start,end)`).
/// - `has_placeholder` — `placeholder_msg_id.is_some()` (the `Some(msg_id)` arm, :6154).
/// - `should_send_ordered_new_chunks` —
///   `watcher_should_send_ordered_new_chunks_for_terminal_fallback(..)` (:6156). The
///   long-chunk fallback branch (send-new-chunks + placeholder delete) is NOT
///   expressible via the controller's `SendNewChunks` (it does not delete the anchor),
///   so it stays legacy → EXCLUDED.
/// - `formatted_is_empty` — the POST-format body is empty. Legacy
///   `replace_long_message_raw_with_outcome` treats a zero-chunk body as
///   `EditedOriginal` (delivered/advance) but the controller short-circuits an empty
///   body to `Skipped` (no-advance), so empty bodies MUST stay legacy (A2b M2 parity).
/// - `tui_completion_gate_required` —
///   `watcher_terminal_kind_requires_tui_completion_gate(terminal_kind)` (:6726). TUI-
///   gated turns' `Delivered`-vs-`Unknown` commit depends on the POST-send
///   `lifecycle_stage_paused` which the controller's inline-commit cannot express, so
///   they are EXCLUDED (stay legacy). Excluding them is ALSO what makes
///   `lifecycle_stage_paused` always-false for the cut-over set (NotGated →
///   `watcher_tui_gate_blocks_lifecycle == false`), so the advance callback returns
///   `true` on confirmed transport.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn watcher_short_replace_cutover(
    controller_enabled: bool,
    will_direct_send: bool,
    ordered_range: bool,
    has_placeholder: bool,
    should_send_ordered_new_chunks: bool,
    formatted_is_empty: bool,
    tui_completion_gate_required: bool,
) -> bool {
    controller_enabled
        && will_direct_send
        && ordered_range
        && has_placeholder
        && !should_send_ordered_new_chunks
        && !formatted_is_empty
        && !tui_completion_gate_required
}

/// #3089 A4: the full short-replace cut-over decision at the watcher lease-acquire
/// site. The flag is checked FIRST so OFF short-circuits before formatting the body
/// — byte-identical / deploy no-op. When ON it formats the body EXACTLY as the send
/// arm (tmux_watcher.rs:6173-6187: `format_for_discord_with[_status_panel]` then the
/// optional `prepend_monitor_auto_turn_origin`) so the `should_send_ordered_new_chunks`
/// (length) and `formatted_is_empty` terms match what the send arm sees, then applies
/// [`watcher_short_replace_cutover`]. Kept here (not inlined) so the frozen
/// `tmux_watcher.rs` call site stays a single line.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn watcher_short_replace_cutover_decision(
    controller_enabled: bool,
    status_panel_v2_enabled: bool,
    should_tag_monitor_origin: bool,
    provider: &ProviderKind,
    direct_terminal_response: &str,
    will_direct_send: bool,
    ordered_range: bool,
    has_placeholder: bool,
    session_bound_fallback_uses_full_body: bool,
    tui_completion_gate_required: bool,
) -> bool {
    if !controller_enabled {
        return false;
    }
    let formatted = if status_panel_v2_enabled {
        crate::services::discord::formatting::format_for_discord_with_status_panel(
            direct_terminal_response,
            provider,
        )
    } else {
        crate::services::discord::formatting::format_for_discord_with_provider(
            direct_terminal_response,
            provider,
        )
    };
    let formatted = if should_tag_monitor_origin {
        crate::services::discord::prepend_monitor_auto_turn_origin(&formatted)
    } else {
        formatted
    };
    watcher_short_replace_cutover(
        controller_enabled,
        will_direct_send,
        ordered_range,
        has_placeholder,
        super::watcher_should_send_ordered_new_chunks_for_terminal_fallback(
            session_bound_fallback_uses_full_body,
            &formatted,
        ),
        formatted.is_empty(),
        tui_completion_gate_required,
    )
}

/// #3089 A4: pure no-double-acquire gate. The watcher acquires its OWN
/// `Leased{Watcher}` marker over `cutover_range` (tmux_watcher.rs ~5944) and
/// commits/advances/releases it inline (~6996/7009/7023). When the short-replace
/// branch is cut over, the CONTROLLER owns that single lease, so the watcher's own
/// acquire/heartbeat/commit/advance/release MUST be skipped — this returns `None`
/// for any cut-over turn. Extracted so the invariant is testable: dropping
/// `!cutover_short_replace` fails `cutover_skips_watcher_lease_acquire`. Mirrors
/// A2b's `sink_guard_lease_range`.
pub(in crate::services::discord) fn watcher_terminal_lease_range(
    cutover_range: Option<(u64, u64)>,
    cutover_short_replace: bool,
) -> Option<(u64, u64)> {
    cutover_range.filter(|_| !cutover_short_replace)
}

/// #3089 A4: adapts the watcher's `DeliveryLeaseHeartbeat` to [`toc::PostHeartbeat`].
/// Holds the `Arc` (the controller drives the lease behind a borrowed `&cell`) and
/// spawns the SAME `DeliveryLeaseHeartbeat::spawn` the legacy watcher used
/// (tmux_watcher.rs:6015, #3041 §3 / #3151 — identical renew cadence); the guard
/// Drop aborts the renew task BEFORE the inline commit (#3151 ordering). Mirrors
/// A2b's `SinkPostHeartbeat`.
pub(in crate::services::discord) struct WatcherPostHeartbeat {
    pub(in crate::services::discord) cell: Arc<DeliveryLeaseCell>,
}

impl toc::PostHeartbeat for WatcherPostHeartbeat {
    fn start(&self, holder: LeaseHolder, turn: TurnKey) -> Box<dyn toc::PostHeartbeatGuard> {
        Box::new(WatcherPostHeartbeatGuard {
            _heartbeat: DeliveryLeaseHeartbeat::spawn(self.cell.clone(), holder, turn),
        })
    }
}

struct WatcherPostHeartbeatGuard {
    _heartbeat: DeliveryLeaseHeartbeat,
}

impl toc::PostHeartbeatGuard for WatcherPostHeartbeatGuard {}

/// #3089 A4: watcher short-replace via the turn-output controller, behaviourally
/// equal to the legacy `replace_long_message_raw_with_outcome` arm — SAME transport,
/// SAME per-channel cell as `LeaseHolder::Watcher` acquired/committed/advanced/
/// released ONCE (no double-acquire: the watcher's own acquire/heartbeat/commit/
/// advance/release are skipped via `watcher_terminal_lease_range`), SAME #3041 §3 /
/// #3151 heartbeat.
///
/// #2757 byte-identical: `EditFailPlaceholderPolicy::PreserveAlways`. The watcher's
/// EFFECTIVE edit-fail policy today is PreserveAlways because
/// `watcher_fallback_edit_failure_can_delete_original_placeholder(..)` returns
/// `false` UNCONDITIONALLY (tmux_watcher/liveness.rs:127-135, #2757 parity), so the
/// conditional-delete arm is dead. `DeleteIfProvenStale` stays dormant; a mutation
/// to it makes the controller delete on `EditFailed`, which the legacy arm never
/// does — so PreserveAlways is load-bearing (`watcher_short_replace_preserve_always`).
///
/// `CommitOnFallback` mirrors the legacy `SentFallbackAfterEditFailure` arm
/// (tmux_watcher.rs:6266-6349), which sets `direct_send_delivered = true` (→ the
/// commit advances when `relay_ok`). `AcquireFailureMode::Transient` mirrors the
/// watcher's B2-skip arm (tmux_watcher.rs:5988/6103): a lost acquire means another
/// holder owns the range → do NOT re-send. `Replace { Active }` keeps
/// `post_send_finalize` a no-op (the replace IS the edit, like legacy).
///
/// Advance: the cut-over set EXCLUDES TUI-gated turns
/// (`!watcher_terminal_kind_requires_tui_completion_gate`), so the legacy
/// `lifecycle_stage_paused` is ALWAYS `false` for it (NotGated →
/// `watcher_tui_gate_blocks_lifecycle(NotGated, _) == false`). The legacy commit
/// therefore advances IFF `relay_ok` (tmux_watcher.rs:6989-7017). On a CONFIRMED
/// transport the controller invokes this callback (never on Transient/Unknown, I2),
/// so the callback calls the REAL `advance_watcher_confirmed_end(.., watcher_lease_end)`
/// — the SAME monotonic-CAS, SAME `end`, SAME call site context as legacy — and
/// returns `true` (→ Delivered). The controller's release then returns the cell to
/// Unleased for the next turn, exactly as the legacy `release` (tmux_watcher.rs:7023).
///
/// `gateway` is a seam: the live path passes the real `DiscordGateway`; the test
/// injects a fake driving the REAL controller + real cell.
///
/// `DeliveryOutcome` → [`WatcherShortReplaceResult`] (the caller maps it back into the
/// watcher's `(relay_ok, direct_send_delivered, retry)` locals; the unchanged
/// lifecycle then consumes them):
/// - `Delivered` / `NotDelivered` → confirmed POST landed → `Delivered`
///   (`relay_ok = true`, `direct_send_delivered = true`). The lease outcome only steered
///   the watcher's own re-send gate, which the controller already committed.
/// - `Transient` → lost acquire (another holder owns the range). The legacy watcher
///   would have lost its OWN acquire at :5944 and taken the `watcher_lease_b2_skip` arm
///   (:6103), which returns `relay_ok = false` with NO transport (the live holder commits
///   the offset). `B2Skip` reproduces that exactly. (The cut-over gate sets
///   `watcher_lease_b2_skip = false` so the chain reaches arm 5; the controller's
///   `AcquireFailureMode::Transient` is the B2-skip equivalent.)
/// - `Unknown` → ambiguous (PartialContinuationFailure / transport Err): I2 — never
///   advanced. Reproduce the legacy partial-failure handling: `relay_ok = false` + the
///   caller resets `retry_terminal_delivery_from_offset` / `current_offset` / `all_data`
///   and abandon-releases (tmux_watcher.rs:6384-6386 / 6546-6579).
/// - `Skipped` → empty body (excluded by the cut-over gate); unreachable in prod.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn deliver_short_replace_via_controller<
    G: TurnGateway + ?Sized,
>(
    gateway: &G,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    msg_id: MessageId,
    relay_text: &str,
    cell: &Arc<DeliveryLeaseCell>,
    turn: TurnKey,
    instance_id: u64,
    start: u64,
    end: u64,
) -> WatcherShortReplaceResult {
    let holder = LeaseHolder::Watcher { instance_id };
    // Self-heal like the legacy acquire (tmux_watcher.rs:5964): reclaim an EXPIRED
    // prior holder before the controller's acquire (a stale dead lease must not make
    // this acquire lose and B2-skip a deliverable range).
    cell.reclaim_if_expired(lease_now_ms());
    let heartbeat = WatcherPostHeartbeat { cell: cell.clone() };
    // Identity-gated advance: INLINE before any post-send await (I1). For the cut-over
    // set `lifecycle_stage_paused` is always false (TUI-gated turns excluded), so the
    // legacy path advances IFF `relay_ok` — i.e. on confirmed transport. The controller
    // invokes this ONLY on confirmed transport (never Transient/Unknown), so it runs
    // the REAL `advance_watcher_confirmed_end` to `end` (the legacy `watcher_lease_end`)
    // and returns `true` → Delivered.
    let advance = |range: (u64, u64)| -> bool {
        debug_assert_eq!(range, (start, end));
        crate::services::discord::tmux::advance_watcher_confirmed_end(
            shared,
            provider,
            channel_id,
            tmux_session_name,
            end,
            "src/services/discord/tmux_watcher/terminal_send.rs:watcher_controller_advance",
        );
        true
    };
    let outcome = toc::deliver_turn_output(
        gateway,
        toc::TurnOutputCtx {
            turn,
            owner: RelayOwnerKind::Watcher,
            holder,
            lease: &**cell,
            channel_id,
            placeholder_controller: &shared.ui.placeholder_controller,
            placeholder: toc::PlaceholderSlot::Active {
                message_id: msg_id,
                key: PlaceholderKey {
                    provider: provider.clone(),
                    channel_id,
                    message_id: msg_id,
                },
            },
            body: relay_text,
            send_range: (start, end),
            // `Replace { Active }` → non-terminal → `post_send_finalize` no-ops (no
            // placeholder transition), matching the legacy edit-in-place.
            plan: toc::OutputPlan::Replace {
                lifecycle: PlaceholderLifecycle::Active,
            },
            // #2757: the watcher NEVER deletes the original on edit-fail fallback
            // (the conditional-delete predicate is const-false). PreserveAlways is
            // byte-identical; `DeleteIfProvenStale` stays dormant.
            edit_fail_policy: toc::EditFailPlaceholderPolicy::PreserveAlways,
            fallback_commit_policy: toc::FallbackCommitPolicy::CommitOnFallback,
            // B2 (single-holder, §5.2): a lost acquire is another holder's range → do
            // NOT re-send. Mirrors the legacy `watcher_lease_b2_skip` arm.
            acquire_failure_mode: toc::AcquireFailureMode::Transient,
            advance: Some(&advance),
            heartbeat: Some(&heartbeat),
        },
    )
    .await;

    match outcome {
        // Confirmed POST (edit OR #2757 fallback): the controller already ran
        // advance + commit + release. The turn delivered.
        toc::DeliveryOutcome::Delivered { .. } | toc::DeliveryOutcome::NotDelivered { .. } => {
            WatcherShortReplaceResult::Delivered
        }
        // Lost acquire → the legacy B2-skip arm (`watcher_lease_b2_skip`,
        // tmux_watcher.rs:6103): another holder owns this range. No transport, no
        // advance — the live holder commits the offset. The legacy arm returns
        // `relay_ok = false` and `direct_send_delivered` stays false.
        toc::DeliveryOutcome::Transient { .. } => WatcherShortReplaceResult::B2Skip,
        // Ambiguous (PartialContinuationFailure or transport Err): I2 — never advanced.
        // Reproduce the legacy partial-failure handling: relay_ok = false + reset the
        // retry offset (the caller performs the `retry_terminal_delivery_from_offset` /
        // current_offset / all_data reset + abandon-release, tmux_watcher.rs:6546-6579).
        toc::DeliveryOutcome::Unknown => WatcherShortReplaceResult::PartialFailureRetry,
        // Empty body — excluded by the cut-over gate, so this is unreachable in prod.
        toc::DeliveryOutcome::Skipped => WatcherShortReplaceResult::Skipped,
    }
}

/// #3089 A4: borrowed `&mut` handles to the watcher send-arm locals the controller
/// path writes back into. Bundled into one struct so the frozen `tmux_watcher.rs`
/// call site stays small (LoC) while keeping the write-back explicit and testable.
pub(in crate::services::discord) struct WatcherShortReplaceLocals<'a> {
    pub(in crate::services::discord) relay_ok: &'a mut bool,
    pub(in crate::services::discord) direct_send_delivered: &'a mut bool,
    pub(in crate::services::discord) tui_direct_anchor_terminal_body_visible: &'a mut bool,
    pub(in crate::services::discord) external_input_lease_consumed_by_relay: &'a mut bool,
    pub(in crate::services::discord) placeholder_msg_id: &'a mut Option<MessageId>,
    pub(in crate::services::discord) placeholder_from_restored_inflight: &'a mut bool,
    pub(in crate::services::discord) last_edit_text: &'a mut String,
    pub(in crate::services::discord) completion_footer_terminal_target:
        &'a mut Option<WatcherCompletionFooterTerminalTarget>,
    pub(in crate::services::discord) retry_terminal_delivery_from_offset: &'a mut bool,
}

/// #3089 A4: run the controller short-replace then write the outcome back into the
/// watcher send-arm locals — the production cut-over wiring. `Delivered` reproduces
/// the legacy `EditedOriginal` delivered side-effects (footer target, placeholder
/// clear, orphan-record drop, `EditTerminal`/`Succeeded` cleanup record). `B2Skip`
/// = the legacy `watcher_lease_b2_skip` arm (`relay_ok = false`, no transport).
/// `PartialFailureRetry` = the legacy partial-continuation reset
/// (`watcher_partial_continuation_retry_plan`, tmux_watcher.rs:6384). `Skipped`
/// (empty body, unreachable in prod) → `relay_ok = false`. `gateway` (the real
/// `DiscordGateway`) is built here from `http`/`shared`/`provider`.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn apply_watcher_short_replace_controller(
    http: &Arc<serenity::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    msg_id: MessageId,
    relay_text: &str,
    cell: &Arc<DeliveryLeaseCell>,
    turn: TurnKey,
    instance_id: u64,
    range: (u64, u64),
    single_message_panel_footer_mode: bool,
    inflight_before_relay: Option<&crate::services::discord::InflightTurnState>,
    locals: WatcherShortReplaceLocals<'_>,
) {
    // Live path: the real `DiscordGateway` (the seam the ON-path test fakes).
    let gateway = crate::services::discord::gateway::DiscordGateway::new(
        http.clone(),
        shared.clone(),
        provider.clone(),
        None,
    );
    let result = deliver_short_replace_via_controller(
        &gateway,
        shared,
        provider,
        channel_id,
        tmux_session_name,
        msg_id,
        relay_text,
        cell,
        turn,
        instance_id,
        range.0,
        range.1,
    )
    .await;
    match result {
        WatcherShortReplaceResult::Delivered => {
            *locals.direct_send_delivered = true;
            *locals.tui_direct_anchor_terminal_body_visible = true;
            *locals.external_input_lease_consumed_by_relay =
                super::watcher_inflight_represents_external_input(inflight_before_relay);
            remember_watcher_completion_footer_terminal_target(
                single_message_panel_footer_mode,
                locals.completion_footer_terminal_target,
                msg_id,
                relay_text,
            );
            *locals.placeholder_msg_id = None;
            *locals.placeholder_from_restored_inflight = false;
            locals.last_edit_text.clear();
            drop_placeholder_orphan_record(provider, shared, channel_id, msg_id);
            // tmux.rs private helper — accessible from this descendant of `tmux`.
            super::super::record_placeholder_cleanup(
                shared,
                provider,
                channel_id,
                msg_id,
                tmux_session_name,
                crate::services::discord::placeholder_cleanup::PlaceholderCleanupOperation::EditTerminal,
                crate::services::discord::placeholder_cleanup::PlaceholderCleanupOutcome::Succeeded,
                "watcher_terminal_relay_controller",
            );
        }
        WatcherShortReplaceResult::B2Skip | WatcherShortReplaceResult::Skipped => {
            *locals.relay_ok = false;
        }
        WatcherShortReplaceResult::PartialFailureRetry => {
            let plan = crate::services::discord::replace_outcome_policy::watcher_partial_continuation_retry_plan();
            *locals.relay_ok = plan.relay_ok;
            *locals.retry_terminal_delivery_from_offset = plan.retry_offset;
        }
    }
}

/// #3089 A4: the controller-path result mapped back into the watcher's send-arm
/// locals by `apply_watcher_short_replace_controller`. Keeps the `DeliveryOutcome`
/// → `(relay_ok, direct_send_delivered, retry)` translation in one testable place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::services::discord) enum WatcherShortReplaceResult {
    /// Confirmed transport (edit or #2757 fallback). The controller committed +
    /// advanced + released. `relay_ok = true`, `direct_send_delivered = true`.
    Delivered,
    /// Lost acquire → the legacy `watcher_lease_b2_skip` arm: another holder owns
    /// the range. No transport. `relay_ok = false`, `direct_send_delivered = false`
    /// (the live holder advances the offset).
    B2Skip,
    /// Partial / ambiguous failure (I2, no advance). `relay_ok = false` and the
    /// caller resets the retry offset (tmux_watcher.rs:6546-6579).
    PartialFailureRetry,
    /// Empty body (cut-over gate excludes it). Unreachable in prod; mapped to a no-op.
    Skipped,
}
