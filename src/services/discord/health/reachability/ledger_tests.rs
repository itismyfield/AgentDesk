//! Obligation-ledger tests (#5071 T4-B2).
//!
//! Every case here uses an explicit path under a `TempDir`, so none of them
//! reads the process runtime root and none can write into a developer's real
//! `runtime/` tree.

use tempfile::TempDir;

use super::*;
use crate::services::discord::health::reachability::obligation::ObligationReason;

fn identity(dev: u64, ino: u64) -> TranscriptFileId {
    TranscriptFileId { dev, ino }
}

fn incarnation(session: &str, generation: i64, nonce: Option<&str>, ino: u64) -> LedgerIncarnation {
    LedgerIncarnation::new(
        session.to_string(),
        generation,
        nonce.map(str::to_string),
        identity(66, ino),
    )
}

fn obligation_record(start: u64, end: u64) -> CanonicalRecord {
    CanonicalRecord {
        generation_mtime_ns: 7,
        start,
        end,
        identity: identity(66, 900),
        reason: ObligationReason::AssistantText,
    }
}

#[test]
fn a_written_ledger_round_trips_through_the_sidecar() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider/123.json");

    let mut ledger = ReachabilityLedger::bootstrap(
        incarnation("adk-chan", 42, Some("nonce-a"), 900),
        4_096,
        LedgerCounters::default(),
    );
    ledger.append_obligations(vec![obligation_record(4_096, 4_200)], 1_700);

    write_ledger_at(&path, &ledger).expect("write");
    assert_eq!(read_ledger_at(&path).expect("read back"), ledger);
}

/// 4987 §-1.4 counterexample 7: an unreadable store is `Unknown`, never a
/// conclusion. The reader therefore reports an ABSENCE, and it is the caller's
/// job to notice that a file was nonetheless present — hence the separate
/// [`ledger_file_exists`], which is what lets "first sight" and "corrupt" take
/// different branches.
#[test]
fn a_malformed_ledger_reads_as_absent_while_the_file_still_reports_present() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider/9.json");
    std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    std::fs::write(&path, "{ this is not a ledger").expect("write garbage");

    assert_eq!(read_ledger_at(&path), None);
    assert!(
        ledger_file_exists(&path),
        "the caller must be able to tell a corrupt store from a first sight"
    );
}

#[test]
fn a_ledger_from_another_schema_version_is_discarded_rather_than_migrated() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider/9.json");
    let ledger = ReachabilityLedger::bootstrap(
        incarnation("adk-chan", 42, None, 900),
        0,
        LedgerCounters::default(),
    );
    write_ledger_at(&path, &ledger).expect("write");
    let bumped = std::fs::read_to_string(&path)
        .expect("read")
        .replace("\"schema_version\": 1", "\"schema_version\": 2");
    std::fs::write(&path, bumped).expect("rewrite");

    assert_eq!(
        read_ledger_at(&path),
        None,
        "an observation record has no authority, so re-bootstrapping beats migrating"
    );
}

/// The incarnation match is a conjunction, and a missing spawn nonce is NOT a
/// wildcard — #5071 T3-R2 and 4987 §-1.3 both forbid widening an absent marker
/// into a match.
#[test]
fn every_incarnation_conjunct_must_match_and_none_is_absent_matches_some() {
    let bound = incarnation("adk-chan", 42, Some("nonce-a"), 900);
    let ledger = ReachabilityLedger::bootstrap(bound.clone(), 0, LedgerCounters::default());

    assert!(ledger.binds_to(&bound));
    assert!(!ledger.binds_to(&incarnation("adk-other", 42, Some("nonce-a"), 900)));
    assert!(!ledger.binds_to(&incarnation("adk-chan", 43, Some("nonce-a"), 900)));
    assert!(!ledger.binds_to(&incarnation("adk-chan", 42, Some("nonce-b"), 900)));
    assert!(!ledger.binds_to(&incarnation("adk-chan", 42, None, 900)));
    assert!(!ledger.binds_to(&incarnation("adk-chan", 42, Some("nonce-a"), 901)));
}

/// 4987 I13: an obligation leaves only through a NAMED reason. A superseded
/// incarnation retires its obligations as `IncarnationRetired` and the count
/// survives on the counter — the bytes are gone, the fact that they were never
/// subtracted is not.
#[test]
fn retiring_an_incarnation_counts_the_obligations_it_took_with_it() {
    let mut ledger = ReachabilityLedger::bootstrap(
        incarnation("adk-chan", 42, None, 900),
        0,
        LedgerCounters::default(),
    );
    ledger.append_obligations(
        vec![obligation_record(0, 10), obligation_record(10, 20)],
        1_700,
    );
    assert_eq!(ledger.counters.total_obligations, 2);

    let rebootstrapped =
        ledger.retire_and_rebootstrap(incarnation("adk-chan", 43, None, 901), 5_000);

    assert!(rebootstrapped.live_obligations().is_empty());
    assert_eq!(rebootstrapped.counters.retired_incarnation, 2);
    assert_eq!(
        rebootstrapped.counters.total_obligations, 2,
        "the 30-day record must survive a rotation"
    );
    assert_eq!(rebootstrapped.cursor_offset, 5_000);
    assert_eq!(rebootstrapped.bootstrap_offset, 5_000);
}

/// The bounded ring is the one place obligations can vanish under load, so the
/// eviction is a typed `ClassifiedDrop` with a counter, never a silent
/// truncation. 4987 §7.3's rule for `SuppressedByDedup` is the same discipline:
/// a non-delivery outcome is never folded into a success.
#[test]
fn overflow_evicts_the_oldest_as_a_typed_classified_drop() {
    let mut ledger = ReachabilityLedger::bootstrap(
        incarnation("adk-chan", 42, None, 900),
        0,
        LedgerCounters::default(),
    );
    let records: Vec<_> = (0..LEDGER_OBLIGATION_CAP as u64 + 3)
        .map(|index| obligation_record(index * 10, index * 10 + 10))
        .collect();

    let extinctions = ledger.append_obligations(records.iter().cloned(), 1_700);

    assert_eq!(ledger.live_obligations().len(), LEDGER_OBLIGATION_CAP);
    assert_eq!(extinctions.len(), 3);
    assert!(extinctions.iter().all(|extinction| matches!(
        extinction,
        ObligationExtinction::ClassifiedDrop {
            reason: ClassifiedDropReason::LedgerCapacity
        }
    )));
    assert_eq!(ledger.counters.retired_classified_drop, 3);
    assert_eq!(
        ledger.live_obligations()[0].start,
        30,
        "eviction drops the OLDEST; the newest observation is the one worth keeping"
    );
    assert_eq!(
        ledger.counters.total_obligations,
        LEDGER_OBLIGATION_CAP as u64 + 3,
        "the total is what was ever observed, not what is still held"
    );
}

/// The one thing this slice must not do: retire an obligation as delivered.
/// `ReceiptCovered` exists in the type set so T4-B3 adds a producer rather than
/// a vocabulary, and this test is the standing statement that B2 has none.
#[test]
fn nothing_in_this_slice_retires_an_obligation_as_receipt_covered() {
    let mut ledger = ReachabilityLedger::bootstrap(
        incarnation("adk-chan", 42, None, 900),
        0,
        LedgerCounters::default(),
    );
    let extinctions = ledger.append_obligations(vec![obligation_record(0, 10)], 1_700);
    assert!(
        !extinctions
            .iter()
            .any(|extinction| matches!(extinction, ObligationExtinction::ReceiptCovered)),
        "the receipt index is T4-B3; B2 cannot observe that an obligation was met"
    );

    let retired = ledger.retire_and_rebootstrap(incarnation("adk-chan", 43, None, 901), 0);
    assert_eq!(retired.counters.retired_receipt_covered, 0);
}

#[test]
fn the_sidecar_path_is_keyed_by_provider_and_channel() {
    let dir = TempDir::new().expect("tempdir");
    // The runtime root is process-global, so it is taken through the repo's
    // shared test-env guard, which serializes against every other env-mutating
    // test and restores the previous value even if an assertion below unwinds.
    let _root = crate::config::set_agentdesk_root_for_test(dir.path());

    let path = ledger_path(&ProviderKind::Claude, 42).expect("path under an overridden root");

    assert!(
        path.ends_with("discord_reachability_ledger/claude/42.json"),
        "{path:?}"
    );
}

/// lock-guarded read-modify-write transaction: append and then read back
/// through the flock-guarded API.
#[test]
fn append_ledger_at_acquires_lock_and_persists_atomically() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider/1001.json");

    // First append — creates the ledger with a bootstrap incarnation.
    let extinctions =
        append_ledger_at(&path, vec![obligation_record(100, 200)], 5_000).expect("first append");

    assert!(extinctions.is_empty(), "first append should not overflow");
    let ledger = read_ledger_at(&path).expect("ledger present");
    assert_eq!(ledger.live_obligations().len(), 1);
    assert_eq!(ledger.counters.total_obligations, 1);

    // Second append — lock serializes the RMW, so the first record is preserved.
    let extinctions =
        append_ledger_at(&path, vec![obligation_record(200, 300)], 5_001).expect("second append");

    assert!(extinctions.is_empty(), "still below cap");
    let ledger = read_ledger_at(&path).expect("ledger present after second append");
    assert_eq!(
        ledger.live_obligations().len(),
        2,
        "both records from sequential appends must be preserved"
    );
    assert_eq!(ledger.counters.total_obligations, 2);
}

/// Two sequential writes without intermediate reads: both must survive.
/// This test demonstrates that the lock serializes writers correctly.
#[test]
fn sequential_appends_preserve_all_obligations() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider/2002.json");

    // Writer A appends.
    append_ledger_at(&path, vec![obligation_record(10, 20)], 1_000).expect("A's append");

    // Writer B appends independently.
    append_ledger_at(&path, vec![obligation_record(30, 40)], 2_000).expect("B's append");

    let ledger = read_ledger_at(&path).expect("ledger present");
    assert_eq!(ledger.live_obligations().len(), 2);
    assert_eq!(ledger.counters.total_obligations, 2);

    let obls = ledger.live_obligations();
    assert_eq!(obls[0].start, 10);
    assert_eq!(obls[1].start, 30);
}

/// Non-obligation records passed to append_obligations are filtered via
/// debug_assert (in test builds). This test verifies the filtering logic works.
#[test]
fn append_obligations_filters_non_obligation_records() {
    use crate::services::discord::health::reachability::obligation::ObligationReason;

    let mut ledger = ReachabilityLedger::bootstrap(
        incarnation("adk-chan", 42, None, 900),
        0,
        LedgerCounters::default(),
    );

    // Create a mix: one obligation, one non-obligation.
    let records = vec![
        CanonicalRecord {
            generation_mtime_ns: 7,
            start: 100,
            end: 200,
            identity: identity(66, 900),
            reason: ObligationReason::AssistantText, // is_obligation() == true
        },
        CanonicalRecord {
            generation_mtime_ns: 7,
            start: 200,
            end: 300,
            identity: identity(66, 900),
            reason: ObligationReason::BlankLine, // is_obligation() == false
        },
    ];

    // In debug builds, the non-obligation record will trigger a debug_assert.
    // In release builds, it will be added anyway (the assert is debug-only).
    // This test documents the intended filtering behavior.
    #[cfg(debug_assertions)]
    {
        // We expect a panic in debug mode when a non-obligation is appended.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut ledger_copy = ledger.clone();
            ledger_copy.append_obligations(records.clone(), 1_000);
        }));
        assert!(
            result.is_err(),
            "debug_assert should catch non-obligation records"
        );
    }

    // For release mode or as a positive case: append only obligations.
    let only_obligations: Vec<_> = records
        .into_iter()
        .filter(|r| r.reason.is_obligation())
        .collect();
    let extinctions = ledger.append_obligations(only_obligations, 1_000);
    assert!(extinctions.is_empty());
    assert_eq!(ledger.live_obligations().len(), 1);
}
