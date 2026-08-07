//! #5071 T1 S5 — the recovery family's single door to the delivery journal, and
//! the `cfg` boundary that door stands on.
//!
//! Same construction, same reasons, same rules as
//! `turn_bridge/terminal_controller_cutover/unix_journal.rs` (#5071 T1 S4) —
//! read that file for the full argument; only what is specific to this family is
//! restated here.
//!
//! `session_relay_sink` is `#[cfg(unix)]` (`services/discord/mod.rs`) while `mod
//! recovery_engine`, `mod recovery_paths` and `mod outbound` are not, so a direct
//! reference from any of them is a hard `E0433` on `windows-latest` — which is
//! exactly how S4 first landed. This module is the ONE place that reference
//! exists, instead of `#[cfg(unix)]` scattered over
//! `terminal_text_idempotency.rs`'s begin and three settles. It is also why
//! `recovery_paths/controller_cutover.rs`, in a third ungated subtree, needs no
//! `cfg` of its own: it names only
//! `RecoveryDeliveryContext::record_successful_fresh_send_after_controller_edit_fallback`,
//! a method whose journal types never appear in its signature.
//!
//! ## On non-unix this family is UNINSTRUMENTED — that is not the same as a no-op
//!
//! The durable write this family observes is NOT `cfg`-gated:
//! `delivery_record::record_recovery_terminal_delivery` compiles on every
//! target, as did the three raw calls it replaced in #5071 T1 S7
//! (`delivery_record::write_delivered_frontier`,
//! `delivery_record::write_proven_gone_equal_range_frontier` and
//! `completed_turn_ledger::append_completed_turn`).
//! The journal that observes them is gated. So on non-unix the recovery frontier
//! advances with no shadow observation at all, and no row for this family can
//! ever exist there.
//!
//! The `#[cfg(not(unix))]` items below are that absence written down, not
//! instrumentation that happens to do nothing. [`NoJournalOnThisPlatform`] is an
//! uninhabited enum, so `Option<NoJournalOnThisPlatform>` has exactly one
//! inhabitant — `None`. Off unix an observation is not merely never made; it
//! cannot be constructed, and nothing may be added to those items.
//!
//! That has to survive a reader who never opens this file, so the anchor imports
//! the module under its own name instead of aliasing it: every call site reads
//! `unix_journal::begin_recovery_terminal(..)` /
//! `unix_journal::settle_recovery_terminal(..)`.
//!
//! `scripts/check_delivery_journal_raw_writer.py` is a text scan and cannot see
//! any of this: read its `uninstrumented families: 1/6` as a unix-only
//! enumeration. What holds this boundary is
//! `test_source_contract_recovery_reaches_the_journal_through_one_cfg_gated_door`
//! in `tests/test_delivery_journal_raw_writer.py`, run by
//! `scripts/ci-script-checks.sh`.

#[cfg(unix)]
pub(in crate::services::discord) use super::super::session_relay_sink::journal::recovery::{
    RecoveryDisposition as Disposition, RecoverySettlement as Settlement, begin_recovery_terminal,
    settle_recovery_terminal,
};

/// The non-unix observation type: an uninhabited enum, so the only value of
/// `Option<NoJournalOnThisPlatform>` is `None`. This is the type-level statement
/// that the recovery family holds no journal obligation off unix.
#[cfg(not(unix))]
pub(in crate::services::discord) enum NoJournalOnThisPlatform {}

/// Mirrors
/// [`super::super::session_relay_sink::journal::recovery::RecoveryDisposition`]
/// so the anchor's call site keeps naming which entry point confirmed the
/// delivery on every target. Naming a site is not observing it.
#[cfg(not(unix))]
#[derive(Clone, Copy)]
pub(in crate::services::discord) enum Disposition {
    NoAnchorFreshSend,
    AnchoredEditFallback,
    ControllerEditFallback,
}

/// Mirrors
/// [`super::super::session_relay_sink::journal::recovery::RecoverySettlement`]
/// for the same reason: the funnel's exits keep naming themselves off unix, they
/// just have nothing to name themselves to.
#[cfg(not(unix))]
#[derive(Clone, Copy)]
pub(in crate::services::discord) enum Settlement {
    FrontierPersisted,
    AnchorBindNotPersisted,
    NoTmuxSessionName,
    NoGenerationMarker,
    DurableWriteFailed,
    FrontierResetDuringDelivery,
    DeliveryNotRecorded,
}

/// There is no journal to open. Returns `None` because no other value exists.
#[cfg(not(unix))]
pub(in crate::services::discord) fn begin_recovery_terminal(
    _shared: &crate::services::discord::SharedData,
    _provider: &crate::services::provider::ProviderKind,
    _disposition: Disposition,
    _channels: (
        poise::serenity_prelude::ChannelId,
        poise::serenity_prelude::ChannelId,
    ),
    _tmux_session_name: Option<&str>,
    _current_msg_id: u64,
    _range: Option<(u64, u64)>,
) -> Option<NoJournalOnThisPlatform> {
    None
}

/// There is no obligation to settle: the argument is uninhabited under the
/// `Option`, so this cannot be reached with anything to close.
#[cfg(not(unix))]
pub(in crate::services::discord) fn settle_recovery_terminal(
    _observation: &mut Option<NoJournalOnThisPlatform>,
    _anchor_msg_id: Option<poise::serenity_prelude::MessageId>,
    _settlement: Settlement,
) {
}
