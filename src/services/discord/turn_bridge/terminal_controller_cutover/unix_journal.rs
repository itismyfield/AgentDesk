//! #5071 T1 S4 — the cutover family's single door to the delivery journal and
//! the `cfg` boundary it stands on. `session_relay_sink`'s journal controller
//! is `#[cfg(unix)]`; `turn_bridge` compiles everywhere, so this module keeps
//! that boundary in ONE place instead of a `#[cfg(unix)]` on each of the
//! anchor's four begins and six settles.
//!
//! On non-unix this family is UNINSTRUMENTED, not a no-op: the durable
//! delivered-frontier writes in `terminal_controller_cutover.rs` are not
//! `cfg`-gated, only the observing journal is, so no row for this family can
//! ever exist there. [`NoJournalOnThisPlatform`] is uninhabited, so an
//! observation off unix cannot be constructed — nothing may be added to
//! these `#[cfg(not(unix))]` items.
//!
//! The anchor imports this module under its own name so `rg 'unix_journal::'`
//! enumerates the family, except `begin_pinned_terminal` (`#[cfg(unix)]`)
//! aliases these two functions as `journal_begin`/`journal_settle` — any count
//! by that pattern must also match those aliases. The actual gate is
//! `scripts/check_delivery_journal_raw_writer.py` (unix-only verdict) plus
//! `test_source_contract_turn_bridge_reaches_the_journal_through_one_cfg_gated_door`.

#[cfg(unix)]
pub(super) use super::super::super::session_relay_sink::journal::controller::{
    ControllerDisposition as Disposition, begin_controller_terminal, settle_controller_terminal,
};

/// Uninhabited: the type-level statement that this family holds no journal
/// obligation off unix.
#[cfg(not(unix))]
pub(super) enum NoJournalOnThisPlatform {}

/// Mirrors [`super::super::super::session_relay_sink::journal::controller::ControllerDisposition`]
/// so the anchor's call sites keep naming which durable writer they sit
/// beside — naming a site is not observing it.
///
/// Unix has four `Disposition`-naming sites in `terminal_controller_cutover.rs`
/// (`:31`, `:265`, `:392`, `:558`); only the last three compile on non-unix
/// (`:31` is inside `begin_pinned_terminal`, `#[cfg(unix)]`). This mirror
/// backs exactly those three.
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
