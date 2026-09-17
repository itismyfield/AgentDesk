//! #5175 soft-terminal delivery-authority tests for the terminal relay plan.
//!
//! Split out of `terminal_relay_plan.rs` to keep that module inside the
//! `src/services/discord/tmux_watcher/**` namespace size cap.

use super::orphan_terminal_frame::{
    OrphanTerminalFrameFacts, TERMINAL_FRAME_OWNER_OR_RECORD_INVARIANT,
    observe_orphan_terminal_frame,
};
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
    assert!(ledger_owes_output(FRAME_END, Some(FRAME_START)));
    assert!(!ledger_owes_output(FRAME_END, Some(FRAME_END)));
    assert!(!ledger_owes_output(0, Some(0)));
    assert!(!ledger_owes_output(FRAME_END, None));

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

const READER_DELIVERED_END: u64 = 4_096;

/// Fixture at REAL session paths: transcript (#4188 EOF), marker (#1270), frontier.
/// The caller MUST already hold `set_agentdesk_root_for_test`: every path built
/// here resolves through the runtime root, and so does every later read of it.
fn reader_fixture(channel: u64, session: &str) -> (serenity::ChannelId, String, String) {
    let transcript = crate::services::tmux_common::session_temp_path(session, "jsonl");
    std::fs::write(&transcript, vec![b'.'; READER_DELIVERED_END as usize]).expect("transcript");
    let marker = crate::services::tmux_common::session_temp_path(session, "generation");
    std::fs::write(&marker, "5464-c1").expect("generation marker");
    let generation_mtime_ns = dr::current_generation_mtime_ns(session);
    let channel = serenity::ChannelId::new(channel);
    dr::write_delivered_frontier(
        &ProviderKind::Claude,
        channel.get(),
        session,
        dr::DeliveredCommit {
            range: (0, READER_DELIVERED_END),
            generation_mtime_ns,
            attempts: 1,
            panel_msg_id: None,
            panel_channel_id: None,
        },
    )
    .expect("durable frontier");
    (channel, transcript, marker)
}

/// #5464 T5 C1: `read_rowless_delivery_authority` EXECUTED, not grepped — the
/// `include_str!` test cannot see argument order, the polarity test never enters it.
#[test]
fn reader_pins_the_ledger_and_lease_operands_5464_c1() {
    // Root isolation, held for the WHOLE body rather than just the fixture:
    // every `read(..)` below re-resolves BOTH roots -- the transcript/marker via
    // `config::runtime_root()` and the durable frontier via
    // `runtime_store::runtime_root()` -- so a guard dropped after setup would
    // leave the later reads racing the other `AGENTDESK_ROOT_DIR` sites in this
    // binary (`cargo test --lib` runs at default parallelism), and a root
    // swapped mid-test turns a frontier/marker/EOF read into `None`, failing the
    // assertions below for an environmental reason. `set_agentdesk_root_for_test`
    // holds the process-global test env lock for the guard's lifetime, and the
    // tempdir keeps the session files out of the live
    // `~/.adk/release/runtime/sessions/` tree. Same shape as `IsolatedRoot` in
    // `delivery_record.rs` and the frontier test in `session_relay_sink/tests.rs`.
    let root = tempfile::tempdir().expect("isolated runtime root");
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let session = "AgentDesk-claude-5464-c1-reader";
    let (channel, path, marker) = reader_fixture(5_464_001, session);
    let read = |consumed_end| {
        read_rowless_delivery_authority(
            &shared,
            &ProviderKind::Claude,
            channel,
            session,
            &path,
            consumed_end,
        )
    };

    assert!(
        read(READER_DELIVERED_END + 1).ledger_obligation_open,
        "owes past frontier"
    );
    assert!(!read(READER_DELIVERED_END).ledger_obligation_open);

    let holder = LeaseHolder::Watcher { instance_id: 1 };
    let key = DeliveryLeaseKey::new(channel, 1, 7, None, Some(TURN_START));
    let lease = shared.delivery_lease(channel);
    assert!(!read(READER_DELIVERED_END).delivery_lease_present);
    assert!(lease.try_acquire(key.clone(), holder, FRAME_START, FRAME_END, u64::MAX));
    assert!(read(READER_DELIVERED_END).delivery_lease_present);
    assert!(lease.release(holder, key, FRAME_START, FRAME_END));
    assert!(!read(READER_DELIVERED_END).delivery_lease_present);

    // `/compact` shrinks the transcript below the frontier END: UNKNOWN (#4188).
    std::fs::write(&path, b"compacted").expect("shrunk transcript");
    assert!(
        !read(READER_DELIVERED_END + 1).ledger_obligation_open,
        "an unknown frontier must not open a delivery obligation (#5175)"
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&marker);
}

// ---------------------------------------------------------------------------
// #5941: the durable record at the #5175 loss seam, and the observability that
// stops "there is no record" from reading as "there is no problem".
// ---------------------------------------------------------------------------

const LOST_BODY: &str = "the assistant answer the sink declined and the watcher could not send";
const LOST_CHANNEL: u64 = 1_479_671_298_497_183_835;
const SENT_PREFIX: usize = 4_096;
const GENERATION_MTIME_NS: i64 = 1_758_000_000_000_000_000;

/// The incident shape: a soft terminal, `TurnStartOutsideFrame` (the denial the
/// stale 1h45m-old inflight row produced 33 times), an ownerless inflight row,
/// and a non-empty unsent tail that nobody delivered.
fn orphan_facts() -> OrphanTerminalFrameFacts<'static> {
    OrphanTerminalFrameFacts {
        denial: Some(SoftTerminalAuthorityDenial::TurnStartOutsideFrame),
        watcher_direct_fallback_requested: true,
        watcher_direct_fallback_authorized: false,
        session_bound_relay_owns_terminal_delivery: false,
        direct_terminal_response_refused_duplicate: false,
        current_response: LOST_BODY,
        response_sent_offset: SENT_PREFIX,
        full_response_len: SENT_PREFIX + LOST_BODY.len(),
        data_start_offset: FRAME_START,
        current_offset: FRAME_END,
        terminal_event_consumed_offset: FRAME_END,
        terminal_kind: Some(WatcherTerminalKind::SoftStopHookSummary),
        session_bound_ack_outcome: SessionBoundRelayAckOutcome::NotDelivered,
        inflight_present: true,
        inflight_relay_owner: "none",
        startup_snapshot_authority: false,
        tmux_session_name: SESSION,
        placeholder_msg_id: Some(serenity::MessageId::new(5_941_000)),
        request_owner_user_id: Some(343_742_347_365_974_026),
    }
}

#[test]
fn denied_terminal_frame_with_a_body_requires_a_durable_record_5941() {
    assert!(
        orphan_facts().record_required(),
        "the incident shape — denial, unauthorized fallback, no other owner, non-empty body — is exactly what must be preserved"
    );
}

#[test]
fn a_frame_with_no_denial_requires_no_record_5941() {
    // `denial: None` means authority was never refused, so this frame is not
    // the loss seam at all. Recording it would inflate the audit count `D` and
    // make the `D == N+` reconciliation with the #5175 WARNs unreadable.
    let facts = OrphanTerminalFrameFacts {
        denial: None,
        ..orphan_facts()
    };
    assert!(!facts.record_required());
}

#[test]
fn an_empty_terminal_body_requires_no_record_5941() {
    // 18 of the 33 denials in the 2026-09-16 incident carried no body: nothing
    // was lost, so a row for them would be noise that breaks the arithmetic.
    let facts = OrphanTerminalFrameFacts {
        current_response: "",
        ..orphan_facts()
    };
    assert!(!facts.record_required());
}

#[test]
fn a_frame_whose_consumed_range_did_not_advance_requires_no_record_5941() {
    for consumed_end in [FRAME_START, FRAME_START - 1, 0] {
        let facts = OrphanTerminalFrameFacts {
            terminal_event_consumed_offset: consumed_end,
            ..orphan_facts()
        };
        assert!(
            !facts.record_required(),
            "consumed_end {consumed_end} does not advance past data_start_offset {FRAME_START}"
        );
    }
}

#[test]
fn a_frame_someone_else_delivered_requires_no_record_5941() {
    // Three different owners, each of which means the body was NOT lost: the
    // session-bound sink committed it, the duplicate guard refused it because it
    // is already in the channel, or the watcher itself was authorized to send.
    for facts in [
        OrphanTerminalFrameFacts {
            session_bound_relay_owns_terminal_delivery: true,
            ..orphan_facts()
        },
        OrphanTerminalFrameFacts {
            direct_terminal_response_refused_duplicate: true,
            ..orphan_facts()
        },
        OrphanTerminalFrameFacts {
            watcher_direct_fallback_authorized: true,
            ..orphan_facts()
        },
        OrphanTerminalFrameFacts {
            watcher_direct_fallback_requested: false,
            ..orphan_facts()
        },
    ] {
        assert!(
            !facts.record_required(),
            "a frame with a delivery owner must not be dead-lettered"
        );
    }
}

#[test]
fn record_decision_is_pinned_across_every_kind_and_denial_pair_5941() {
    // The record admission must agree with the AUTHORITY rule for every
    // combination, not just the incident's. A hard provider result keeps its
    // recovery fallback even when the soft contract denied it, so a denial
    // alone must not conjure a dead letter there.
    for terminal_kind in [
        None,
        Some(WatcherTerminalKind::HardResult),
        Some(WatcherTerminalKind::SoftStopHookSummary),
        Some(WatcherTerminalKind::SoftUserBoundary),
    ] {
        for denial in [
            None,
            Some(SoftTerminalAuthorityDenial::NoInflightRow),
            Some(SoftTerminalAuthorityDenial::SessionMismatch),
            Some(SoftTerminalAuthorityDenial::TurnStartOutsideFrame),
            Some(SoftTerminalAuthorityDenial::RelayOwnerNone),
            Some(SoftTerminalAuthorityDenial::TurnNonceMissing),
            Some(SoftTerminalAuthorityDenial::TurnNonceMismatch),
        ] {
            let authorized =
                watcher_direct_fallback_has_turn_authority(terminal_kind, denial.is_none());
            let facts = OrphanTerminalFrameFacts {
                denial,
                terminal_kind,
                watcher_direct_fallback_authorized: authorized,
                ..orphan_facts()
            };
            assert_eq!(
                facts.record_required(),
                denial.is_some() && !authorized,
                "kind {terminal_kind:?} denial {denial:?} authorized {authorized}"
            );
        }
    }
}

#[test]
fn the_record_reason_carries_both_coordinate_systems_5941() {
    // The two systems are NOT interchangeable: `response_sent_offset` /
    // `full_response_len` index the in-memory response String that bounds
    // `content`, while `jsonl_start` / `jsonl_end` are transcript byte offsets
    // the frontier failed to advance past. A recovery that reads one as the
    // other re-publishes the wrong bytes, so both must survive in the row.
    let facts = orphan_facts();
    let reason = facts.reason(
        SoftTerminalAuthorityDenial::TurnStartOutsideFrame,
        &ProviderKind::Claude,
        GENERATION_MTIME_NS,
    );

    assert!(
        reason.starts_with("terminal_no_delivery_owner "),
        "{reason}"
    );
    for expected in [
        format!(
            "denial={}",
            SoftTerminalAuthorityDenial::TurnStartOutsideFrame.as_str()
        ),
        format!(
            "terminal_kind={}",
            WatcherTerminalKind::SoftStopHookSummary.as_str()
        ),
        format!("response_sent_offset={SENT_PREFIX}"),
        format!("full_response_len={}", SENT_PREFIX + LOST_BODY.len()),
        format!("jsonl_start={FRAME_START}"),
        format!("jsonl_end={FRAME_END}"),
        format!("current_offset={FRAME_END}"),
        format!("generation_mtime_ns={GENERATION_MTIME_NS}"),
        format!("tmux_session={SESSION}"),
        format!("provider={}", ProviderKind::Claude.as_str()),
        "inflight_relay_owner=none".to_string(),
        "frame_ack_outcome=NotDelivered".to_string(),
    ] {
        assert!(
            reason.contains(&expected),
            "reason must carry `{expected}`: {reason}"
        );
    }
    assert!(
        !reason.contains(&format!("jsonl_start={SENT_PREFIX}")),
        "the response coordinate must never be reported as a transcript offset: {reason}"
    );
}

#[test]
fn the_dead_letter_row_preserves_the_unsent_tail_and_the_delivery_channel_5941() {
    // Behaviour cannot reach the INSERT without a pool, so pin the mapping at
    // the source: `content` must be the UNSENT tail (`current_response`, not the
    // whole `full_response`, which would re-deliver the prefix the user already
    // read), and `channel_id` must be the DELIVERY channel the plan was handed,
    // not the watcher's owner channel.
    let module = include_str!("orphan_terminal_frame.rs");
    let record = module
        .split_once("RelayDeadLetterRecord {")
        .expect("the seam must build a dead-letter record")
        .1
        .split_once("\n        },")
        .expect("the record literal must terminate")
        .0;

    assert!(record.contains("kind: crate::db::relay_dead_letter::KIND_TERMINAL_NO_DELIVERY_OWNER"));
    assert!(
        record.contains("content: facts.current_response.to_string()"),
        "the row must carry the unsent tail: {record}"
    );
    assert!(!record.contains("full_response"), "{record}");
    assert!(
        record.contains("channel_id: channel_id.to_string()"),
        "{record}"
    );
    assert!(!record.contains("watcher_owner_channel_id"), "{record}");
    assert!(
        record.contains("author_id: facts.request_owner_user_id"),
        "{record}"
    );
    assert!(
        record.contains("message_id: facts.placeholder_msg_id"),
        "{record}"
    );
    assert!(record.contains("reason,"), "{record}");
    assert!(
        module.contains("record_detached(\n        shared.pg_pool.as_ref(),"),
        "the record must be written through the shared PG pool, fire-and-forget"
    );
}

#[test]
fn the_production_call_site_hands_the_seam_the_lost_body_5941() {
    // The predicate tests above cannot see whether the plan actually feeds this
    // seam the frame it lost: swapping `watcher_resend_range_end` for
    // `data_start_offset`, or dropping the body, leaves them all green while
    // restoring the silent loss.
    let source = include_str!("terminal_relay_plan.rs");
    let call_site = source
        .split_once("orphan_terminal_frame::observe_orphan_terminal_frame(")
        .expect("the plan must route the denial seam through the orphan-frame recorder")
        .1
        .split_once("\n        );")
        .expect("the call must terminate")
        .0;

    for operand in [
        "shared,",
        "channel_id,",
        "watcher_provider,",
        "denial: soft_terminal_authority_denial,",
        "current_response,",
        "response_sent_offset,",
        "full_response_len: full_response.len(),",
        "data_start_offset,",
        "terminal_event_consumed_offset: watcher_resend_range_end,",
        "placeholder_msg_id,",
        "request_owner_user_id: inflight_before_relay",
    ] {
        assert!(
            call_site.contains(operand),
            "the seam must be fed `{operand}` from the frame being lost: {call_site}"
        );
    }

    // The WARN and the per-conjunct counter MOVED with the seam; they did not
    // get duplicated, and they did not get dropped. Operators grep `#5175:`.
    let module = include_str!("orphan_terminal_frame.rs");
    assert!(!source.contains("record_relay_terminal_authority_denied("));
    assert!(module.contains("record_relay_terminal_authority_denied("));
    assert!(module.contains("#5175: terminal frame has NO delivery owner"));
    assert!(module.contains("soft_terminal_denial = denial.as_str()"));
}

#[test]
fn the_dead_letter_kind_survives_a_non_unix_build_5941() {
    // The `kind` string is platform-independent but its only writer is the
    // `#[cfg(unix)]` watcher, so a Windows build sees it unused and `-D warnings`
    // turns that into CI red. Asserted in PAIR with the existing sibling so the
    // test cannot pass by accident if the attribute convention changes.
    let dlq = include_str!("../../../db/relay_dead_letter.rs");
    for kind in [
        "KIND_READOPT_RELAY_STUCK",
        "KIND_TERMINAL_NO_DELIVERY_OWNER",
    ] {
        let before = dlq
            .split_once(&format!("pub(crate) const {kind}:"))
            .unwrap_or_else(|| panic!("{kind} must be declared"))
            .0;
        assert!(
            before
                .trim_end()
                .ends_with("#[cfg_attr(not(unix), allow(dead_code))]"),
            "{kind} must be declared unconditionally with a non-unix dead-code allowance"
        );
    }
}

#[test]
fn a_dropped_body_with_no_durable_record_violates_i17_5941() {
    // I17 (`docs/relay-state-contract.md`): a terminal frame carrying a body
    // ends with a delivery owner or a record. Force the violating state — a
    // record-required frame with NO pool to record it into — and prove the
    // invariant check FIRES. This is the #5941 defect itself: the pre-fix code
    // read the absence of a record as the absence of a problem and reported
    // `healthy` while three answers were gone.
    let root = tempfile::tempdir().expect("isolated runtime root");
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    assert!(
        shared.pg_pool.is_none(),
        "the violating state requires an unconfigured dead-letter sink"
    );
    let channel = serenity::ChannelId::new(LOST_CHANNEL);
    let observe = |facts: &OrphanTerminalFrameFacts<'_>| {
        observe_orphan_terminal_frame(&shared, channel, &ProviderKind::Claude, facts)
    };

    assert!(
        !observe(&orphan_facts()),
        "a body dropped with no delivery owner AND no durable record must violate I17"
    );

    // Same seam, but the body is not lost: no record is owed, so nothing pages.
    for benign in [
        OrphanTerminalFrameFacts {
            session_bound_relay_owns_terminal_delivery: true,
            ..orphan_facts()
        },
        OrphanTerminalFrameFacts {
            current_response: "",
            ..orphan_facts()
        },
        OrphanTerminalFrameFacts {
            denial: None,
            ..orphan_facts()
        },
    ] {
        assert!(
            observe(&benign),
            "a frame with an owner, no body, or no denial must not page as an unrecorded loss"
        );
    }

    assert_eq!(
        TERMINAL_FRAME_OWNER_OR_RECORD_INVARIANT, "terminal_frame_has_a_delivery_owner_or_a_record",
        "the invariant key is the alert table's status filter and the contract doc's key"
    );
}
