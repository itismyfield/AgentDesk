//! #3089 A4 watcher terminal cutover to the unified turn-output controller.
//!
//! This sibling keeps the A4 controller helpers out of the frozen
//! `tmux_watcher.rs` root. Long-chunk helpers live in `terminal_long_chunks.rs`;
//! the shared heartbeat adapter lives in `controller_heartbeat.rs`.

use std::sync::Arc;

use super::*;

use crate::services::discord::gateway::TurnGateway;
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::discord::outbound::delivery_record as dr;
use crate::services::discord::outbound::turn_output_controller as toc;
use crate::services::discord::placeholder_controller::{PlaceholderKey, PlaceholderLifecycle};
use crate::services::discord::turn_finalizer::TurnKey;
use crate::services::discord::{DeliveryLeaseCell, LeaseHolder, SharedData, lease_now_ms};
use crate::services::provider::ProviderKind;

use super::controller_heartbeat::WatcherPostHeartbeat;

/// #3089 A4/#3998 S1-d: watcher terminal controller cut-over decision.
/// Computed at the lease acquire site so the watcher's own acquire/heartbeat/
/// commit/advance/release can be gated behind `!cutover`.
///
/// Structural exclusions stay legacy: no direct send, no ordered range, no
/// placeholder anchor, empty formatted body, or TUI completion gate. Long bodies
/// still use the controller through `SendNewChunks { delete_anchor: true }` when
/// the same structural conditions hold.
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn watcher_short_replace_cutover(
    will_direct_send: bool,
    ordered_range: bool,
    has_placeholder: bool,
    _should_send_ordered_new_chunks: bool,
    formatted_is_empty: bool,
    tui_completion_gate_required: bool,
) -> bool {
    will_direct_send
        && ordered_range
        && has_placeholder
        && !formatted_is_empty
        && !tui_completion_gate_required
}

/// #3089 A4: full cut-over decision at the watcher lease-acquire site. Formats
/// the body exactly as the send arm, then applies
/// [`watcher_short_replace_cutover`].
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn watcher_short_replace_cutover_decision(
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
/// - `Delivered { EditedOriginal | None }` / `NotDelivered` → confirmed POST via the
///   in-place edit → `Delivered` (`relay_ok = true`, `direct_send_delivered = true`).
///   The lease outcome only steered the watcher's own re-send gate, which the
///   controller already committed.
/// - `Delivered { FreshFallbackAfterEditFailure { edit_error, .. } }` → confirmed
///   POST via a FRESH fallback send after the in-place edit failed →
///   `DeliveredFallback` (#3089 A4 r2). Still delivered/advanced, but the write-back
///   mirrors the legacy fallback arm (NO footer target, `Failed(edit_error)` cleanup,
///   original preserved).
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
/// - `Skipped` → no-op/no-retry (empty body, or a permanent watcher transport
///   failure classified by the controller).
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
    delivered_body: &str,
    cell: &Arc<DeliveryLeaseCell>,
    turn: TurnKey,
    lease_key: Option<crate::services::discord::DeliveryLeaseKey>,
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
            lease_key,
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

    // #3089 B2a: shadow-mirror durable delivered frontier — flag-gated,
    // observe-only, Delivered-only (I2). #4081 still records confirmed body
    // fingerprints when the frontier mirror is OFF. Extends B1's sink coverage to
    // the watcher (A4) before B2b's authority flip.
    // #3610 PR-1: anchor = `msg_id` — the controller active-slot `current_msg_id`
    // (the assistant response message terminal-replace edits in place), NOT
    // `status_message_id`. Records the true terminal anchor for PR-2.
    // #3610 PR-1b: the anchor pair's channel is this same `channel_id` (same-channel
    // path — the frontier key, edit target, and placeholder all share `channel_id`).
    dr::shadow_mirror_delivered_frontier(
        shared,
        provider,
        channel_id,
        (start, end),
        dr::outcome_is_shadow_delivered(&outcome),
        Some(msg_id.get()),
        Some(channel_id.get()),
        Some(delivered_body),
        // #4564: same-channel watcher path — `channel_id` IS the inbound channel, so
        // `None` lets the funnel reload it (race-safe under identity-gated advance).
        None,
    );

    match outcome {
        // Confirmed POST (edit OR #2757 fallback): the controller already ran
        // advance + commit + release. The turn delivered. Carry the replace
        // identity (#3089 A4 r2): `EditedOriginal` → the legacy edit side-effects
        // (footer-target, `Succeeded`); `FreshFallbackAfterEditFailure` → the
        // legacy fallback arm (NO footer-target, `Failed(edit_error)`, preserve).
        // `NotDelivered` carries no delivered original to footer → treat as
        // `EditedOriginal` (the existing arm; the gate excludes the refused-advance
        // case from the cut-over set, so this is the conservative legacy default).
        toc::DeliveryOutcome::Delivered {
            replace_kind:
                Some(toc::ReplaceDeliveryKind::FreshFallbackAfterEditFailure { edit_error, .. }),
            ..
        } => WatcherShortReplaceResult::DeliveredFallback { edit_error },
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
        // #3089 A5: the watcher uses CommitOnFallback, so `fell_back` is always
        // false for it (a fallback send commits → Delivered, never reaching this
        // arm) — byte-identical: the watcher ignores the field.
        toc::DeliveryOutcome::Unknown { .. } => WatcherShortReplaceResult::PartialFailureRetry,
        // SendFresh is not a short-replace plan; keep an impossible cross-verb
        // result conservative rather than claiming placeholder delivery.
        toc::DeliveryOutcome::FreshDelivered { .. } => WatcherShortReplaceResult::Skipped,
        // No-op/no-retry: empty body, or permanent watcher transport failure.
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
/// watcher send-arm locals — the production cut-over wiring. `Delivered`
/// (`EditedOriginal`) reproduces the legacy `EditedOriginal` delivered side-effects
/// (footer target, placeholder clear, orphan-record drop, `EditTerminal`/
/// `Succeeded` cleanup record, tmux_watcher.rs:6247-6288). `DeliveredFallback`
/// (`SentFallbackAfterEditFailure`) reproduces the legacy FALLBACK arm
/// (tmux_watcher.rs:6289-6372, codex r1 [High]): NO footer target,
/// `EditTerminal`/`Failed(edit_error)` cleanup record, the original placeholder
/// PRESERVED (#2757) — clear placeholder locals + drop orphan record only. `B2Skip`
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
    delivered_body: &str,
    cell: &Arc<DeliveryLeaseCell>,
    turn: TurnKey,
    lease_key: Option<crate::services::discord::DeliveryLeaseKey>,
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
        delivered_body,
        cell,
        turn,
        lease_key,
        instance_id,
        range.0,
        range.1,
    )
    .await;
    apply_watcher_short_replace_result(
        result,
        shared,
        provider,
        channel_id,
        tmux_session_name,
        msg_id,
        relay_text,
        single_message_panel_footer_mode,
        inflight_before_relay,
        locals,
    );
}

/// #3089 A4: write the controller-path [`WatcherShortReplaceResult`] back into the
/// watcher send-arm locals. Split out of [`apply_watcher_short_replace_controller`]
/// (a pure, synchronous mapping with NO gateway/transport) so the per-variant
/// side-effects — the #3089 A4 r2 footer-target / cleanup-record / preserve branch —
/// are unit-testable WITHOUT a live `DiscordGateway`
/// (`watcher_short_replace_fallback_mirrors_legacy`).
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) fn apply_watcher_short_replace_result(
    result: WatcherShortReplaceResult,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    tmux_session_name: &str,
    msg_id: MessageId,
    relay_text: &str,
    single_message_panel_footer_mode: bool,
    inflight_before_relay: Option<&crate::services::discord::InflightTurnState>,
    locals: WatcherShortReplaceLocals<'_>,
) {
    match result {
        // Legacy `EditedOriginal` arm (tmux_watcher.rs:6247-6288): the original was
        // edited in place → register it as the completion-footer target +
        // `EditTerminal`/`Succeeded` cleanup record.
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
        // #3089 A4 r2 (codex r1 [High]): the in-place edit FAILED and the body was
        // delivered via a FRESH fallback send. Mirror the LEGACY fallback arm
        // (tmux_watcher.rs:6289-6372) EXACTLY — it does the OPPOSITE of the
        // `EditedOriginal` arm for the original `msg_id`: it does NOT register the
        // original as the completion-footer target (so footer mode can never later
        // edit the preserved original), it records the cleanup as
        // `Failed(edit_error)` (not `Succeeded`), and it PRESERVES the original
        // placeholder. The body still landed (advance happened in the controller),
        // so `direct_send_delivered`/`tui_direct_anchor_terminal_body_visible`/
        // `external_input_lease_consumed_by_relay` are set identically to the
        // `EditedOriginal` arm. Because the watcher's
        // `watcher_fallback_edit_failure_can_delete_original_placeholder` is
        // UNCONDITIONALLY false (#2757, liveness.rs:127-135), the legacy arm always
        // takes its `else` branch (tmux_watcher.rs:6353-6371): clear the placeholder
        // locals and drop the orphan record while preserving the message itself.
        WatcherShortReplaceResult::DeliveredFallback { edit_error } => {
            *locals.direct_send_delivered = true;
            *locals.tui_direct_anchor_terminal_body_visible = true;
            *locals.external_input_lease_consumed_by_relay =
                super::watcher_inflight_represents_external_input(inflight_before_relay);
            // Legacy fallback cleanup record: `EditTerminal` / `Failed(edit_error)`
            // (tmux_watcher.rs:6305-6314) — NOT `Succeeded`. The `edit_error` is the
            // failing in-place edit's error, surfaced through the controller.
            super::super::record_placeholder_cleanup(
                shared,
                provider,
                channel_id,
                msg_id,
                tmux_session_name,
                crate::services::discord::placeholder_cleanup::PlaceholderCleanupOperation::EditTerminal,
                crate::services::discord::placeholder_cleanup::PlaceholderCleanupOutcome::failed(
                    edit_error,
                ),
                "watcher_terminal_relay_controller",
            );
            // #2757: NO `remember_watcher_completion_footer_terminal_target` — the
            // original placeholder is preserved and must NEVER become the footer
            // target (the legacy fallback arm omits the call entirely).
            //
            // Legacy `else` branch (const-false delete predicate, #2757):
            // clear the placeholder locals + drop the orphan record while the
            // message itself is preserved (tmux_watcher.rs:6353-6371).
            *locals.placeholder_msg_id = None;
            *locals.placeholder_from_restored_inflight = false;
            locals.last_edit_text.clear();
            drop_placeholder_orphan_record(provider, shared, channel_id, msg_id);
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

/// #3610 PR-1d: record the durable terminal anchor for the WATCHER legacy
/// long-chunk fallback arm (`tmux_watcher.rs` — the
/// `watcher_should_send_ordered_new_chunks_for_terminal_fallback` branch:
/// `send_long_message_raw_with_rollback` send-new-chunks + placeholder delete).
/// This arm is the watcher-owned counterpart of the bridge long-chunk arm PR-1c
/// instrumented. S1-d routes the flag-ON long-chunk path through the controller;
/// this helper remains the shared durable-anchor record point for the controller
/// path and the flag-OFF legacy path.
///
/// The caller (the FROZEN giant `tmux_watcher.rs`) invokes this with a SINGLE line,
/// ONLY when BOTH gates hold (matching PR-1c's M4 discipline at the bridge):
/// - (A) the send fully committed: `send_long_message_raw_with_rollback` is
///   all-or-nothing — a partial chunk failure rolls back the already-sent chunks
///   and returns `Err` (formatting.rs), so the `last_chunk_anchor_msg_id` is only
///   `Some` on the full-commit `Ok` arm; and
/// - (M4) the watcher lease `commit` returned `true` AND advanced (the caller gates
///   on `committed && commit_outcome == Delivered`, the exact site that runs
///   `advance_watcher_confirmed_end` to `watcher_lease_end`). Recording without an
///   in-memory advance would leave the durable frontier END ahead of
///   `confirmed_end_offset` (M4 violation), so the caller passes the anchor through
///   to the post-advance site rather than recording at the send arm.
///
/// Same-channel (unlike the bridge cutover's channel split): the watcher acquires
/// its lease on, advances, and edits the SAME `channel_id`, so the frontier key
/// (offset authority) and the anchor pair's channel are BOTH `channel_id`. `range`
/// is `(watcher_lease_start, watcher_lease_end)` — the SAME offset range the lease
/// committed and `confirmed_end_offset` advanced to (never mix offset spaces).
/// Delegates to the shared `dr::record_long_chunk_terminal_delivery` (PR-1c) with
/// `watcher_owner_channel_id == delivery_channel_id == channel_id`; the delivered
/// frontier still obeys the shadow flag, while #4081 records the confirmed body
/// fingerprint for degenerate-key duplicate refusal.
pub(in crate::services::discord) fn record_watcher_long_chunk_terminal_delivery(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel_id: ChannelId,
    range: (u64, u64),
    last_chunk_anchor_msg_id: Option<u64>,
    delivered_body: &str,
) {
    dr::record_long_chunk_terminal_delivery(
        shared,
        provider,
        channel_id,
        channel_id,
        range,
        last_chunk_anchor_msg_id,
        delivered_body,
        // #4564: same-channel watcher long-chunk (owner == delivery == channel_id),
        // so `None` reloads `channel_id` for the inbound id (race-safe here).
        None,
    );
}

/// #3089 A4: the controller-path result mapped back into the watcher's send-arm
/// locals by `apply_watcher_short_replace_controller`. Keeps the `DeliveryOutcome`
/// → `(relay_ok, direct_send_delivered, retry)` translation in one testable place.
///
/// NOT `Copy` (the `DeliveredFallback` arm carries the `edit_error` `String` so the
/// write-back reproduces the legacy `PlaceholderCleanupOutcome::failed(edit_error)`
/// record — #3089 A4 r2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::services::discord) enum WatcherShortReplaceResult {
    /// Confirmed transport via the in-place edit (`EditedOriginal`). The
    /// controller committed + advanced + released. `relay_ok = true`,
    /// `direct_send_delivered = true`. The original IS the final message →
    /// register it as the completion-footer target + `EditTerminal`/`Succeeded`
    /// cleanup (legacy `EditedOriginal` arm, tmux_watcher.rs:6247-6288).
    Delivered,
    /// Confirmed transport via a FRESH fallback send after the in-place edit
    /// FAILED (`SentFallbackAfterEditFailure`). The body landed (advance), but the
    /// original placeholder is PRESERVED (#2757) and is NOT the final message.
    /// Mirrors the legacy fallback arm (tmux_watcher.rs:6289-6372): do NOT register
    /// the original as the footer target, record `EditTerminal`/`Failed(edit_error)`
    /// cleanup, preserve the original (clear placeholder locals / drop orphan).
    DeliveredFallback { edit_error: String },
    /// Lost acquire → the legacy `watcher_lease_b2_skip` arm: another holder owns
    /// the range. No transport. `relay_ok = false`, `direct_send_delivered = false`
    /// (the live holder advances the offset).
    B2Skip,
    /// Partial / ambiguous failure (I2, no advance). `relay_ok = false` and the
    /// caller resets the retry offset (tmux_watcher.rs:6546-6579).
    PartialFailureRetry,
    /// No-op/no-retry: empty body (cut-over gate excludes it) or permanent
    /// watcher transport failure from the controller.
    Skipped,
}

#[cfg(test)]
mod tests {
    use super::*;
    use poise::serenity_prelude::ChannelId;

    /// #3610 PR-1d: the watcher long-chunk delivery helper under the default-OFF
    /// shadow flag must be a COMPLETE no-op (no panic, no durable write) regardless
    /// of the resolved anchor — the deploy-safe property (tests never set
    /// `AGENTDESK_DELIVERY_RECORD_SHADOW`, so the OnceLock reads OFF and the call
    /// short-circuits inside `shadow_mirror_delivered_frontier`). This is the
    /// watcher-arm counterpart of delivery_record.rs's
    /// `record_long_chunk_terminal_delivery_off_is_noop_3610c`.
    #[test]
    fn watcher_long_chunk_delivery_off_is_noop_3610d() {
        let temp = tempfile::TempDir::new().expect("temp runtime root");
        let _root_guard = crate::config::set_agentdesk_root_for_test(temp.path());
        let _shadow = dr::shadow_test_seam::force(false);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(556_677_889);
        // Does not panic; OFF → writes nothing.
        record_watcher_long_chunk_terminal_delivery(
            &shared,
            &ProviderKind::Claude,
            channel,
            (0, 8192),
            Some(912_345_678),
            "",
        );
        // No durable record was created under the test root for this channel.
        assert!(dr::read_record(&ProviderKind::Claude, channel.get()).is_none());
    }

    /// #3610 PR-1d gate (D), `last = None`: the empty-Vec anchor (impossible on the
    /// full-commit `Ok` path, but type-honest) is forwarded as `None` and the helper
    /// still no-ops under OFF without panicking. Pins that a null anchor is a legal
    /// input to the watcher wrapper (range-only record when the flag is ON).
    #[test]
    fn watcher_long_chunk_delivery_none_anchor_is_noop_3610d() {
        let temp = tempfile::TempDir::new().expect("temp runtime root");
        let _root_guard = crate::config::set_agentdesk_root_for_test(temp.path());
        let _shadow = dr::shadow_test_seam::force(false);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(112_233_445);
        record_watcher_long_chunk_terminal_delivery(
            &shared,
            &ProviderKind::Claude,
            channel,
            (0, 2048),
            None,
            "",
        );
        assert!(dr::read_record(&ProviderKind::Claude, channel.get()).is_none());
    }
}
