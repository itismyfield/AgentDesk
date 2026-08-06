//! #5175 soft-terminal delivery-authority tests for the terminal relay plan.
//!
//! Split out of `terminal_relay_plan.rs` to keep that module inside the
//! `src/services/discord/tmux_watcher/**` namespace size cap.

use super::*;
use crate::services::discord::inflight::RelayOwnerKind;

const SESSION: &str = "AgentDesk-claude-adk-cc";
const FRAME_START: u64 = 1_534_426;
const TURN_START: u64 = 1_534_500;
const FRAME_END: u64 = 1_650_085;
const WATCHER_NONCE: &str = "nonce-bound-while-consuming-this-turn";

fn row(turn_nonce: Option<&str>, owner: RelayOwnerKind) -> InflightTurnState {
    let mut state = InflightTurnState::new(
        ProviderKind::Claude,
        42,
        Some("adk-cc".to_string()),
        7,
        0,
        0,
        "prompt".to_string(),
        None,
        Some(SESSION.to_string()),
        Some("/tmp/out.jsonl".to_string()),
        Some("/tmp/in.fifo".to_string()),
        TURN_START,
    );
    state.turn_start_offset = Some(TURN_START);
    state.turn_nonce = turn_nonce.map(str::to_owned);
    state.set_relay_owner_kind(owner);
    state
}

/// The binding a TUI-direct turn produces: the pre-turn startup snapshot is
/// absent, so the pre-#5175 verdict is false.
fn tui_direct_binding() -> WatcherSoftTerminalAuthority {
    watcher_soft_terminal_has_turn_authority(None, SESSION, FRAME_START, Some(WATCHER_NONCE))
}

#[test]
fn soft_terminal_authority_reads_the_pre_relay_row_not_the_startup_snapshot_5175() {
    let binding = tui_direct_binding();
    assert!(!binding.startup_snapshot_authorized());

    let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
        &binding,
        Some(&row(Some(WATCHER_NONCE), RelayOwnerKind::Watcher)),
        FRAME_END,
        Some(WatcherTerminalKind::SoftStopHookSummary),
    );

    assert!(
        authorized,
        "a TUI-direct soft terminal must be authorized by the inflight row that exists at turn end"
    );
    assert_eq!(denial, None);
}

#[test]
fn missing_pre_relay_row_denies_soft_terminal_direct_send_5175() {
    let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
        &tui_direct_binding(),
        None,
        FRAME_END,
        Some(WatcherTerminalKind::SoftStopHookSummary),
    );

    assert!(!authorized);
    assert_eq!(denial, Some(SoftTerminalAuthorityDenial::NoInflightRow));
}

#[test]
fn forged_soft_terminal_is_denied_even_when_the_startup_snapshot_authorized_5175() {
    // The snapshot verdict is TRUE here (exact resume-floor match on the
    // pre-turn snapshot). If the decision still consulted it, a forged
    // ownerless row at turn end would be waved through.
    let mut snapshot = row(Some(WATCHER_NONCE), RelayOwnerKind::Watcher);
    snapshot.turn_start_offset = Some(FRAME_START);
    snapshot.last_offset = FRAME_START;
    let binding = watcher_soft_terminal_has_turn_authority(
        Some(&snapshot),
        SESSION,
        FRAME_START,
        Some(WATCHER_NONCE),
    );
    assert!(binding.startup_snapshot_authorized());

    let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
        &binding,
        Some(&row(Some(WATCHER_NONCE), RelayOwnerKind::None)),
        FRAME_END,
        Some(WatcherTerminalKind::SoftStopHookSummary),
    );

    assert!(!authorized);
    assert_eq!(denial, Some(SoftTerminalAuthorityDenial::RelayOwnerNone));
}

#[test]
fn compact_forged_nonce_is_denied_at_the_direct_send_seam_5175() {
    let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
        &tui_direct_binding(),
        Some(&row(
            Some("compact-rewritten-nonce"),
            RelayOwnerKind::Watcher,
        )),
        FRAME_END,
        Some(WatcherTerminalKind::SoftStopHookSummary),
    );

    assert!(!authorized);
    assert_eq!(denial, Some(SoftTerminalAuthorityDenial::TurnNonceMismatch));
}

#[test]
fn hard_result_terminal_keeps_its_recovery_fallback_and_reports_no_denial_5175() {
    // Control group: the `hard_result` watcher_direct lane that already
    // worked on other channels must stay authorized with no inflight row at
    // all, and must not be blamed for a soft-contract denial.
    for terminal_kind in [Some(WatcherTerminalKind::HardResult), None] {
        let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
            &tui_direct_binding(),
            None,
            FRAME_END,
            terminal_kind,
        );
        assert!(authorized, "hard terminal fallback must be preserved");
        assert_eq!(denial, None);
    }
}

#[test]
fn production_call_site_feeds_the_pre_relay_inflight_row_5175() {
    // The unit tests above pin the decision; this pins the WIRING, which is
    // where #5175 actually lived. Rewiring the call site back to the
    // pre-turn snapshot (or starving it of the row) must not be silent.
    let source = include_str!("terminal_relay_plan.rs");
    let call_site = source
        .split_once("let (watcher_direct_fallback_authorized, soft_terminal_authority_denial) =")
        .expect("the terminal relay plan must decide soft-terminal authority")
        .1
        .split_once(");")
        .expect("the authority call must terminate")
        .0;
    assert!(
        call_site.contains("watcher_soft_terminal_direct_send_authority("),
        "authority must be decided by the seam these tests cover"
    );
    assert!(
        call_site.contains("inflight_before_relay.as_ref()"),
        "authority must be decided against the PRE-RELAY inflight row (#5175)"
    );
    assert!(
        call_site.contains("current_offset"),
        "the offset containment term needs the consumed offset (#5175)"
    );
}
