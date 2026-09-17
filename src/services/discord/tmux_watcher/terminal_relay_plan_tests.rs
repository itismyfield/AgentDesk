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

// #5941: the durable record at the #5175 loss seam, and the observability that
// stops "there is no record" from reading as "there is no problem".

const LOST_BODY: &str = "the assistant answer the sink declined and the watcher could not send";
const LOST_CHANNEL: u64 = 1_479_671_298_497_183_835;
const SENT_PREFIX: usize = 4_096;
const GENERATION_MTIME_NS: i64 = 1_758_000_000_000_000_000;

/// The incident shape: a soft terminal, `TurnStartOutsideFrame` (the denial the
/// stale 1h45m-old inflight row produced 33 times), and an unsent tail nobody
/// delivered.
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
        watcher_resend_committed: FRAME_START,
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
    // Authority was never refused, so this is not the loss seam: a row here
    // inflates `D` past the §I17 `D <= N+` audit bound.
    let facts = OrphanTerminalFrameFacts {
        denial: None,
        ..orphan_facts()
    };
    assert!(!facts.record_required());
}

#[test]
fn an_empty_terminal_body_requires_no_record_5941() {
    // 18 of the 33 denials in the incident carried no body: nothing was lost.
    let facts = OrphanTerminalFrameFacts {
        current_response: "",
        ..orphan_facts()
    };
    assert!(!facts.record_required());
}

#[test]
fn a_frame_with_no_consumed_range_at_all_requires_no_record_5941() {
    // A frame that consumed nothing has no JSONL identity to record against.
    let facts = OrphanTerminalFrameFacts {
        terminal_event_consumed_offset: 0,
        ..orphan_facts()
    };
    assert!(!facts.record_required());
}

#[test]
fn a_turn_served_from_the_leftover_buffer_still_requires_a_record_5941() {
    // #1216: a turn carried in the leftover buffer is serviced against the
    // CARRIED buffer's turn start, so its consumed end can sit at or below
    // `data_start_offset` while a real body was lost; the pre-r1 guard
    // (`consumed > data_start_offset`) excluded exactly these turns. r2 P1-B:
    // the committed floor does not re-exclude them — every `confirmed_end_offset`
    // advance targets its OWN turn's consumed end (leftover bytes subtracted out),
    // so a real prior turn's floor lands strictly below this one, `consumed_end
    // - 1` being the tightest such floor.
    for consumed_end in [FRAME_START, FRAME_START - 1] {
        let facts = OrphanTerminalFrameFacts {
            terminal_event_consumed_offset: consumed_end,
            watcher_resend_committed: consumed_end - 1,
            ..orphan_facts()
        };
        assert!(facts.record_required(), "leftover {consumed_end} dropped");
    }
}

#[test]
fn a_body_the_sink_already_put_on_screen_requires_no_record_5941() {
    // The #5941 r1 P1-1 interleaving: the sink's POST landed but its commit
    // proof did not (`SentButUncommitted` -> `TerminalUnknown` -> `RingUnknown`),
    // so `session_bound_ack_confirms_transport` is false and the watcher denies
    // itself in the SAME pass — every watcher-side conjunct then reads "nobody
    // delivered this" about a body the user is already reading. `TimedOut` is NOT
    // the same shape and stays recordable: it is the deadline fall-through in
    // `wait_for_session_bound_relay_delivery_ack`, returned after the ack ring
    // stayed SILENT and never settled afterwards — an absence of evidence, which
    // is the case the record exists for. The table is every non-transport-
    // confirming arm, i.e. all seven that reach this seam.
    for (ack, want) in [
        (SessionBoundRelayAckOutcome::RingUnknown, false),
        (SessionBoundRelayAckOutcome::TimedOut, true),
        (SessionBoundRelayAckOutcome::NotDelivered, true),
        (SessionBoundRelayAckOutcome::Dropped, true),
        (SessionBoundRelayAckOutcome::SinkError, true),
        (SessionBoundRelayAckOutcome::MissingTarget, true),
        (SessionBoundRelayAckOutcome::NotAttempted, true),
    ] {
        let facts = OrphanTerminalFrameFacts {
            session_bound_ack_outcome: ack,
            ..orphan_facts()
        };
        assert_eq!(facts.record_required(), want, "{ack:?} decides wrong");
    }

    // The same question asked of the offset authority: a range at or below the
    // committed floor was delivered, which is why the sibling
    // `SkipAlreadyCommitted` arm suppresses a re-send over it.
    for committed in [FRAME_END, FRAME_END + 1] {
        let delivered = OrphanTerminalFrameFacts {
            watcher_resend_committed: committed,
            ..orphan_facts()
        };
        assert!(!delivered.record_required(), "floor {committed} covers");
    }
    // One byte short of the range end is NOT a delivery: the tail is still lost.
    let partial = OrphanTerminalFrameFacts {
        watcher_resend_committed: FRAME_END - 1,
        ..orphan_facts()
    };
    assert!(partial.record_required());
}

#[test]
fn a_frame_someone_else_delivered_requires_no_record_5941() {
    // Each of these means the body was NOT lost: the sink committed it, the
    // duplicate guard saw it in the channel, or the watcher was authorized.
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
    // The admission must agree with the AUTHORITY rule for every combination. A
    // hard result keeps its recovery fallback even under a soft denial.
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
    // `full_response_len` index the response String bounding `content`, while
    // `jsonl_start` / `jsonl_end` are transcript byte offsets. A recovery that
    // reads one as the other re-publishes the wrong bytes.
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
    let denial = SoftTerminalAuthorityDenial::TurnStartOutsideFrame;
    let kind = WatcherTerminalKind::SoftStopHookSummary;
    for expected in [
        format!("denial={}", denial.as_str()),
        format!("terminal_kind={}", kind.as_str()),
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
    // Run the mapping rather than grep it: `content` must be the UNSENT tail
    // (`full_response` re-publishes the prefix the user already read) and
    // `channel_id` the DELIVERY channel the plan was handed. Emptying either
    // leaves a source grep green.
    let facts = orphan_facts();
    let row = facts.dead_letter_record(
        serenity::ChannelId::new(LOST_CHANNEL),
        "reason-under-test".to_string(),
    );

    assert_eq!(
        row.kind,
        crate::db::relay_dead_letter::KIND_TERMINAL_NO_DELIVERY_OWNER
    );
    assert_eq!(row.content, LOST_BODY, "the row must carry the unsent tail");
    assert!(
        row.content.len() < facts.full_response_len,
        "the tail, not the whole response"
    );
    assert_eq!(row.channel_id, LOST_CHANNEL.to_string());
    assert_eq!(row.author_id.as_deref(), Some("343742347365974026"));
    assert_eq!(row.message_id.as_deref(), Some("5941000"));
    assert_eq!(row.reason, "reason-under-test");

    // A faithful copy, absences included: recovery must not invent a target.
    let bare = OrphanTerminalFrameFacts {
        placeholder_msg_id: None,
        request_owner_user_id: None,
        ..orphan_facts()
    };
    let row = bare.dead_letter_record(serenity::ChannelId::new(LOST_CHANNEL), String::new());
    assert!(row.author_id.is_none() && row.message_id.is_none());
}

#[test]
fn the_production_call_site_hands_the_seam_the_lost_body_5941() {
    // Wiring the predicate tests above cannot reach: swapping
    // `watcher_resend_range_end` for `data_start_offset`, or dropping the body,
    // leaves every behavioural test green while restoring the silent loss.
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
        "watcher_resend_committed,",
        "placeholder_msg_id,",
        "request_owner_user_id: inflight_before_relay",
    ] {
        assert!(
            call_site.contains(operand),
            "the seam must be fed `{operand}` from the frame being lost: {call_site}"
        );
    }

    // The WARN and the counter MOVED with the seam — not duplicated, not
    // dropped. Operators grep `#5175:`.
    let module = include_str!("orphan_terminal_frame.rs");
    assert!(!source.contains("record_relay_terminal_authority_denied("));
    assert!(module.contains("record_relay_terminal_authority_denied("));
    assert!(module.contains("#5175: terminal frame has NO delivery owner"));
    assert!(module.contains("soft_terminal_denial = denial.as_str()"));

    // #5941 r3 P1-2: the PER-CONJUNCT cause counter answers "which authority
    // conjunct refused" and must fire for EVERY denial, including the ones that
    // lose nothing — a forged turn nonce is refused with an empty tail, so
    // behind the record admission the one arm the counter exists to name never
    // emits. Order, not presence: it has to precede the `record_required` gate.
    let cause = module
        .find("record_relay_terminal_denial_cause(")
        .expect("the seam must emit the per-conjunct cause counter");
    let gate = module
        .find("if !facts.record_required() {")
        .expect("the record admission gate must exist");
    let aggregate = module
        .find("metrics::record_relay_terminal_authority_denied(")
        .expect("the seam must emit the aggregate denial counter");
    assert!(
        cause < gate,
        "the per-conjunct cause counter must be emitted BEFORE the record admission gate"
    );
    assert!(
        gate < aggregate,
        "the aggregate counter feeds a threshold-1 alert row and must stay behind the admission"
    );
}

#[test]
fn the_dead_letter_kind_survives_a_non_unix_build_5941() {
    // The `kind` string is platform-independent but its only writer is the
    // `#[cfg(unix)]` watcher, so a Windows build sees it unused under
    // `-D warnings`. Asserted in PAIR with the existing sibling.
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
    // The behavioural half: this seam's discriminator is its own, so an operator
    // filtering the DLQ by `kind` never mixes #5941 in with a sibling vector.
    assert_eq!(
        crate::db::relay_dead_letter::KIND_TERMINAL_NO_DELIVERY_OWNER,
        "terminal_no_delivery_owner"
    );
    for sibling in [
        crate::db::relay_dead_letter::KIND_CATCH_UP_TOO_OLD,
        crate::db::relay_dead_letter::KIND_QUEUE_OVERFLOW,
        crate::db::relay_dead_letter::KIND_READOPT_RELAY_STUCK,
    ] {
        assert_ne!(
            sibling,
            crate::db::relay_dead_letter::KIND_TERMINAL_NO_DELIVERY_OWNER
        );
    }
}

#[test]
fn a_dropped_body_with_no_durable_record_violates_i17_5941() {
    // I17 (`docs/relay-state-contract.md`): a terminal frame carrying a body
    // ends with a delivery owner or a record. Force the violating state — a
    // record-required frame with NO pool to record into — and prove the check
    // FIRES. The pre-fix code read the absence of a record as the absence of a
    // problem and reported `healthy` while three answers were gone.
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

    // #5941 r1: pin the guarantee at the level the code provides. A restart
    // re-observing the same frame observes it AGAIN — no dedup key — so `D` may
    // carry duplicates, exactly as the contract doc says.
    assert!(!observe(&orphan_facts()) && !observe(&orphan_facts()));

    assert_eq!(
        TERMINAL_FRAME_OWNER_OR_RECORD_INVARIANT, "terminal_frame_has_a_delivery_owner_or_a_record",
        "the invariant key is the alert table's status filter and the contract doc's key"
    );
}

/// A channel of its own: the counter below is process-global and cumulative, so
/// sharing `LOST_CHANNEL` with the sibling I17 test would race under `--test-threads`.
const PLAN_DRIVEN_CHANNEL: u64 = 1_479_671_298_497_183_836;

/// Losses ADMITTED for `channel` — the aggregate counter the seam raises only
/// after `record_required` passes, i.e. exactly when a DLQ row is owed.
fn admitted_losses(channel: u64) -> u64 {
    crate::services::observability::metrics::snapshot()
        .into_iter()
        .filter(|row| row.channel_id == channel)
        .map(|row| row.relay_terminal_authority_denied)
        .sum()
}

#[tokio::test]
async fn the_plan_itself_admits_the_lost_body_to_the_record_5941() {
    // The only #5941 test that enters through PRODUCTION. The others hand-build
    // `OrphanTerminalFrameFacts`, and the wiring test greps the call site's
    // ARGUMENT LIST — so `let current_response = "";` planted on the line ABOVE
    // the call falsifies `record_required` forever, writes zero dead-letter rows
    // and leaves every one of them green. The gap was the ENTRY POINT, not the
    // strength of any assertion, so this one drives `run_terminal_relay_plan`
    // itself over the incident shape: a soft stop-hook terminal carrying an
    // assistant body, no inflight row (soft-terminal authority is denied) and no
    // cached relay producer (the sink never owns the frame).
    let root = tempfile::tempdir().expect("isolated runtime root");
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let shared = crate::services::discord::make_shared_data_for_tests();
    let channel = serenity::ChannelId::new(PLAN_DRIVEN_CHANNEL);
    let before = admitted_losses(PLAN_DRIVEN_CHANNEL);

    let http = std::sync::Arc::new(serenity::Http::new("fixture-no-network"));
    let provider = ProviderKind::Claude;
    let session = SESSION.to_string();
    let output_path = root.path().join("out.jsonl").display().to_string();
    let all_data = String::new();
    let full_response = format!("{}{LOST_BODY}", "x".repeat(SENT_PREFIX));
    let tool_state = WatcherToolState::new();
    let context = TerminalRelayPlanContext {
        http: &http,
        shared: &shared,
        channel_id: channel,
        watcher_provider: &provider,
        tmux_session_name: &session,
        output_path: &output_path,
        inflight_before_relay: &None,
        cached_relay_producer: &None,
        prompt_anchor_present_before_relay: false,
        external_input_lease_before_relay: false,
        session_bound_relay_turn_fully_mirrored: false,
        session_bound_relay_turn_first_forwarded_sequence: None,
        split_trailing_turn_follows: false,
        startup_soft_terminal_authority: tui_direct_binding(),
    };
    let locals = TerminalRelayPlanLocals {
        current_offset: FRAME_END,
        data_start_offset: FRAME_START,
        all_data: &all_data,
        full_response: &full_response,
        current_response: LOST_BODY,
        response_sent_offset: SENT_PREFIX,
        has_assistant_response: true,
        terminal_kind: Some(WatcherTerminalKind::SoftStopHookSummary),
        task_notification_kind: None,
        assistant_text_seen: true,
        fresh_assistant_text_seen: true,
        tool_state: &tool_state,
        placeholder_msg_id: None,
        status_panel_msg_id: None,
    };
    let mut session_bound_relay_ack = None;
    let mut monitor_auto_turn_claimed = false;
    let mut monitor_auto_turn_finished = false;
    let mut monitor_auto_turn_synthetic_msg_id = None;
    let mut monitor_auto_turn_ledger_generation = None;
    let mut state = TerminalRelayPlanState {
        all_data_session_bound_relay_ack: &mut session_bound_relay_ack,
        monitor_auto_turn_claimed: &mut monitor_auto_turn_claimed,
        monitor_auto_turn_finished: &mut monitor_auto_turn_finished,
        monitor_auto_turn_synthetic_msg_id: &mut monitor_auto_turn_synthetic_msg_id,
        monitor_auto_turn_ledger_generation: &mut monitor_auto_turn_ledger_generation,
    };

    let outcome = run_terminal_relay_plan(&context, locals, &mut state).await;
    assert!(
        matches!(outcome, TerminalRelayPlanOutcome::Proceed(_)),
        "the plan must reach its terminal decision rather than bailing before the seam"
    );
    assert_eq!(
        admitted_losses(PLAN_DRIVEN_CHANNEL) - before,
        1,
        "a frame the plan itself left with no delivery owner must be admitted to the record exactly once"
    );
}
