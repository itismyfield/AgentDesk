//! EPIC #3078 PR-4 — SHADOW parity helper for the tmux watcher's status-panel
//! RECLAIM path.
//!
//! The watcher (`tmux_watcher.rs`) owns the TUI-direct / external-input panel
//! create→edit→complete→reclaim path. PR-4 routes the RECLAIM decision through
//! the [`StatusPanelController`] behind a parity check: the controller computes
//! which panel id it WOULD reclaim and we assert it agrees with the legacy
//! watcher decision, while the legacy path keeps executing the real Discord IO.
//! The executing cutover lands in a later PR.
//!
//! Scope note: CREATE/ADOPT parity is now FAITHFUL — the controller re-derives
//! the create/adopt DECISION from the SAME RAW inputs the legacy
//! `watcher_should_create_external_input_status_panel` branch reads
//! (`WatcherCreateDecision`), so the shadow check validates the controller would
//! make the same publish/adopt choice, NOT the tautology of echoing the already-
//! resolved id back (the codex P2 finding that scoped PR-4 to RECLAIM-only). The
//! create id is not known until the send returns, so the comparison is on the
//! decision (adopt-which-id / create / none), not on a post-send message id.
//!
//! COMPLETION parity is still DEFERRED to the controller execute-cutover PR: a
//! faithful completion check needs the SendFallback-aware terminal id from the
//! legacy completion path, which is only known after the bridge/watcher send
//! resolves. The RECLAIM path is faithful via `sweeper_reclaim_parity_id` (the
//! read-only collapse codex validated for PR-3) against the panel id the legacy
//! reclaim is about to delete.
//!
//! The parity LOGIC lives here (not inline in the grandfathered, LoC-frozen
//! `tmux_watcher.rs`): the watcher reclaim site adds only a single `.await` hook
//! into `assert_watcher_reclaim_parity` below. Same posture as PR-2
//! (`recovery_engine`) and PR-3 (`placeholder_sweeper` + this crate's
//! `shadow_parity_warn`): never `panic!` (so an unseen shape cannot crash a prod
//! turn over the still-executing legacy path) — `debug_assert` (fail loud in
//! test/dev) + a bounded, log-once `warn!` via [`ParityWarnOnce`].
//!
//! v2-gated: when status-panel-v2 is OFF the controller actor is NOT spawned, so
//! the helper short-circuits BEFORE the awaited controller read whose ack would
//! never be answered (mirrors `recovery_engine` / `placeholder_sweeper`).

use serenity::model::id::{ChannelId, MessageId};

use super::SharedData;
use super::shadow_parity_warn::ParityWarnOnce;
use super::status_panel_controller::WatcherCreateDecision;
use super::turn_finalizer::TurnKey;
use crate::services::provider::ProviderKind;
use std::sync::Arc;

/// Watcher panel parity-mismatch shape: `(channel, site, controller_id, legacy_id)`.
/// `site` is retained for forward-compat with the deferred create/complete sites
/// so each diverging shape logs once.
type WatcherShape = (u64, &'static str, Option<u64>, Option<u64>);

/// One-shot bound for the PR-4 watcher parity-mismatch `warn!`: a hot watcher
/// loop iterating over a persistently-diverging turn must not log-flood, so each
/// distinct mismatch shape logs at most once.
static WATCHER_PARITY_WARNED: ParityWarnOnce<WatcherShape> = ParityWarnOnce::new();

/// Build the `TurnKey` the watcher parity gate keys on, reusing the
/// process-wide generation the controller ledger collapse expects. A TUI-direct
/// turn carries `user_msg_id == 0`, which the controller's `resolve_channel_only`
/// collapses onto the channel's single live entry (the #3003 turn-aware path).
fn watcher_turn_key(channel_id: ChannelId, user_msg_id: u64) -> TurnKey {
    TurnKey::new(
        channel_id,
        user_msg_id,
        super::runtime_store::load_generation(),
    )
}

/// Core parity assertion: `debug_assert` + bounded log-once `warn!`. Never
/// panics in release. Pure (no IO / no await) so the gating + warn-once shape is
/// unit-testable.
fn assert_parity(
    site: &'static str,
    controller_id: Option<MessageId>,
    legacy_id: Option<MessageId>,
    channel_id: u64,
) {
    if controller_id == legacy_id {
        return;
    }
    debug_assert_eq!(
        controller_id, legacy_id,
        "#3078 PR-4 watcher status-panel {site} parity mismatch (channel {channel_id}): controller chose {controller_id:?}, legacy chose {legacy_id:?}"
    );
    if !WATCHER_PARITY_WARNED.should_warn((
        channel_id,
        site,
        controller_id.map(MessageId::get),
        legacy_id.map(MessageId::get),
    )) {
        return;
    }
    tracing::warn!(
        channel = channel_id,
        site = site,
        controller_id = ?controller_id,
        legacy_id = ?legacy_id,
        "#3078 PR-4 watcher status-panel parity mismatch — StatusPanelController chose a different panel id than the legacy watcher; legacy path executed (no behaviour change), divergence logged once for the later controller-executes cutover"
    );
}

/// PR-4 reclaim site: assert the controller's chosen reclaim target equals the
/// `panel_msg_id` the legacy `cleanup_orphan_external_input_status_panel` deletes
/// + clears. v2-off → inert (no controller read). The watcher reclaim path is
/// keyed channel-only (TUI-direct `user_msg_id == 0`), like the sweeper, and
/// reuses the same read-only `sweeper_reclaim_parity_id` collapse (faithful,
/// codex-validated in PR-3).
pub(in crate::services::discord) async fn assert_watcher_reclaim_parity(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    panel_msg_id: MessageId,
) {
    if !shared.ui.status_panel_v2_enabled {
        return;
    }
    let controller_id = shared
        .status_panel_controller
        .sweeper_reclaim_parity_id(
            watcher_turn_key(channel_id, 0),
            provider.clone(),
            Some(panel_msg_id),
        )
        .await;
    assert_parity(
        "reclaim",
        controller_id,
        Some(panel_msg_id),
        channel_id.get(),
    );
}

/// EPIC #3078 PR-6 — SHADOW-seed the status panel the watcher liveness self-heal
/// (`reacquire_watcher_inflight_for_active_stream`) re-binds onto its freshly
/// minted synthetic (`user_msg_id == 0`) watcher-owned inflight, into the
/// [`StatusPanelController`](super::status_panel_controller) ledger.
///
/// SUBSTRATE, not a decision-parity: the re-acquire merely *reuses* the persisted
/// `status_message_id`, so re-deriving "which id" here would be the tautology the
/// PR-4 codex P2 finding warned against (echoing an already-resolved id). PR-6
/// instead SEEDS the ledger so a LATER faithful parity / execute-cutover can
/// observe the liveness-recovered panel — mirroring the PR-5 bridge ledger seed.
///
/// Uses `seed_live_panel` (NOT `adopt_recovered`): the watcher synthetic key is
/// REUSED across re-acquisitions, so the seed must OVERWRITE a stale earlier panel
/// id rather than first-id-wins — otherwise a second same-generation reacquire
/// would leave the ledger pinned to the prior panel (no controller finalize runs
/// in the shadow phase to terminalize it between turns).
///
/// Zero behaviour change:
/// - gated on `reacquired`: only the synthetic save that actually WON the
///   `save_inflight_state_if_absent` race owns the panel; a lost race (a
///   concurrent intake inflight) must NOT seed a Watcher-owned ledger entry the
///   watcher does not own.
/// - v2-gated: the controller actor is spawned but the flag is off, so we skip the
///   send entirely (mirrors `assert_watcher_reclaim_parity`).
/// - ledger-only: `seed_live_panel` is fire-and-forget (no Discord IO, no persist;
///   the PR-1 durable-mirror sink is still dormant), so the legacy liveness path
///   is byte-for-byte unchanged.
pub(in crate::services::discord) fn shadow_adopt_liveness_reacquired_panel(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    provider: &ProviderKind,
    panel_msg_id: Option<MessageId>,
    reacquired: bool,
) {
    seed_liveness_panel_into_ledger(
        &shared.status_panel_controller,
        shared.ui.status_panel_v2_enabled,
        channel_id,
        provider,
        panel_msg_id,
        reacquired,
    );
}

/// Testable seam for [`shadow_adopt_liveness_reacquired_panel`]: the full
/// gate-then-seed logic against an explicit controller + v2 flag (so a directly
/// spawned v2-on controller can exercise the seed path that a v2-off test
/// `SharedData` cannot reach). All three gates live here, so a unit test that
/// removes the seed call fails the positive case.
fn seed_liveness_panel_into_ledger(
    controller: &super::status_panel_controller::StatusPanelController,
    status_panel_v2_enabled: bool,
    channel_id: ChannelId,
    provider: &ProviderKind,
    panel_msg_id: Option<MessageId>,
    reacquired: bool,
) {
    if !reacquired || !status_panel_v2_enabled {
        return;
    }
    let Some(panel) = panel_msg_id else {
        return;
    };
    controller.seed_live_panel(
        watcher_turn_key(channel_id, 0),
        provider.clone(),
        super::status_panel_controller::PanelOwnerKind::Watcher,
        Some(panel),
    );
}

/// EPIC #3078 — the legacy watcher CREATE/ADOPT decision, re-derived from the
/// SAME RAW inputs the `tmux_watcher.rs` ~7213 branch reads, as the parity
/// reference. Mirrors that branch order exactly: a persisted (restart-safe) id
/// adopts, else `watcher_should_create_external_input_status_panel`
/// (v2 on, no live panel, external-input/panel-eligible turn) creates, else
/// nothing. Kept here (not calling the private `tmux_watcher` helper) so the
/// reference and the controller's [`WatcherCreateDecision::watcher_create_parity_decision`]
/// are two INDEPENDENT derivations of the same truth table — that independence is
/// what makes the shadow check meaningful rather than tautological.
fn legacy_watcher_create_decision(
    status_panel_v2_enabled: bool,
    panel_present: bool,
    inflight_represents_external_input: bool,
    persisted_panel_id: Option<MessageId>,
) -> WatcherCreateDecision {
    if let Some(persisted) = persisted_panel_id {
        return WatcherCreateDecision::AdoptPersisted(persisted);
    }
    if status_panel_v2_enabled && !panel_present && inflight_represents_external_input {
        return WatcherCreateDecision::Create;
    }
    WatcherCreateDecision::None
}

/// A create/adopt decision rendered to a stable `(adopt_id, create, none)` shape
/// for the bounded one-shot warn (a `WatcherCreateDecision` is not directly a
/// warn-key, and an `AdoptPersisted` id must distinguish from `Create`/`None`).
fn decision_shape(decision: WatcherCreateDecision) -> (Option<u64>, bool, bool) {
    match decision {
        WatcherCreateDecision::AdoptPersisted(id) => (Some(id.get()), false, false),
        WatcherCreateDecision::Create => (None, true, false),
        WatcherCreateDecision::None => (None, false, true),
    }
}

/// One-shot bound for the CREATE/ADOPT decision parity warn, keyed by
/// `(channel, controller_shape, legacy_shape)` so each distinct divergence logs
/// at most once on a hot watcher loop. Separate from `WATCHER_PARITY_WARNED`
/// (which keys on message-id parity) because the decision shape carries a
/// create/none discriminant a bare id cannot.
type WatcherCreateShape = (u64, (Option<u64>, bool, bool), (Option<u64>, bool, bool));
static WATCHER_CREATE_WARNED: ParityWarnOnce<WatcherCreateShape> = ParityWarnOnce::new();

/// CREATE/ADOPT site: assert the controller's INDEPENDENT create/adopt decision
/// (re-derived from raw inputs) equals the legacy watcher decision. v2-off →
/// inert (the legacy decision is `None` and the controller agrees; no controller
/// actor read is needed — the decision is pure). Never panics in release
/// (`debug_assert` + bounded log-once `warn!`); the legacy path keeps executing
/// the real create/adopt IO.
pub(in crate::services::discord) fn assert_watcher_create_parity(
    shared: &Arc<SharedData>,
    channel_id: ChannelId,
    status_panel_v2_enabled: bool,
    panel_present: bool,
    inflight_represents_external_input: bool,
    persisted_panel_id: Option<MessageId>,
) {
    let controller = shared
        .status_panel_controller
        .watcher_create_parity_decision(
            status_panel_v2_enabled,
            panel_present,
            inflight_represents_external_input,
            persisted_panel_id,
        );
    let legacy = legacy_watcher_create_decision(
        status_panel_v2_enabled,
        panel_present,
        inflight_represents_external_input,
        persisted_panel_id,
    );
    if controller == legacy {
        return;
    }
    debug_assert_eq!(
        controller,
        legacy,
        "#3078 watcher status-panel create/adopt parity mismatch (channel {}): controller chose {controller:?}, legacy chose {legacy:?}",
        channel_id.get()
    );
    let shape = (
        channel_id.get(),
        decision_shape(controller),
        decision_shape(legacy),
    );
    if !WATCHER_CREATE_WARNED.should_warn(shape) {
        return;
    }
    tracing::warn!(
        channel = channel_id.get(),
        site = "create",
        controller_decision = ?controller,
        legacy_decision = ?legacy,
        "#3078 watcher status-panel create/adopt parity mismatch — StatusPanelController would make a different create/adopt decision than the legacy watcher; legacy path executed (no behaviour change), divergence logged once for the later controller-executes cutover"
    );
}

#[cfg(test)]
mod tests {
    //! EPIC #3078 PR-4: confirm the controller's chosen RECLAIM id equals the
    //! legacy watcher decision (orphan reclaim, #3003 channel-only turn-aware),
    //! that the parity assert never fires for matching ids, that the warn is
    //! bounded, and that the SharedData-gated wrapper short-circuits when v2 is
    //! off (default test SharedData: v2 off, controller actor NOT spawned — so
    //! the awaited read must never be reached). CREATE/ADOPT and COMPLETION
    //! parity are deferred to the controller execute-cutover PR (see module doc).
    use super::*;
    use crate::services::discord::status_panel_controller::StatusPanelController;

    /// Orphan reclaim: the controller's reclaim target IS the persisted panel id
    /// (channel-only key, #3003 turn-aware), agreeing with the legacy watcher
    /// cleanup target.
    #[tokio::test(flavor = "current_thread")]
    async fn controller_reclaim_target_matches_legacy() {
        let ctl = StatusPanelController::spawn(true);
        let key = TurnKey::new(ChannelId::new(4004), 0, 0);
        let legacy = MessageId::new(7004);
        let target = ctl
            .sweeper_reclaim_parity_id(key, ProviderKind::Claude, Some(legacy))
            .await;
        assert_eq!(target, Some(legacy), "reclaim target must match legacy");
        assert_parity("reclaim", target, Some(legacy), 4004);
    }

    /// v2-off: the SharedData-gated wrapper must return BEFORE the awaited
    /// controller read. The default test SharedData has v2 off and an UNSPAWNED
    /// controller, so reaching the read would hang on an ack that is never
    /// answered — completing without hang/panic proves the short-circuit.
    #[tokio::test(flavor = "current_thread")]
    async fn v2_off_short_circuits_without_panic() {
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        assert!(!shared.ui.status_panel_v2_enabled);
        let channel = ChannelId::new(4005);
        assert_watcher_reclaim_parity(
            &shared,
            channel,
            &ProviderKind::Claude,
            MessageId::new(8003),
        )
        .await;
    }

    /// The bounded guard logs a given `(channel, site, controller, legacy)` shape
    /// at most once (the parity warn cannot flood a hot watcher loop).
    #[test]
    fn warn_is_bounded_once_per_shape() {
        let shape: WatcherShape = (4999, "reclaim", Some(1), Some(2));
        assert!(super::WATCHER_PARITY_WARNED.should_warn(shape));
        assert!(!super::WATCHER_PARITY_WARNED.should_warn(shape));
    }

    /// EPIC #3078 CREATE/ADOPT: the controller's independent decision agrees with
    /// the legacy reference across the full truth table — persisted adopt,
    /// create (v2 on / no panel / external-input), and each `None` branch —
    /// driven through the SharedData-backed `assert_watcher_create_parity` so the
    /// real wiring (no panic, no spurious warn) is exercised.
    #[tokio::test(flavor = "current_thread")]
    async fn create_parity_agrees_across_truth_table() {
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        let channel = ChannelId::new(5001);
        let persisted = MessageId::new(6001);

        // Persisted id → adopt (matching on both sides → no warn/panic).
        assert_watcher_create_parity(&shared, channel, true, false, true, Some(persisted));
        // Eligible external-input turn, no panel, v2 on → create.
        assert_watcher_create_parity(&shared, channel, true, false, true, None);
        // v2 off / panel present / not-external → none (each agrees).
        assert_watcher_create_parity(&shared, channel, false, false, true, None);
        assert_watcher_create_parity(&shared, channel, true, true, true, None);
        assert_watcher_create_parity(&shared, channel, true, false, false, None);

        // The two independent derivations agree element-for-element.
        for &(v2, present, ext, id) in &[
            (true, false, true, Some(persisted)),
            (true, false, true, None),
            (false, false, true, None),
            (true, true, true, None),
            (true, false, false, None),
        ] {
            assert_eq!(
                shared
                    .status_panel_controller
                    .watcher_create_parity_decision(v2, present, ext, id),
                legacy_watcher_create_decision(v2, present, ext, id),
            );
        }
    }

    /// The CREATE/ADOPT decision warn is bounded once per distinct
    /// `(channel, controller_shape, legacy_shape)` divergence.
    #[test]
    fn create_warn_is_bounded_once_per_shape() {
        let shape: WatcherCreateShape = (5099, (None, true, false), (None, false, true));
        assert!(super::WATCHER_CREATE_WARNED.should_warn(shape));
        assert!(!super::WATCHER_CREATE_WARNED.should_warn(shape));
    }

    /// PR-6 liveness seed, v2-off: the public SharedData wrapper must short-circuit
    /// BEFORE the controller send even when `reacquired == true` and a panel id is
    /// present. The default test SharedData has v2 off; completing without panic
    /// proves the wrapper threads the flag (the seed is purely additive, so
    /// off == inert).
    #[tokio::test(flavor = "current_thread")]
    async fn liveness_seed_v2_off_wrapper_is_inert() {
        let shared = super::super::make_shared_data_for_tests_with_storage(None, None);
        assert!(!shared.ui.status_panel_v2_enabled);
        shadow_adopt_liveness_reacquired_panel(
            &shared,
            ChannelId::new(4006),
            &ProviderKind::Claude,
            Some(MessageId::new(8006)),
            true,
        );
    }

    /// PR-6 liveness seed mechanism (POSITIVE, v2-on): the full helper seam against
    /// a spawned controller seeds the reacquired watcher panel under the channel-
    /// only synthetic key, and `current_panel` reads it back. Removing the seed
    /// call from `seed_liveness_panel_into_ledger` fails this assertion.
    #[tokio::test(flavor = "current_thread")]
    async fn liveness_seed_writes_watcher_panel_into_ledger() {
        let ctl = StatusPanelController::spawn(true);
        let channel = ChannelId::new(4007);
        let panel = MessageId::new(7007);
        seed_liveness_panel_into_ledger(
            &ctl,
            true,
            channel,
            &ProviderKind::Claude,
            Some(panel),
            true,
        );
        assert_eq!(
            ctl.current_panel(watcher_turn_key(channel, 0)).await,
            Some(panel),
            "liveness-reacquired watcher panel must seed the controller ledger"
        );
    }

    /// PR-6 reused-key OVERWRITE (the Finding-1 fix): a second same-generation
    /// liveness seed with a DIFFERENT panel must win — `seed_live_panel` overwrites
    /// the non-terminal entry rather than first-id-wins, so the ledger tracks the
    /// LATEST liveness panel, never a stale earlier one.
    #[tokio::test(flavor = "current_thread")]
    async fn liveness_seed_overwrites_stale_reused_key() {
        let ctl = StatusPanelController::spawn(true);
        let channel = ChannelId::new(4008);
        let panel_a = MessageId::new(7008);
        let panel_b = MessageId::new(7009);
        seed_liveness_panel_into_ledger(
            &ctl,
            true,
            channel,
            &ProviderKind::Claude,
            Some(panel_a),
            true,
        );
        seed_liveness_panel_into_ledger(
            &ctl,
            true,
            channel,
            &ProviderKind::Claude,
            Some(panel_b),
            true,
        );
        assert_eq!(
            ctl.current_panel(watcher_turn_key(channel, 0)).await,
            Some(panel_b),
            "the latest liveness panel must overwrite the stale earlier one"
        );
    }

    /// PR-6 gates at the seam (v2-on controller, so a leaked send would be
    /// observable): `reacquired == false` and `panel == None` each short-circuit
    /// before the seed — the ledger stays empty.
    #[tokio::test(flavor = "current_thread")]
    async fn liveness_seed_reacquired_and_none_gates_are_inert() {
        let ctl = StatusPanelController::spawn(true);
        let channel = ChannelId::new(4009);
        // reacquired == false: a lost save race must not seed.
        seed_liveness_panel_into_ledger(
            &ctl,
            true,
            channel,
            &ProviderKind::Claude,
            Some(MessageId::new(7010)),
            false,
        );
        // panel == None: nothing to seed.
        seed_liveness_panel_into_ledger(&ctl, true, channel, &ProviderKind::Claude, None, true);
        assert_eq!(
            ctl.current_panel(watcher_turn_key(channel, 0)).await,
            None,
            "neither a lost reacquire race nor a missing panel may seed the ledger"
        );
    }
}
