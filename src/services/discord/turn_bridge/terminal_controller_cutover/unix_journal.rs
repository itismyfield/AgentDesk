//! #5071 T1 S4 — the cutover family's single door to the delivery journal, and
//! the `cfg` boundary that door stands on.
//!
//! `session_relay_sink` is `#[cfg(unix)]` (`services/discord/mod.rs`), so
//! `session_relay_sink::journal::controller` does not exist on non-unix targets.
//! `turn_bridge` carries no such gate and compiles everywhere. S4 first landed a
//! direct `use super::super::session_relay_sink::journal::controller::…` in
//! `terminal_controller_cutover.rs`, which is a hard build break on
//! `windows-latest` (`E0433: could not find session_relay_sink in super`).
//! Routing the family through this module puts that boundary in ONE place
//! instead of nine `#[cfg(unix)]` attributes scattered over the anchor file's
//! three begins and five settles.
//!
//! ## On non-unix this family is UNINSTRUMENTED — that is not the same as a no-op
//!
//! The three durable delivered-frontier writes in
//! `terminal_controller_cutover.rs` — one `dr::shadow_mirror_delivered_frontier`
//! and two `dr::record_long_chunk_terminal_delivery` — are NOT `cfg`-gated: they
//! compile on every target. The journal that observes them is gated. So on
//! non-unix the frontier advances with no shadow observation at all, and no row
//! for this family can ever exist there.
//!
//! The `#[cfg(not(unix))]` items below are that absence written down, not
//! instrumentation that happens to do nothing. [`NoJournalOnThisPlatform`] is an
//! uninhabited enum, so `Option<NoJournalOnThisPlatform>` has exactly one
//! inhabitant — `None`. Off unix an observation is not merely never made; it
//! cannot be constructed. Nothing may be added to those items: an observation
//! that only some platforms make must never be spelled the way one that every
//! platform makes is spelled.
//!
//! Both facts have to survive a reader who never opens this file, which is why
//! the anchor imports the module under its own name instead of aliasing it:
//! every call site reads `unix_journal::begin_controller_terminal(..)` /
//! `unix_journal::settle_controller_terminal(..)`, so the platform bound sits in
//! the identifier at all eight of them.
//!
//! ## The family gate cannot see any of this
//!
//! `scripts/check_delivery_journal_raw_writer.py` decides "instrumented" by
//! matching facade-call TEXT in the anchor file. It does not parse Rust, does
//! not evaluate `cfg`, and has no notion of a target, so its verdict for this
//! family is byte-identical on every platform and means only "instrumented on
//! unix". Read its `uninstrumented families: 2/6` as a unix-only enumeration,
//! never as a claim about the build that actually broke. What holds this
//! boundary is
//! `test_source_contract_turn_bridge_reaches_the_journal_through_one_cfg_gated_door`
//! in `tests/test_delivery_journal_raw_writer.py`, run by
//! `scripts/ci-script-checks.sh`.

#[cfg(unix)]
pub(super) use super::super::super::session_relay_sink::journal::controller::{
    ControllerDisposition as Disposition, begin_controller_terminal, settle_controller_terminal,
};

/// The non-unix observation type: an uninhabited enum, so the only value of
/// `Option<NoJournalOnThisPlatform>` is `None`. This is the type-level statement
/// that the cutover family holds no journal obligation off unix.
#[cfg(not(unix))]
pub(super) enum NoJournalOnThisPlatform {}

/// Mirrors [`super::super::super::session_relay_sink::journal::controller::ControllerDisposition`]
/// so the anchor's three call sites keep naming which durable writer they sit
/// beside on every target. Naming a site is not observing it.
#[cfg(not(unix))]
pub(super) enum Disposition {
    ShortReplace,
    LongChunks,
    LongChunksLegacy,
}

/// There is no journal to open. Returns `None` because no other value exists.
#[cfg(not(unix))]
pub(super) fn begin_controller_terminal(
    _shared: &crate::services::discord::SharedData,
    _provider: &crate::services::provider::ProviderKind,
    _disposition: Disposition,
    _channels: (
        poise::serenity_prelude::ChannelId,
        poise::serenity_prelude::ChannelId,
    ),
    _range: Option<(u64, u64)>,
) -> Option<NoJournalOnThisPlatform> {
    None
}

/// There is no obligation to settle: the argument is uninhabited under the
/// `Option`, so this cannot be reached with anything to close.
#[cfg(not(unix))]
pub(super) fn settle_controller_terminal(
    _observation: &mut Option<NoJournalOnThisPlatform>,
    _anchor_msg_id: Option<poise::serenity_prelude::MessageId>,
    _committed: bool,
) {
}
