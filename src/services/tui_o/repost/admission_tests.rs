use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::time::Duration;

use chrono::Utc;
use sqlx::PgPool;
use tokio::sync::watch;

use super::admission::{
    AdmittedKeys, Admitter, ApprovedRejected, BacklogReason, Candidate, OriginalGate, PriorBudget,
    classify, posted_receipts, report_backlog,
};
use super::config::{RepostConfig, RepostSwitch};
use super::identity::{PieceKey, marker, piece_of};
use super::io::probe::evidence::EvidenceScope;
use super::io::probe::matcher::{
    Attribution, AttributionSnapshot, ObservedMessage, RecoveryKind, match_observations,
};
use super::io::probe::tests::{absent_evidence, run};
use super::o_piece_attempts::{
    AttemptResult, GrantOutcome, GrantRequest, Intent, SlotGrant, attempts, grant, settle,
};
use super::o_piece_delivery::{
    AdmitOutcome, Receipt, ReceiptMethod, ReceiptOutcome, load, record_receipt,
};
use super::provenance::{ProvenanceEntry, ProvenanceLog};
use super::send::RepostIds;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::tui_o::shadow::tap::TuiOConfig;
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};
use crate::services::tui_o::store::ledger::{PieceOutcome, PieceRecord};

pub(super) const CHANNEL: u64 = 6325;
pub(super) const BOT: u64 = 900;

fn unit(native_key: &str, kind: UnitKind) -> UnitKey {
    UnitKey {
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        native_key: native_key.into(),
        kind,
    }
}

fn record(native_key: &str, payload: &str, outcome: Option<PieceOutcome>) -> PieceRecord {
    PieceRecord {
        unit_key: unit(native_key, UnitKind::Body),
        piece_index: 0,
        payload: payload.into(),
        anchor_id: 40,
        epoch: 1,
        prepared_at: Utc::now(),
        outcome,
    }
}

pub(super) fn switch(enabled: bool) -> RepostSwitch {
    let boot = TuiOConfig {
        repost: RepostConfig { enabled },
        ..TuiOConfig::default()
    };
    RepostSwitch::new(Some(&boot), watch::channel(None).1)
}

pub(super) fn sidecar(dir: &tempfile::TempDir) -> ProvenanceLog {
    ProvenanceLog::open(&dir.path().join("repost-provenance.jsonl")).unwrap()
}

/// Activates at `frontier` and writes the intent each `(serial, record)` had at its POST.
fn sent_while_on(log: &mut ProvenanceLog, frontier: u64, sent: &[(u64, &PieceRecord)]) {
    let generation = log.activate(frontier).unwrap().generation;
    for (serial, record) in sent {
        let intent = ProvenanceEntry::intent(*serial, record, generation).unwrap();
        log.append(intent).unwrap();
    }
}

fn request(key: &PieceKey, revision: i64, intent: Intent) -> GrantRequest<'_> {
    GrantRequest {
        key,
        expected_revision: revision,
        intent,
        owner: "node-a",
        run_id: "run",
        ttl: Duration::from_secs(60),
    }
}

/// Consumes the next slot and settles it as an uncertain send; returns its slot.
pub(super) async fn send_uncertain(pool: &PgPool, key: &PieceKey, intent: Intent) -> u8 {
    let revision = load(pool, key).await.unwrap().unwrap().revision;
    let granted: SlotGrant = match grant(pool, request(key, revision, intent)).await.unwrap() {
        GrantOutcome::Granted(granted) => granted,
        other => panic!("grant: {other:?}"),
    };
    settle(pool, &granted, AttemptResult::Uncertain)
        .await
        .unwrap();
    granted.slot()
}

pub(super) async fn next_grant(pool: &PgPool, key: &PieceKey) -> GrantOutcome {
    let revision = load(pool, key).await.unwrap().unwrap().revision;
    grant(pool, request(key, revision, Intent::AutoReconfirm))
        .await
        .unwrap()
}

#[test]
fn only_an_uncertain_original_sent_while_on_is_eligible_and_the_rest_is_reported_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut log = sidecar(&dir);
    let not_found = Some(PieceOutcome::NotFound);
    let ledger = [
        (0, record("before-a", "a", not_found.clone())),
        (1, record("before-open", "b", None)),
        (
            2,
            record("before-c", "c", Some(PieceOutcome::Unresolved("t".into()))),
        ),
        (3, record("on", "d", not_found.clone())),
        (
            4,
            record(
                "on-no-intent",
                "e",
                Some(PieceOutcome::Ambiguous(vec![7, 8])),
            ),
        ),
        (5, record("on-posted", "f", Some(PieceOutcome::Posted(50)))),
        (
            6,
            record("on-rejected", "g", Some(PieceOutcome::Rejected(403))),
        ),
        (7, record("on-mismatch", "h", None)),
    ];
    let changed = record("on-mismatch", "h but changed", None);
    sent_while_on(&mut log, 3, &[(3, &ledger[3].1), (5, &ledger[5].1)]);
    sent_while_on(&mut log, 3, &[(6, &ledger[6].1), (7, &changed)]);
    let classes: Vec<_> = ledger
        .iter()
        .map(|(serial, record)| classify(*serial, record, log.state()))
        .collect();
    let before = Candidate::Backlog(BacklogReason::BeforeActivation);
    assert_eq!(classes[..3], [before.clone(), before.clone(), before]);
    assert!(matches!(&classes[3], Candidate::Eligible(original) if original.serial == 3));
    assert_eq!(classes[4], Candidate::Backlog(BacklogReason::NoIntent));
    assert_eq!(classes[5..7], [Candidate::Settled, Candidate::Settled]);
    assert_eq!(
        classes[7],
        Candidate::Backlog(BacklogReason::IntentMismatch)
    );

    let latest = || ledger.iter().map(|(serial, record)| (*serial, record));
    let reported = report_backlog(&mut log, latest()).unwrap();
    let serials: Vec<u64> = reported.iter().map(|entry| entry.serial).collect();
    assert_eq!(serials, [0, 1, 2, 4, 7]);
    assert!(report_backlog(&mut log, latest()).unwrap().is_empty());
    let mut reopened = sidecar(&dir);
    assert!(report_backlog(&mut reopened, latest()).unwrap().is_empty());
}

#[test]
fn a_torn_sidecar_tail_is_cut_and_any_other_damage_refuses_the_sidecar() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("repost-provenance.jsonl");
    let mut log = ProvenanceLog::open(&path).unwrap();
    let sent = record("on", "d", None);
    sent_while_on(&mut log, 0, &[(0, &sent)]);
    let kept = log.state().clone();
    let append = |bytes: &[u8]| {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        file.write_all(bytes).unwrap();
    };
    append(b"{\"at\":\"2026-10-10T00:00:00Z\",\"entry\":{\"type\":\"admitt");
    assert_eq!(ProvenanceLog::open(&path).unwrap().state(), &kept);
    assert!(std::fs::read(&path).unwrap().ends_with(b"\n"));

    let unknown =
        b"{\"at\":\"2026-10-10T00:00:00Z\",\"entry\":{\"type\":\"later\",\"serial\":0}}\n";
    append(unknown);
    let refused = ProvenanceLog::open(&path).unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidData);
}

#[tokio::test]
async fn with_the_switch_off_no_admitter_exists_to_reach_postgres_or_the_sidecar() {
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://nobody@127.0.0.1:1/none")
        .unwrap();
    assert!(Admitter::when_on(&mut switch(false), &pool, "node-a", BOT).is_none());
    assert!(Admitter::when_on(&mut switch(true), &pool, "node-a", BOT).is_some());
}

#[tokio::test]
async fn an_admission_a_crash_or_outage_cut_resumes_from_the_sidecar_with_one_slot_zero_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let mut log = sidecar(&dir);
    let ledger = [
        (3, record("cut", "x", None)),
        (4, record("committed", "y", None)),
    ];
    sent_while_on(&mut log, 3, &[(3, &ledger[0].1), (4, &ledger[1].1)]);
    let mut on = switch(true);
    let admitter = Admitter::when_on(&mut on, &pool, "node-a", BOT).unwrap();
    // The crash cut serial 3 after its pending entry and serial 4 after its PostgreSQL commit.
    let other = tempfile::tempdir().unwrap();
    let mut elsewhere = sidecar(&other);
    sent_while_on(&mut elsewhere, 3, &[(4, &ledger[1].1)]);
    let Candidate::Eligible(committed) = classify(4, &ledger[1].1, elsewhere.state()) else {
        panic!("serial 4 is an on original");
    };
    admitter
        .adopt_uncertain(&mut elsewhere, &committed)
        .await
        .unwrap();
    for serial in [3, 4] {
        log.append(ProvenanceEntry::AdmissionPending { serial })
            .unwrap();
    }
    let mut log = sidecar(&dir);
    let record = |serial| ledger.iter().find(|(s, _)| *s == serial).map(|(_, r)| r);

    let down = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy(&db.database_url)
        .unwrap();
    down.close().await;
    let outage = Admitter::when_on(&mut on, &down, "node-a", BOT).unwrap();
    let failed = outage.resume_pending(&mut log, record).await;
    assert!(
        failed.iter().all(|(_, result)| result.is_err()),
        "{failed:?}"
    );
    assert_eq!(log.state().pending().collect::<Vec<_>>(), [3, 4]);

    let resumed = admitter.resume_pending(&mut log, record).await;
    let admitted = matches!(
        resumed[..],
        [
            (3, Ok(AdmitOutcome::Admitted(_))),
            (4, Ok(AdmitOutcome::Existing(_)))
        ]
    );
    assert!(admitted, "{resumed:?}");
    assert_eq!(log.state().pending().count(), 0);
    for (_, record) in &ledger {
        let key = piece_of(record).unwrap();
        let spent: Vec<_> = attempts(&pool, &key).await.unwrap();
        assert_eq!(spent.len(), 1);
        assert_eq!(
            (spent[0].slot, spent[0].result),
            (0, Some(AttemptResult::Uncertain))
        );
    }
    assert!(
        admitter
            .resume_pending(&mut sidecar(&dir), record)
            .await
            .is_empty()
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn a_new_holder_never_resends_an_admitted_original_and_a_changed_payload_gets_no_budget_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let mut log = sidecar(&dir);
    let sent = record("t10", "the piece", Some(PieceOutcome::NotFound));
    sent_while_on(&mut log, 3, &[(3, &sent)]);
    let mut on = switch(true);
    let holder_a = Admitter::when_on(&mut on, &pool, "node-a", BOT).unwrap();
    let Candidate::Eligible(original) = classify(3, &sent, log.state()) else {
        panic!("an on original");
    };
    holder_a.adopt_uncertain(&mut log, &original).await.unwrap();
    let key = original.key.clone();
    assert_eq!(send_uncertain(&pool, &key, Intent::AutoReconfirm).await, 1);

    // Holder B re-derives the piece under its own serial, as after a fork or a renumbered ledger.
    let holder_b = Admitter::when_on(&mut on, &pool, "node-b", BOT).unwrap();
    let index = holder_b.admitted_keys(CHANNEL).await;
    assert_eq!(index.original(&sent.unit_key, 0), OriginalGate::Admitted);
    assert_eq!(index.original(&sent.unit_key, 1), OriginalGate::Send);
    let tool = unit("t10", UnitKind::Tool);
    assert_eq!(index.original(&tool, 0), OriginalGate::Send);

    let dir_b = tempfile::tempdir().unwrap();
    let mut log_b = sidecar(&dir_b);
    let changed = record("t10", "the piece, edited", Some(PieceOutcome::NotFound));
    sent_while_on(&mut log_b, 17, &[(17, &changed)]);
    let Candidate::Eligible(rederived) = classify(17, &changed, log_b.state()) else {
        panic!("an on original of holder b");
    };
    let conflict = holder_b
        .adopt_uncertain(&mut log_b, &rederived)
        .await
        .unwrap();
    let AdmitOutcome::IdentityConflict(kept) = conflict else {
        panic!("a changed payload under an admitted key: {conflict:?}");
    };
    assert_eq!(
        (kept.payload.as_str(), kept.origin_serial),
        ("the piece", 3)
    );
    let slots: Vec<u8> = attempts(&pool, &key)
        .await
        .unwrap()
        .iter()
        .map(|a| a.slot)
        .collect();
    assert_eq!(slots, [0, 1]);
    assert_eq!(send_uncertain(&pool, &key, Intent::AutoReconfirm).await, 2);
    assert_eq!(next_grant(&pool, &key).await, GrantOutcome::CapReached);

    let down = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy(&db.database_url)
        .unwrap();
    down.close().await;
    let unread = Admitter::when_on(&mut on, &down, "node-b", BOT).unwrap();
    let index = unread.admitted_keys(CHANNEL).await;
    assert!(matches!(index, AdmittedKeys::Unreadable(_)));
    assert_eq!(index.original(&sent.unit_key, 0), OriginalGate::Unknown);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn a_local_posted_of_an_admitted_piece_becomes_its_receipt_before_any_further_slot_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let mut log = sidecar(&dir);
    let sent = record("t08", "piece", Some(PieceOutcome::NotFound));
    sent_while_on(&mut log, 3, &[(3, &sent)]);
    let mut on = switch(true);
    let admitter = Admitter::when_on(&mut on, &pool, "node-a", BOT).unwrap();
    let Candidate::Eligible(original) = classify(3, &sent, log.state()) else {
        panic!("an on original");
    };
    admitter.adopt_uncertain(&mut log, &original).await.unwrap();
    let key = original.key.clone();
    // The slot 1 send posted and its local Posted is durable; Discord changed the content and the
    // receipt write found PostgreSQL down.
    let revision = load(&pool, &key).await.unwrap().unwrap().revision;
    let GrantOutcome::Granted(granted) =
        grant(&pool, request(&key, revision, Intent::AutoReconfirm))
            .await
            .unwrap()
    else {
        panic!("slot 1");
    };
    settle(&pool, &granted, AttemptResult::Created)
        .await
        .unwrap();
    let down = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy(&db.database_url)
        .unwrap();
    down.close().await;
    let outage = Admitter::when_on(&mut on, &down, "node-a", BOT).unwrap();
    assert!(outage.promote_posted(&key, 61).await.is_err());

    let reposted = record("t08", "piece", Some(PieceOutcome::Posted(61)));
    let unrelated = record("other", "x", Some(PieceOutcome::Posted(62)));
    let admitted = BTreeSet::from([key.clone()]);
    let promote = posted_receipts([&sent, &reposted, &unrelated], &admitted);
    assert_eq!(promote, [(key.clone(), 61)]);
    let recorded = admitter.promote_posted(&key, 61).await.unwrap();
    assert!(matches!(recorded, ReceiptOutcome::Recorded { .. }));
    assert_eq!(next_grant(&pool, &key).await, GrantOutcome::Resolved);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn an_operator_retry_and_the_automatic_reconfirm_share_one_three_post_budget_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let mut log = sidecar(&dir);
    let mut on = switch(true);
    let admitter = Admitter::when_on(&mut on, &pool, "node-a", BOT).unwrap();
    let approved = |native_key: &str| ApprovedRejected {
        rejected_serial: 2,
        key: PieceKey::new(unit(native_key, UnitKind::Body), 0).unwrap(),
        payload: "piece".into(),
        anchor: 40,
    };
    let operator = || Intent::OperatorResume {
        approval_id: "approval-1".into(),
    };

    // 403 original, approved retry goes uncertain, the automatic reconfirm takes the last slot.
    let first = approved("t18");
    let known = PriorBudget::Known { counted_posts: 1 };
    let admitted = admitter.adopt_operator(&first, known).await.unwrap();
    assert!(matches!(admitted, AdmitOutcome::Admitted(_)));
    assert_eq!(send_uncertain(&pool, &first.key, operator()).await, 1);
    let retry = record(
        "t18",
        "piece",
        Some(PieceOutcome::Unresolved("timeout".into())),
    );
    sent_while_on(&mut log, 9, &[(9, &retry)]);
    let Candidate::Eligible(uncertain) = classify(9, &retry, log.state()) else {
        panic!("the retry was sent while on");
    };
    let joined = admitter
        .adopt_uncertain(&mut log, &uncertain)
        .await
        .unwrap();
    assert!(matches!(joined, AdmitOutcome::Existing(_)));
    assert_eq!(
        send_uncertain(&pool, &first.key, Intent::AutoReconfirm).await,
        2
    );
    assert_eq!(
        next_grant(&pool, &first.key).await,
        GrantOutcome::CapReached
    );
    let results: Vec<_> = attempts(&pool, &first.key).await.unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].result, Some(AttemptResult::Rejected));

    // Sends an operator already made before admission count against the same three.
    let earlier = approved("t18-earlier");
    let known = PriorBudget::Known { counted_posts: 2 };
    admitter.adopt_operator(&earlier, known).await.unwrap();
    assert_eq!(send_uncertain(&pool, &earlier.key, operator()).await, 2);
    assert_eq!(
        next_grant(&pool, &earlier.key).await,
        GrantOutcome::CapReached
    );
    let unknown = approved("t18-unknown");
    admitter
        .adopt_operator(&unknown, PriorBudget::Unknown)
        .await
        .unwrap();
    let refused = next_grant(&pool, &unknown.key).await;
    assert_eq!(
        refused,
        GrantOutcome::Failed(super::o_piece_delivery::Failure::CapUnknown)
    );
    pool.close().await;
    db.drop().await;
}

/// Admits each `(serial, native_key)` as an on original of `payload` that went uncertain.
pub(super) async fn admit_on(
    pool: &PgPool,
    log: &mut ProvenanceLog,
    pieces: &[(u64, &str)],
    payload: &str,
) -> Vec<PieceKey> {
    let records: Vec<_> = pieces
        .iter()
        .map(|(serial, native_key)| {
            (
                *serial,
                record(native_key, payload, Some(PieceOutcome::NotFound)),
            )
        })
        .collect();
    let sent: Vec<_> = records
        .iter()
        .map(|(serial, record)| (*serial, record))
        .collect();
    sent_while_on(log, 3, &sent);
    let mut on = switch(true);
    let admitter = Admitter::when_on(&mut on, pool, "node-a", BOT).unwrap();
    let mut keys = Vec::new();
    for (serial, record) in &records {
        let Candidate::Eligible(original) = classify(*serial, record, log.state()) else {
            panic!("an on original");
        };
        admitter.adopt_uncertain(log, &original).await.unwrap();
        keys.push(original.key);
    }
    keys
}

async fn scope_now(pool: &PgPool, key: &PieceKey) -> EvidenceScope {
    let row = load(pool, key).await.unwrap().unwrap();
    let spent = attempts(pool, key).await.unwrap();
    EvidenceScope::of(&row, &spent, &run("run-1")).unwrap()
}

fn sent_by_bot(id: u64, content: &str) -> ObservedMessage {
    ObservedMessage {
        id,
        channel_id: CHANNEL,
        author_id: BOT,
        content: content.into(),
        rich_embeds: 0,
        footers: Vec::new(),
        nonce: None,
    }
}

fn attribute(
    scope: &EvidenceScope,
    snapshot: AttributionSnapshot,
    seen: &ObservedMessage,
) -> Attribution {
    let mut into = Attribution::default();
    match_observations(scope, &snapshot, [seen], &mut into);
    into
}

fn nobody_else() -> AttributionSnapshot {
    AttributionSnapshot::Known {
        receipts: BTreeMap::new(),
        same_payload: Vec::new(),
    }
}

#[tokio::test]
async fn f4_nonce_receipt_closes_the_existing_budget_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let key = admit_on(&pool, &mut sidecar(&dir), &[(3, "f4-nonce")], "piece")
        .await
        .remove(0);
    let scope = scope_now(&pool, &key).await;
    let nonce = RepostIds::for_piece(&marker(&key))
        .unwrap()
        .nonce()
        .to_owned();
    let returned = ObservedMessage {
        nonce: Some(nonce),
        ..sent_by_bot(41, "piece, as Discord rendered it")
    };
    let seen = attribute(&scope, nobody_else(), &returned);
    let candidate = &seen.found[&41];
    assert_eq!(candidate.recovery, RecoveryKind::OriginalRecovered);
    assert_eq!(candidate.receipt.slot, Some(0));

    // A write that never reached PostgreSQL resolves nothing.
    let down = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy(&db.database_url)
        .unwrap();
    down.close().await;
    assert!(record_receipt(&down, &candidate.receipt).await.is_err());
    let unchanged = load(&pool, &key).await.unwrap().unwrap();
    assert_eq!(unchanged.revision, scope.row_revision);

    let recorded = record_receipt(&pool, &candidate.receipt).await.unwrap();
    assert!(matches!(recorded, ReceiptOutcome::Recorded { .. }));
    assert_eq!(next_grant(&pool, &key).await, GrantOutcome::Resolved);
    let spent = attempts(&pool, &key).await.unwrap();
    assert_eq!(spent.iter().map(|a| a.slot).collect::<Vec<_>>(), [0]);
    assert_eq!(load(&pool, &key).await.unwrap().unwrap().payload, "piece");
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn f4_receipt_race_never_reattributes_a_message_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let pieces = [(3, "f4-a"), (4, "f4-b")];
    let keys = admit_on(&pool, &mut sidecar(&dir), &pieces, "same").await;
    let (a, b) = (&keys[0], &keys[1]);
    let message = sent_by_bot(51, "same");
    let seen_by_a = attribute(&scope_now(&pool, a).await, nobody_else(), &message);
    let candidate = seen_by_a.found[&51].receipt.clone();
    assert_eq!(candidate.method, ReceiptMethod::ExactMatch);

    // B records the same message first.
    let first = Receipt {
        key: b.clone(),
        ..candidate.clone()
    };
    assert!(matches!(
        record_receipt(&pool, &first).await.unwrap(),
        ReceiptOutcome::Recorded { .. }
    ));
    let late = record_receipt(&pool, &candidate).await.unwrap();
    assert_eq!(late, ReceiptOutcome::AttributedElsewhere(b.clone()));
    assert_eq!(
        record_receipt(&pool, &first).await.unwrap(),
        ReceiptOutcome::Known
    );

    let now_known = AttributionSnapshot::Known {
        receipts: BTreeMap::from([(51, b.clone())]),
        same_payload: Vec::new(),
    };
    assert!(attribute(&scope_now(&pool, a).await, now_known, &message).is_clear());
    assert_ne!(next_grant(&pool, a).await, GrantOutcome::Resolved);
    pool.close().await;
    db.drop().await;
}

#[tokio::test]
async fn f4_row_change_invalidates_absence_evidence_pg() {
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let dir = tempfile::tempdir().unwrap();
    let pieces = [(3, "f4-slot"), (4, "f4-receipt")];
    let keys = admit_on(&pool, &mut sidecar(&dir), &pieces, "piece").await;
    let current = |key: PieceKey| {
        let pool = pool.clone();
        async move {
            let row = load(&pool, &key).await.unwrap().unwrap();
            (row, attempts(&pool, &key).await.unwrap())
        }
    };
    let run_1 = run("run-1");

    // Another holder consumes and settles slot 1 after the passes.
    let slot = &keys[0];
    let evidence = absent_evidence(scope_now(&pool, slot).await, run_1.clone());
    let (row, spent) = current(slot.clone()).await;
    assert!(evidence.validate_current(&row, &spent, &run_1));
    assert_eq!(send_uncertain(&pool, slot, Intent::AutoReconfirm).await, 1);
    let (row, spent) = current(slot.clone()).await;
    assert!(!evidence.validate_current(&row, &spent, &run_1));
    let stale = grant(
        &pool,
        request(slot, evidence.scope().row_revision, Intent::AutoReconfirm),
    );
    assert!(matches!(stale.await.unwrap(), GrantOutcome::Stale { .. }));
    assert_eq!(attempts(&pool, slot).await.unwrap().len(), 2);

    // A receipt moves only the revision; the evidence is stale all the same.
    let receipt = &keys[1];
    let evidence = absent_evidence(scope_now(&pool, receipt).await, run_1.clone());
    let recorded = Receipt {
        key: receipt.clone(),
        message_id: 61,
        author_id: BOT,
        slot: None,
        method: ReceiptMethod::Marker,
    };
    record_receipt(&pool, &recorded).await.unwrap();
    let (row, spent) = current(receipt.clone()).await;
    assert_eq!(spent.len(), 1);
    assert!(!evidence.validate_current(&row, &spent, &run_1));
    pool.close().await;
    db.drop().await;
}
