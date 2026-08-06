//! #5175 soft-terminal delivery-authority contract.
//!
//! Split out of `turn_identity.rs` so that module stays inside the
//! `src/services/discord/tmux_watcher/**` namespace size cap. The parent
//! re-exports these items, so callers still reach them through the watcher's
//! usual `use turn_identity::*` glob.

/// #5175: the conjunct that denied soft-terminal delivery authority.
///
/// The denial used to be a single opaque `route="soft_terminal_no_authority"`
/// string, which is why a channel that lost EVERY terminal body for a week
/// still read `gap 0 / wedge 0` to the watchdog. Naming the failing conjunct
/// makes the loss greppable, alertable, and attributable to one contract term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SoftTerminalAuthorityDenial {
    /// No inflight row survived to the pre-relay read.
    NoInflightRow,
    /// The row names a different tmux session than the relaying watcher.
    SessionMismatch,
    /// The row's turn did not start inside the byte range this frame covers.
    TurnStartOutsideFrame,
    /// The row is ownerless (`RelayOwnerKind::None`) — e.g. a bare recovery row.
    RelayOwnerNone,
    /// The row carries no `turn_nonce` at all.
    TurnNonceMissing,
    /// The row's nonce is not the nonce this watcher bound to the turn it consumed.
    TurnNonceMismatch,
}

impl SoftTerminalAuthorityDenial {
    /// Stable, exact-match-friendly label for a dedicated log field.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NoInflightRow => "no_inflight_row",
            Self::SessionMismatch => "session_mismatch",
            Self::TurnStartOutsideFrame => "turn_start_outside_frame",
            Self::RelayOwnerNone => "relay_owner_none",
            Self::TurnNonceMissing => "turn_nonce_missing",
            Self::TurnNonceMismatch => "turn_nonce_mismatch",
        }
    }

    /// Flight-recorder `route` label. Keeps the historical
    /// `soft_terminal_no_authority` prefix so existing prefix greps and
    /// dashboards keep matching, and appends the failing conjunct.
    pub(crate) fn route_label(self) -> &'static str {
        match self {
            Self::NoInflightRow => "soft_terminal_no_authority:no_inflight_row",
            Self::SessionMismatch => "soft_terminal_no_authority:session_mismatch",
            Self::TurnStartOutsideFrame => "soft_terminal_no_authority:turn_start_outside_frame",
            Self::RelayOwnerNone => "soft_terminal_no_authority:relay_owner_none",
            Self::TurnNonceMissing => "soft_terminal_no_authority:turn_nonce_missing",
            Self::TurnNonceMismatch => "soft_terminal_no_authority:turn_nonce_mismatch",
        }
    }

    /// Per-conjunct root-cause counter name.
    pub(crate) fn metric_name(self) -> &'static str {
        match self {
            Self::NoInflightRow => "relay_terminal_authority_denied_no_inflight_row",
            Self::SessionMismatch => "relay_terminal_authority_denied_session_mismatch",
            Self::TurnStartOutsideFrame => {
                "relay_terminal_authority_denied_turn_start_outside_frame"
            }
            Self::RelayOwnerNone => "relay_terminal_authority_denied_relay_owner_none",
            Self::TurnNonceMissing => "relay_terminal_authority_denied_turn_nonce_missing",
            Self::TurnNonceMismatch => "relay_terminal_authority_denied_turn_nonce_mismatch",
        }
    }
}

/// #5175: the watcher's turn-identity binding for the frame being relayed.
///
/// Captured once at turn-stream exit and carried into the terminal relay plan
/// so soft-terminal authority is decided against the inflight row that exists
/// WHEN THE TURN ENDS (`inflight_before_relay`), not against the
/// `startup_inflight_snapshot` taken BEFORE the turn produced a single byte.
///
/// A TUI-direct turn cannot satisfy the snapshot rule by construction: the
/// watcher re-enters the turn-stream collector as soon as the previous turn
/// ends, so the snapshot is `None` on the first turn (conjunct #1) and belongs
/// to the PREVIOUS turn on every later one (the exact-offset conjunct #2).
/// Deciding on the snapshot therefore denied delivery authority to every
/// TUI-direct soft terminal permanently, and because the session-bound sink
/// assumes "the watcher owns terminal delivery" whenever it does not, NOBODY
/// posted the body — the frontier froze and redrive re-published stale answers.
///
/// The nonce carried here is the watcher's OWN binding
/// (`refresh_watcher_turn_identity`, refreshed at poll entry), captured
/// strictly before the pre-relay inflight read. Authenticating the pre-relay
/// row against it is therefore NOT self-authenticating: a `/compact` rewrite
/// or a newer turn that replaced the row mid-frame carries a different nonce
/// and is still refused.
pub(crate) struct WatcherSoftTerminalAuthority {
    tmux_session_name: String,
    data_start_offset: u64,
    watcher_turn_nonce: Option<String>,
    /// Verdict the pre-#5175 rule would have produced against the pre-turn
    /// startup snapshot. Telemetry ONLY — it never gates delivery again.
    startup_snapshot_authorized: bool,
}

/// Capture the watcher's turn-identity binding for this frame.
///
/// Despite the historical name this no longer DECIDES authority: the decision
/// moved to [`WatcherSoftTerminalAuthority::authorize_pre_relay_inflight`],
/// which runs at the relay-plan seam where the turn's own inflight row is
/// loaded. The pre-turn `state` snapshot is still evaluated under the legacy
/// exact-offset rule and retained as a telemetry-only field so the flight
/// recorder can show the old verdict next to the new one.
pub(crate) fn watcher_soft_terminal_has_turn_authority(
    state: Option<&crate::services::discord::inflight::InflightTurnState>,
    tmux_session_name: &str,
    data_start_offset: u64,
    watcher_turn_nonce: Option<&str>,
) -> WatcherSoftTerminalAuthority {
    let startup_snapshot_authorized = state.is_some_and(|state| {
        state.tmux_session_name.as_deref() == Some(tmux_session_name)
            && state.last_offset.max(state.turn_start_offset.unwrap_or(0)) == data_start_offset
            && !matches!(
                state.effective_relay_owner_kind(),
                crate::services::discord::inflight::RelayOwnerKind::None
            )
            && state.turn_nonce.as_deref().is_some()
            && state.turn_nonce.as_deref() == watcher_turn_nonce
    });
    WatcherSoftTerminalAuthority {
        tmux_session_name: tmux_session_name.to_string(),
        data_start_offset,
        watcher_turn_nonce: watcher_turn_nonce.map(str::to_owned),
        startup_snapshot_authorized,
    }
}

impl WatcherSoftTerminalAuthority {
    /// Pre-#5175 verdict against the pre-turn startup snapshot. Telemetry only.
    pub(crate) fn startup_snapshot_authorized(&self) -> bool {
        self.startup_snapshot_authorized
    }

    /// #5175: decide soft-terminal delivery authority against the inflight row
    /// loaded immediately before this relay.
    ///
    /// Four of the five conjuncts are unchanged from the snapshot rule and are
    /// what still refuses a `/compact`-forged soft boundary: the row must name
    /// THIS tmux session, must not be ownerless, must carry a `turn_nonce`, and
    /// that nonce must be the one this watcher bound to the turn it consumed.
    ///
    /// The offset conjunct is the one that moved: exact equality against a
    /// snapshot resume floor became RANGE CONTAINMENT of the row's turn start,
    /// `data_start_offset <= turn_start_offset < current_offset`. That is the
    /// same expression `pinned_finalize_user_msg_id` and the watcher-yield
    /// guard (`tmux.rs` `watcher_should_yield_to_inflight_state`) already use,
    /// so the three cannot disagree, and it survives a redrive rewind. It stays
    /// discriminating in both directions: a historical row that started BEFORE
    /// this frame and a newer follow-up row that starts AT/AFTER the consumed
    /// offset are both outside the range and refused.
    pub(crate) fn authorize_pre_relay_inflight(
        &self,
        inflight_before_relay: Option<&crate::services::discord::inflight::InflightTurnState>,
        current_offset: u64,
    ) -> Result<(), SoftTerminalAuthorityDenial> {
        let Some(state) = inflight_before_relay else {
            return Err(SoftTerminalAuthorityDenial::NoInflightRow);
        };
        if state.tmux_session_name.as_deref().map(str::trim) != Some(self.tmux_session_name.trim())
        {
            return Err(SoftTerminalAuthorityDenial::SessionMismatch);
        }
        let turn_start_offset = state.turn_start_offset.unwrap_or(state.last_offset);
        if turn_start_offset < self.data_start_offset || turn_start_offset >= current_offset {
            return Err(SoftTerminalAuthorityDenial::TurnStartOutsideFrame);
        }
        if matches!(
            state.effective_relay_owner_kind(),
            crate::services::discord::inflight::RelayOwnerKind::None
        ) {
            return Err(SoftTerminalAuthorityDenial::RelayOwnerNone);
        }
        let Some(row_turn_nonce) = state.turn_nonce.as_deref() else {
            return Err(SoftTerminalAuthorityDenial::TurnNonceMissing);
        };
        if Some(row_turn_nonce) != self.watcher_turn_nonce.as_deref() {
            return Err(SoftTerminalAuthorityDenial::TurnNonceMismatch);
        }
        Ok(())
    }
}
