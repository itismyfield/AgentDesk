//! #5175 soft-terminal delivery-authority tests for the terminal relay plan.
//!
//! Split out of `terminal_relay_plan.rs` to keep that module inside the
//! `src/services/discord/tmux_watcher/**` namespace size cap.

use super::rowless_delivery_authority::{lease_has_live_holder, ledger_owes_output};
use super::*;
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::discord::{DeliveryLeaseKey, LeaseHolder, LeaseOutcome, LeaseSnapshot};

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
        RowlessDeliveryAuthority::default(),
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
        RowlessDeliveryAuthority::default(),
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
        RowlessDeliveryAuthority::default(),
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
        RowlessDeliveryAuthority::default(),
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
            RowlessDeliveryAuthority::default(),
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

// ---------------------------------------------------------------------------
// #5464 T5 C1 — `no_inflight_row` is a STRUCTURAL signal, not a delivery verdict.
//
// T5 AC1: the absence of a durable inflight row does not end Discord delivery
// authority; authority is derived from the DeliveryJournal's OutputObligation
// and the delivery lease. The 27-hour live sample that opened C1 counted 150
// `soft_terminal_denial="no_inflight_row"` frames (of 753 `NO delivery owner`)
// with `inflight_present=false` — terminal bodies that reached no channel.
// ---------------------------------------------------------------------------

/// Both AC1 operands present, inside the enforcement cohort.
fn full_rowless_authority() -> RowlessDeliveryAuthority {
    RowlessDeliveryAuthority {
        cohort_admits: true,
        ledger_obligation_open: true,
        delivery_lease_present: true,
    }
}

#[test]
fn rowless_soft_terminal_stays_a_delivery_candidate_on_a_ledger_obligation_5464_c1() {
    // The audit's closing scenario: row absent, but the ledger still owes output
    // for this frame. The structural signal must not end delivery on its own.
    let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
        &tui_direct_binding(),
        None,
        FRAME_END,
        Some(WatcherTerminalKind::SoftStopHookSummary),
        RowlessDeliveryAuthority {
            cohort_admits: true,
            ledger_obligation_open: true,
            delivery_lease_present: false,
        },
    );

    assert!(
        authorized,
        "an unsettled ledger obligation must keep a rowless soft terminal a delivery candidate (T5 AC1)"
    );
    assert_eq!(
        denial, None,
        "a frame that is no longer refused must not be blamed for a denial"
    );
}

#[test]
fn rowless_soft_terminal_stays_a_delivery_candidate_on_a_delivery_lease_5464_c1() {
    // The other AC1 operand, alone: the ledger has nothing open, but a delivery
    // lease exists on the channel, so delivery authority is derivable without
    // the row. WHO sends stays the downstream B2 acquire's decision.
    let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
        &tui_direct_binding(),
        None,
        FRAME_END,
        Some(WatcherTerminalKind::SoftStopHookSummary),
        RowlessDeliveryAuthority {
            cohort_admits: true,
            ledger_obligation_open: false,
            delivery_lease_present: true,
        },
    );

    assert!(
        authorized,
        "a delivery lease must keep a rowless soft terminal a delivery candidate (T5 AC1)"
    );
    assert_eq!(denial, None);
}

#[test]
fn rowless_soft_terminal_is_still_denied_without_ledger_or_lease_5464_c1() {
    // The other side of the contract, asserted because AC1 removes the row's
    // veto without handing delivery to a frame nobody owes. Ledger settled, no
    // lease → the historical refusal stands unchanged.
    let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
        &tui_direct_binding(),
        None,
        FRAME_END,
        Some(WatcherTerminalKind::SoftStopHookSummary),
        RowlessDeliveryAuthority {
            cohort_admits: true,
            ledger_obligation_open: false,
            delivery_lease_present: false,
        },
    );

    assert!(!authorized);
    assert_eq!(
        denial,
        Some(SoftTerminalAuthorityDenial::NoInflightRow),
        "with neither AC1 operand the structural refusal must survive"
    );
}

#[test]
fn rowless_evidence_is_inert_outside_the_enforcement_cohort_5464_c1() {
    // The deployment no-op: under the shipped dial `cohort_admits` is false, so
    // even both operands together change nothing and the channel keeps the
    // mapping that ships today.
    let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
        &tui_direct_binding(),
        None,
        FRAME_END,
        Some(WatcherTerminalKind::SoftStopHookSummary),
        RowlessDeliveryAuthority {
            cohort_admits: false,
            ledger_obligation_open: true,
            delivery_lease_present: true,
        },
    );

    assert!(!authorized);
    assert_eq!(denial, Some(SoftTerminalAuthorityDenial::NoInflightRow));
}

#[test]
fn rowless_evidence_never_relaxes_the_five_exact_episode_conjuncts_5464_c1() {
    // C1 moves ONE branch. The other five are exact-episode vetoes — the row
    // that EXISTS names a different session, turn, or nonce — and the #5464 T5
    // audit judged `turn_start_outside_frame` (522) and `turn_nonce_mismatch`
    // (81) NON-violations for exactly that reason. Handing each of them the
    // fullest possible AC1 evidence must change nothing, or the
    // `/compact`-forged soft boundary #5175 closed re-opens.
    let mut foreign_session = row(Some(WATCHER_NONCE), RelayOwnerKind::Watcher);
    foreign_session.tmux_session_name = Some("AgentDesk-someone-else".to_string());

    let mut outside_frame = row(Some(WATCHER_NONCE), RelayOwnerKind::Watcher);
    outside_frame.turn_start_offset = Some(FRAME_END + 1);

    let cases = [
        (
            foreign_session,
            SoftTerminalAuthorityDenial::SessionMismatch,
        ),
        (
            outside_frame,
            SoftTerminalAuthorityDenial::TurnStartOutsideFrame,
        ),
        (
            row(Some(WATCHER_NONCE), RelayOwnerKind::None),
            SoftTerminalAuthorityDenial::RelayOwnerNone,
        ),
        (
            row(None, RelayOwnerKind::Watcher),
            SoftTerminalAuthorityDenial::TurnNonceMissing,
        ),
        (
            row(Some("compact-rewritten-nonce"), RelayOwnerKind::Watcher),
            SoftTerminalAuthorityDenial::TurnNonceMismatch,
        ),
    ];

    for (state, expected) in cases {
        let (authorized, denial) = watcher_soft_terminal_direct_send_authority(
            &tui_direct_binding(),
            Some(&state),
            FRAME_END,
            Some(WatcherTerminalKind::SoftStopHookSummary),
            full_rowless_authority(),
        );

        assert!(
            !authorized,
            "{expected:?} is an exact-episode veto and must survive full AC1 evidence"
        );
        assert_eq!(denial, Some(expected));
    }
}

#[test]
fn rowless_candidacy_requires_the_cohort_and_one_positive_operand_5464_c1() {
    // The predicate's whole truth table, so a mutation that drops an operand or
    // flips the conjunction cannot stay green on the scenarios above alone.
    for cohort_admits in [false, true] {
        for ledger_obligation_open in [false, true] {
            for delivery_lease_present in [false, true] {
                let evidence = RowlessDeliveryAuthority {
                    cohort_admits,
                    ledger_obligation_open,
                    delivery_lease_present,
                };
                assert_eq!(
                    evidence.retains_delivery_candidacy(),
                    cohort_admits && (ledger_obligation_open || delivery_lease_present),
                    "{evidence:?}"
                );
            }
        }
    }
}

#[test]
fn production_call_site_reads_the_ledger_and_the_delivery_lease_5464_c1() {
    // The unit tests above pin the DECISION; this pins the LOOKUPS, which is
    // where the defect actually lives. Deleting either AC1 operand's read — the
    // durable ledger frontier or the delivery-lease cell — leaves every
    // behavioural assertion above green while restoring the body drop in
    // production, so the removal must not be silent.
    let source = include_str!("terminal_relay_plan.rs");
    let reader = include_str!("rowless_delivery_authority.rs")
        .split_once("fn read_rowless_delivery_authority(")
        .expect("the plan must read rowless delivery authority")
        .1
        .split_once("\n}\n")
        .expect("the reader must terminate")
        .0;

    assert!(
        reader.contains("delivered_frontier_end_current_generation"),
        "the ledger obligation must be read from the durable delivered frontier (T5 AC1)"
    );
    assert!(
        reader.contains("ledger_owes_output("),
        "the obligation must come from the pure operand whose polarity is pinned"
    );
    assert!(
        reader.contains("delivery_lease(channel_id)"),
        "the delivery lease must be read from the channel's live lease cell (T5 AC1)"
    );
    assert!(
        reader.contains("lease_has_live_holder("),
        "lease presence must come from the pure operand whose polarity is pinned"
    );
    assert!(
        reader.contains("cohort::enforcement_admits"),
        "the relaxation must be gated by the shared relay-authority cohort predicate"
    );

    let call_site = source
        .split_once("let (watcher_direct_fallback_authorized, soft_terminal_authority_denial) =")
        .expect("the terminal relay plan must decide soft-terminal authority")
        .1
        .split_once(");")
        .expect("the authority call must terminate")
        .0;
    assert!(
        call_site.contains("read_rowless_delivery_authority("),
        "the authority seam must be fed freshly read AC1 evidence, not a literal"
    );
}

#[test]
fn ac1_operand_polarity_is_pinned_by_behaviour_not_the_source_grep_5464_c1() {
    // P1-2: dropping either `!` keeps every `include_str!` assertion above green.
    // P1-3: a `Committed` cell is a FINISHED delivery that is never reclaimed.
    assert!(ledger_owes_output(FRAME_END, FRAME_START));
    assert!(!ledger_owes_output(FRAME_END, FRAME_END));
    assert!(!ledger_owes_output(0, 0));

    let holder = LeaseHolder::Watcher { instance_id: 1 };
    let key = DeliveryLeaseKey::new(serenity::ChannelId::new(42), 1, 7, None, Some(TURN_START));
    assert!(!lease_has_live_holder(&LeaseSnapshot::Unleased));
    assert!(lease_has_live_holder(&LeaseSnapshot::Leased {
        holder,
        key: key.clone(),
        deadline_ms: 1,
        start: FRAME_START,
        end: FRAME_END,
    }));
    let committed = LeaseSnapshot::Committed {
        holder,
        key,
        start: FRAME_START,
        end: FRAME_END,
        outcome: LeaseOutcome::Delivered,
    };
    assert!(!lease_has_live_holder(&committed));
}
