//! The names a pane veto or an unconfirmed paste reports under, for each TUI runtime.

use super::{NOT_BUSY, codex_inject};
use crate::services::claude_tui::busy_inject::Veto;

pub(super) fn veto_name(veto: Veto) -> &'static str {
    match veto {
        Veto::InvalidInput => "invalid_input",
        Veto::LockContended => "lock_contended",
        Veto::PaneUnavailable => "pane_unavailable",
        Veto::Modal => "modal",
        Veto::NotBusy => "not_busy",
        Veto::TranscriptUnavailable => "transcript_unavailable",
        Veto::LoadFailed => "load_failed",
    }
}

pub(super) fn codex_veto_name(veto: codex_inject::Veto) -> &'static str {
    use codex_inject::Veto;
    match veto {
        Veto::InvalidInput => "invalid_input",
        Veto::LockContended => "lock_contended",
        Veto::PaneUnavailable => "pane_unavailable",
        Veto::Modal => "modal",
        Veto::ComposerUnread => "composer_unread",
        Veto::DraftNavigationUnproven => "draft_navigation_unproven",
        Veto::DraftSelectionUnproven => "draft_selection_unproven",
        Veto::DraftCommandUnproven => "draft_command_unproven",
        Veto::DraftPasteTransformUnproven => "draft_paste_transform_unproven",
        Veto::DraftPlaceholderUnproven => "draft_placeholder_unproven",
        Veto::DraftNotFullyVisible => "draft_not_fully_visible",
        Veto::DraftImagePositionUnproven => "draft_image_position_unproven",
        Veto::NotSteerable => "not_steerable",
        Veto::NotBusy => NOT_BUSY,
        Veto::TurnUnknown => "turn_unknown",
        Veto::TurnChanged => "turn_changed",
        Veto::TranscriptUnavailable => "transcript_unavailable",
        Veto::LoadFailed => "load_failed",
        Veto::LedgerFull => "ledger_full",
        Veto::AmbiguousIdenticalPending => "ambiguous_identical_pending",
    }
}

pub(super) fn codex_unconfirmed_name(detail: codex_inject::Unconfirmed) -> &'static str {
    use codex_inject::Unconfirmed;
    match detail {
        Unconfirmed::PasteFailed => "paste_failed",
        Unconfirmed::EnterFailed => "enter_failed",
    }
}
