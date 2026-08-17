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

fn bootstrap_test_ledger(path: &Path) {
    bootstrap_ledger_at(path, incarnation("adk-chan", 42, None, 900), 0).expect("bootstrap ledger");
}

#[test]
fn a_written_ledger_round_trips_through_the_sidecar() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider/123.json");
    let expected_incarnation = incarnation("adk-chan", 42, Some("nonce-a"), 900);
    let expected_record = obligation_record(4_096, 4_200);

    bootstrap_ledger_at(&path, expected_incarnation.clone(), 4_096).expect("bootstrap");
    append_ledger_at(&path, vec![expected_record.clone()], 1_700).expect("append");

    let mut expected =
        ReachabilityLedger::bootstrap(expected_incarnation, 4_096, LedgerCounters::default());
    expected.append_obligations(vec![expected_record], 1_700);
    assert_eq!(read_ledger_at(&path).expect("read back"), expected);
}

/// A startup-time ensure-bootstrapped call must not retire the live ledger on
/// every process restart. In particular, a later bootstrap offset is not a
/// reason to erase obligations or advance the durable cursor while the full
/// incarnation identity still matches.
#[test]
fn bootstrap_is_a_byte_preserving_noop_for_the_bound_incarnation() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider/123.json");
    let bound = incarnation("adk-chan", 42, Some("nonce-a"), 900);

    bootstrap_ledger_at(&path, bound.clone(), 100).expect("first bootstrap");
    append_ledger_at(&path, vec![obligation_record(100, 200)], 1_700).expect("append");
    let before = std::fs::read(&path).expect("read before ensure");
    let ledger_before = read_ledger_at(&path).expect("ledger before ensure");

    bootstrap_ledger_at(&path, bound, 999).expect("ensure same incarnation");

    assert_eq!(
        std::fs::read(&path).expect("read after ensure"),
        before,
        "same-incarnation bootstrap must not rewrite the sidecar"
    );
    assert_eq!(
        read_ledger_at(&path).expect("ledger after ensure"),
        ledger_before,
        "same-incarnation bootstrap must preserve obligations and cursor"
    );
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
fn a_ledger_from_another_schema_version_is_rejected_without_migration() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider/9.json");
    bootstrap_test_ledger(&path);
    let bumped = std::fs::read_to_string(&path)
        .expect("read")
        .replace("\"schema_version\": 1", "\"schema_version\": 2");
    std::fs::write(&path, bumped).expect("rewrite");

    assert_eq!(
        read_ledger_at(&path),
        None,
        "the reader must reject an unknown schema rather than invent coverage"
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

/// Sequential read-modify-write composition: a later append reads and preserves
/// the record written by the earlier append.
#[test]
fn sequential_append_ledger_at_calls_compose_without_lost_records() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider/1001.json");
    bootstrap_test_ledger(&path);

    let extinctions =
        append_ledger_at(&path, vec![obligation_record(100, 200)], 5_000).expect("first append");

    assert!(extinctions.is_empty(), "first append should not overflow");
    let ledger = read_ledger_at(&path).expect("ledger present");
    assert_eq!(ledger.live_obligations().len(), 1);
    assert_eq!(ledger.counters.total_obligations, 1);

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

#[test]
fn observation_transaction_rejects_records_from_another_incarnation() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider/identity.json");
    let bound = incarnation("adk-chan", 42, None, 900);
    bootstrap_ledger_at(&path, bound.clone(), 0).expect("bootstrap");
    let before = std::fs::read(&path).expect("read before rejected transaction");
    let wrong_generation = CanonicalRecord {
        generation_mtime_ns: 41,
        start: 0,
        end: 10,
        identity: bound.identity(),
        reason: ObligationReason::AssistantText,
    };

    let error = record_observation_at(
        &path,
        &bound,
        0,
        vec![wrong_generation],
        10,
        10,
        false,
        1_000,
    )
    .expect_err("foreign generation must be rejected");

    assert_eq!(
        error,
        "observation record does not bind to the ledger incarnation"
    );
    assert_eq!(
        std::fs::read(&path).expect("read after rejected transaction"),
        before,
        "identity rejection must preserve both cursor and obligations"
    );
}

/// Two sequential write transactions without an intermediate caller-side read
/// both compose into the persisted ledger.
#[test]
fn sequential_appends_preserve_all_obligations() {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider/2002.json");
    bootstrap_test_ledger(&path);

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

/// Concurrent writers open independent flock file descriptors. The lock must
/// cover each complete read-modify-write transaction so neither thread loses
/// records read from the other's prior commit.
#[test]
fn concurrent_append_ledger_at_calls_preserve_every_obligation() {
    const APPENDS_PER_THREAD: u64 = 128;

    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("provider/3003.json");
    bootstrap_test_ledger(&path);

    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let mut writers = Vec::new();
    for writer in 0..2_u64 {
        let path = path.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        writers.push(std::thread::spawn(move || {
            barrier.wait();
            for index in 0..APPENDS_PER_THREAD {
                let start = (writer * APPENDS_PER_THREAD + index) * 10;
                append_ledger_at(
                    &path,
                    vec![obligation_record(start, start + 10)],
                    1_000 + index,
                )
                .expect("concurrent append");
            }
        }));
    }
    for writer in writers {
        writer.join().expect("writer thread");
    }

    let ledger = read_ledger_at(&path).expect("ledger present");
    assert_eq!(
        ledger.live_obligations().len(),
        2 * APPENDS_PER_THREAD as usize
    );
    assert_eq!(ledger.counters.total_obligations, 2 * APPENDS_PER_THREAD);
}

#[test]
fn mutation_transactions_reject_absent_and_unreadable_ledgers_without_replacing_them() {
    let dir = TempDir::new().expect("tempdir");
    let absent_path = dir.path().join("provider/absent.json");

    let absent_append_error = append_ledger_at(&absent_path, vec![obligation_record(0, 10)], 1_000)
        .expect_err("append must reject an absent ledger");
    let absent_retire_error =
        retire_ledger_at(&absent_path, incarnation("adk-chan", 43, None, 901), 10)
            .expect_err("retire must reject an absent ledger");
    println!("absent append error: {absent_append_error}");
    println!("absent retire error: {absent_retire_error}");
    assert_eq!(absent_append_error, "ledger not bootstrapped");
    assert_eq!(absent_retire_error, "ledger not bootstrapped");
    assert!(!ledger_file_exists(&absent_path));
    println!(
        "absent file exists after mutations: {}",
        ledger_file_exists(&absent_path)
    );

    let unreadable_path = dir.path().join("provider/unreadable.json");
    let original = "{ definitely not a reachability ledger";
    std::fs::write(&unreadable_path, original).expect("write malformed ledger");
    let unreadable_append_error =
        append_ledger_at(&unreadable_path, vec![obligation_record(0, 10)], 1_000)
            .expect_err("append must reject an unreadable ledger");
    let unreadable_retire_error =
        retire_ledger_at(&unreadable_path, incarnation("adk-chan", 43, None, 901), 10)
            .expect_err("retire must reject an unreadable ledger");
    println!("unreadable append error: {unreadable_append_error}");
    println!("unreadable retire error: {unreadable_retire_error}");
    assert_eq!(
        unreadable_append_error,
        "ledger unreadable or schema incompatible"
    );
    assert_eq!(
        unreadable_retire_error,
        "ledger unreadable or schema incompatible"
    );
    assert_eq!(
        std::fs::read_to_string(&unreadable_path).expect("malformed source remains readable"),
        original,
        "failed mutations must leave the original file untouched"
    );
    println!(
        "unreadable source preserved: {}",
        std::fs::read_to_string(&unreadable_path).expect("malformed source remains readable")
            == original
    );
}

/// Non-obligation records passed to append_obligations are ignored without
/// entering the live set or changing the monotone obligation counter.
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

    let extinctions = ledger.append_obligations(records, 1_000);
    assert!(extinctions.is_empty());
    assert_eq!(ledger.live_obligations().len(), 1);
    assert_eq!(ledger.live_obligations()[0].start, 100);
    assert_eq!(ledger.counters.total_obligations, 1);
}
