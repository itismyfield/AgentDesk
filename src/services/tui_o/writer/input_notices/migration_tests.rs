use super::*;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId, SourceRange, UnitKind};
use crate::services::tui_o::store::ledger::input_notices::{
    MigrationHistory, MigrationSnapshot, MigrationSource, NoticeEvidence,
};
use crate::services::tui_o::store::{Initialized, OStore, StoreConfig, fault};
use crate::services::tui_o::writer::input_notices::NoticeOrigin;
use crate::services::tui_o::writer::{PostOutcome, SeenMessage};
use chrono::Utc;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Port {
    messages: Mutex<Vec<SeenMessage>>,
    unreadable: AtomicBool,
    endless: AtomicBool,
    fail_after: AtomicUsize,
    reads: AtomicUsize,
    posts: AtomicUsize,
}

impl DiscordPort for Port {
    fn bot_id(&self) -> u64 {
        42
    }

    fn post(&self, _: u64, _: String) -> impl Future<Output = PostOutcome> + Send + 'static {
        self.posts.fetch_add(1, Ordering::SeqCst);
        async { PostOutcome::Refused(403) }
    }

    async fn history_after(&self, _: u64, after: u64) -> Result<Vec<SeenMessage>, String> {
        let read = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
        let fail_after = self.fail_after.load(Ordering::SeqCst);
        if self.unreadable.load(Ordering::SeqCst) || (fail_after > 0 && read > fail_after) {
            return Err("HTTP 403".into());
        }
        if self.endless.load(Ordering::SeqCst) {
            return Ok((1..=confirm::HISTORY_PAGE)
                .map(|offset| SeenMessage {
                    id: after + offset as u64,
                    author_id: 1,
                    content: "unrelated".into(),
                })
                .collect());
        }
        Ok(self
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter(|message| message.id > after)
            .take(confirm::HISTORY_PAGE)
            .cloned()
            .collect())
    }

    async fn message(&self, _: u64, id: u64) -> Result<Option<SeenMessage>, String> {
        if self.unreadable.load(Ordering::SeqCst) {
            return Err("HTTP 403".into());
        }
        Ok(self
            .messages
            .lock()
            .unwrap()
            .iter()
            .find(|message| message.id == id)
            .cloned())
    }
}

struct Lease;

impl DeliveryLease for Lease {
    type Held = ();
    fn try_acquire(&self, _: u64, _: u64) -> Option<()> {
        Some(())
    }
}

struct Alarms;

impl AlarmSink for Alarms {
    fn raise(&self, _: u64, _: WriterAlarm) {}
}

struct Fixture {
    runtime: tempfile::TempDir,
    store: OStore,
    port: Arc<Port>,
}

impl Fixture {
    fn new() -> Self {
        let runtime = tempfile::tempdir().unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
            .unwrap()
            .unwrap();
        store
            .begin_era(&[7], Utc::now(), |channel| {
                Ok(Initialized {
                    channel,
                    sources: Vec::new(),
                    initial_anchor: 10,
                    build_digest: "fixture".into(),
                    at: Utc::now(),
                })
            })
            .unwrap();
        Self {
            runtime,
            store,
            port: Arc::default(),
        }
    }

    fn writer(&self) -> ChannelWriter<Port, Lease, Alarms> {
        let era = self.store.read_era().unwrap().unwrap();
        ChannelWriter::new(
            self.store.open_channel(&era, 7).unwrap().unwrap(),
            Arc::new(OwnershipGate::default()),
            Arc::clone(&self.port),
            Lease,
            Alarms,
        )
    }

    fn message(&self, id: u64, content: &str) {
        self.port.messages.lock().unwrap().push(SeenMessage {
            id,
            author_id: 42,
            content: content.into(),
        });
    }
}

fn evidence(uuid: &str, prompt: &str, start: u64) -> NoticeEvidence {
    let raw_record = serde_json::to_vec(&serde_json::json!({
        "type":"user", "uuid":uuid, "message":{"role":"user","content":prompt}
    }))
    .unwrap();
    let record = SourceRange {
        source: SourceId {
            session_id: "native".into(),
            path: "/fixture/transcript.jsonl".into(),
            dev: 1,
            ino: 2,
        },
        start,
        end: start + raw_record.len() as u64 + 1,
    };
    NoticeEvidence {
        record: record.clone(),
        raw_record,
        prompt: Some(prompt.into()),
        prompt_sha256: Some(hex::encode(Sha256::digest(prompt.as_bytes()))),
        unit_key: Some(UnitKey {
            channel_id: 7,
            provider: ShadowProvider::Claude,
            native_key: uuid.into(),
            kind: UnitKind::InputNotice,
        }),
        payload: Some(format!("-# ⌨️ {prompt}")),
        origin: Some(Box::new(NoticeOrigin {
            record,
            logical_session: "pane".into(),
            binding_seq: 3,
            execution_nonce: "original".into(),
        })),
    }
}

fn seed(
    writer: &mut ChannelWriter<Port, Lease, Alarms>,
    inputs: &[NoticeEvidence],
    history: MigrationHistory,
) {
    writer
        .record(LedgerEntry::InputNotice {
            entry: Box::new(NoticeEntry::MigrationStarted {
                snapshot: MigrationSnapshot {
                    sources: vec![MigrationSource {
                        source: inputs[0].record.source.clone(),
                        lower: 0,
                        upper: 10_000,
                        prefix_hash: "0".repeat(64),
                    }],
                    binding_checkpoint: 3,
                    history,
                },
            }),
        })
        .unwrap();
    for input in inputs {
        writer
            .record(LedgerEntry::InputNotice {
                entry: Box::new(NoticeEntry::Materialized {
                    evidence: Box::new(input.clone()),
                }),
            })
            .unwrap();
    }
}

fn fixed() -> MigrationHistory {
    MigrationHistory::Fixed {
        lower: 10,
        upper: 90,
    }
}

#[tokio::test]
async fn legacy_identical_inputs_remain_unresolved_without_receipts_or_posts() {
    let fixture = Fixture::new();
    let inputs = [evidence("p", "same", 0), evidence("q", "same", 1_000)];
    fixture.message(40, "-# ⌨️ same");
    let mut writer = fixture.writer();
    seed(&mut writer, &inputs, fixed());
    let keys: Vec<_> = inputs
        .iter()
        .map(|input| input.unit_key.clone().unwrap())
        .collect();
    assert!(
        writer
            .reconcile_notice_migration(true, &[], &keys, &[])
            .await
            .unwrap()
            .is_empty()
    );
    let ledger = writer.store().ledger();
    assert_eq!(ledger.input_notices().unresolved.len(), 2);
    assert!(ledger.input_notices().imported.is_empty());
    assert!(keys.iter().all(|key| ledger.excluded(key).is_none()));
    assert_eq!((ledger.anchor(), ledger.next_serial()), (10, 0));
    let reads = fixture.port.reads.load(Ordering::SeqCst);
    drop(writer);
    let mut writer = fixture.writer();
    assert!(
        writer
            .reconcile_notice_migration(true, &[], &keys, &[])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(writer.store().ledger().input_notices().unresolved.len(), 2);
    assert_eq!(fixture.port.reads.load(Ordering::SeqCst), reads);
    assert_eq!(fixture.port.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_unique_legacy_match_stays_debt_once_without_moving_body_frontiers() {
    let fixture = Fixture::new();
    let input = evidence("p", "unique", 0);
    let key = input.unit_key.clone().unwrap();
    fixture.message(40, "-# ⌨️ unique");
    let mut writer = fixture.writer();
    seed(&mut writer, &[input], fixed());
    assert!(
        writer
            .reconcile_notice_migration(true, &[], std::slice::from_ref(&key), &[])
            .await
            .unwrap()
            .is_empty()
    );
    let ledger = writer.store().ledger();
    // The message may announce an input before the frozen lower, so it settles nothing.
    let unresolved: Vec<_> = ledger.input_notices().unresolved.values().collect();
    assert_eq!(unresolved.len(), 1);
    assert_eq!(unresolved[0].candidates, [40]);
    assert!(ledger.input_notices().imported.is_empty());
    assert!(ledger.excluded(&key).is_none());
    assert!(ledger.input_notices().reserved_ids.contains(&40));
    assert_eq!((ledger.anchor(), ledger.next_serial()), (10, 0));
    let reads = fixture.port.reads.load(Ordering::SeqCst);
    assert!(
        writer
            .reconcile_notice_migration(true, &[], std::slice::from_ref(&key), &[])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(fixture.port.reads.load(Ordering::SeqCst), reads);
    assert_eq!(fixture.port.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn ineligible_input_still_prevents_a_same_payload_legacy_receipt_import() {
    let fixture = Fixture::new();
    fixture.message(40, "-# ⌨️ same");
    let inputs = [evidence("p", "same", 0), evidence("q", "same", 1_000)];
    let eligible = inputs[0].unit_key.clone().unwrap();
    let keys: Vec<_> = inputs
        .iter()
        .map(|input| input.unit_key.clone().unwrap())
        .collect();
    let mut writer = fixture.writer();
    seed(&mut writer, &inputs, fixed());
    assert!(
        writer
            .reconcile_notice_migration(true, &[], std::slice::from_ref(&eligible), &[])
            .await
            .unwrap()
            .is_empty()
    );
    let ledger = writer.store().ledger();
    assert_eq!(ledger.input_notices().unresolved.len(), 2);
    assert!(ledger.input_notices().imported.is_empty());
    assert!(keys.iter().all(|key| ledger.excluded(key).is_none()));
    assert!(ledger.input_notices().reserved_ids.contains(&40));
    assert_eq!((ledger.anchor(), ledger.next_serial()), (10, 0));
    assert_eq!(fixture.port.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn excluded_input_is_not_audited_as_debt_but_still_prevents_a_legacy_collision_import() {
    let fixture = Fixture::new();
    fixture.message(40, "-# ⌨️ same");
    let inputs = [evidence("p", "same", 0), evidence("q", "same", 1_000)];
    let eligible = inputs[0].unit_key.clone().unwrap();
    let excluded = inputs[1].unit_key.clone().unwrap();
    let excluded_record = inputs[1].record.clone();
    let mut writer = fixture.writer();
    seed(&mut writer, &inputs, fixed());
    writer
        .record(LedgerEntry::Excluded {
            unit_key: excluded.clone(),
            reason: "input_notice_source".into(),
        })
        .unwrap();
    assert!(
        writer
            .reconcile_notice_migration(true, &[], std::slice::from_ref(&eligible), &[])
            .await
            .unwrap()
            .is_empty()
    );
    let ledger = writer.store().ledger();
    assert_eq!(ledger.input_notices().unresolved.len(), 1);
    assert!(ledger.input_notices().imported.is_empty());
    assert!(ledger.input_notices().reserved_ids.contains(&40));
    assert!(!ledger.input_notices().unresolved.contains_key(&(
        excluded_record.source,
        excluded_record.start,
        excluded_record.end,
    )));
    assert_eq!(ledger.excluded(&excluded), Some("input_notice_source"));
    assert!(ledger.excluded(&eligible).is_none());
    assert_eq!((ledger.anchor(), ledger.next_serial()), (10, 0));
    assert_eq!(fixture.port.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn posted_legacy_notice_is_never_reclassified_as_debt_on_the_next_poll_or_restart() {
    let fixture = Fixture::new();
    let input = evidence("p", "unique", 0);
    let key = input.unit_key.clone().unwrap();
    let mut writer = fixture.writer();
    seed(
        &mut writer,
        std::slice::from_ref(&input),
        MigrationHistory::Fixed {
            lower: 10,
            upper: 10,
        },
    );
    assert_eq!(
        writer
            .reconcile_notice_migration(true, &[], std::slice::from_ref(&key), &[])
            .await
            .unwrap(),
        vec![key.clone()]
    );
    writer
        .record(LedgerEntry::Prepared {
            serial: 0,
            unit_key: key.clone(),
            piece_index: 0,
            payload: input.payload.unwrap(),
            anchor_id: 10,
            epoch: 0,
            local_retry: None,
            notice_origin: input.origin,
        })
        .unwrap();
    writer
        .record(LedgerEntry::Posted {
            serial: 0,
            msg_id: 40,
        })
        .unwrap();
    assert!(
        writer
            .reconcile_notice_migration(true, &[], &[], &[])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        writer
            .store()
            .ledger()
            .input_notices()
            .unresolved
            .is_empty()
    );
    drop(writer);
    let mut writer = fixture.writer();
    assert!(
        writer
            .reconcile_notice_migration(true, &[], std::slice::from_ref(&key), &[])
            .await
            .unwrap()
            .is_empty()
    );
    let ledger = writer.store().ledger();
    assert!(ledger.input_notices().unresolved.is_empty());
    assert!(ledger.input_notices().imported.is_empty());
    assert!(ledger.excluded(&key).is_none());
    assert_eq!((ledger.anchor(), ledger.next_serial()), (40, 1));
    assert_eq!(fixture.port.reads.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.port.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn partial_legacy_history_preserves_discovered_candidate_reservations_after_failure() {
    let fixture = Fixture::new();
    *fixture.port.messages.lock().unwrap() = (11..=110)
        .map(|id| SeenMessage {
            id,
            author_id: if id == 40 { 42 } else { 1 },
            content: if id == 40 {
                "-# ⌨️ unique".into()
            } else {
                "unrelated".into()
            },
        })
        .collect();
    fixture.port.fail_after.store(1, Ordering::SeqCst);
    let input = evidence("p", "unique", 0);
    let key = input.unit_key.clone().unwrap();
    let record = input.record.clone();
    let mut writer = fixture.writer();
    seed(
        &mut writer,
        &[input],
        MigrationHistory::Fixed {
            lower: 10,
            upper: 10_000,
        },
    );
    assert!(
        writer
            .reconcile_notice_migration(true, &[], std::slice::from_ref(&key), &[])
            .await
            .unwrap()
            .is_empty()
    );
    let ledger = writer.store().ledger();
    let debt = ledger
        .input_notices()
        .unresolved
        .get(&(record.source, record.start, record.end))
        .unwrap();
    assert_eq!(debt.candidates, vec![40]);
    assert!(ledger.input_notices().reserved_ids.contains(&40));
    assert!(ledger.input_notices().imported.is_empty());
    assert!(ledger.excluded(&key).is_none());
    assert_eq!(fixture.port.reads.load(Ordering::SeqCst), 2);
    drop(writer);
    let mut writer = fixture.writer();
    assert!(
        writer
            .reconcile_notice_migration(true, &[], std::slice::from_ref(&key), &[])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        writer
            .store()
            .ledger()
            .input_notices()
            .reserved_ids
            .contains(&40)
    );
    assert_eq!(fixture.port.reads.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.port.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn empty_frozen_range_is_ready_and_never_searches_new_owner_inputs() {
    let fixture = Fixture::new();
    let input = evidence("p", "unique", 0);
    let key = input.unit_key.clone().unwrap();
    let mut writer = fixture.writer();
    seed(
        &mut writer,
        &[input],
        MigrationHistory::Fixed {
            lower: 10,
            upper: 10,
        },
    );
    assert_eq!(
        writer
            .reconcile_notice_migration(true, &[], std::slice::from_ref(&key), &[])
            .await
            .unwrap(),
        vec![key]
    );
    assert_eq!(fixture.port.reads.load(Ordering::SeqCst), 0);
    assert!(
        writer
            .store()
            .ledger()
            .input_notices()
            .unresolved
            .is_empty()
    );
}

#[tokio::test]
async fn records_before_the_migration_lower_stay_nonretroactive_beside_a_new_source() {
    let fixture = Fixture::new();
    let old = evidence("old", "stale", 0);
    let old_key = old.unit_key.clone().unwrap();
    let mut fresh = evidence("fresh", "new source", 0);
    fresh.record.source.path = "/fixture/new-source.jsonl".into();
    fresh.origin.as_mut().unwrap().record = fresh.record.clone();
    let fresh_key = fresh.unit_key.clone().unwrap();
    let mut writer = fixture.writer();
    writer
        .record(LedgerEntry::InputNotice {
            entry: Box::new(NoticeEntry::MigrationStarted {
                snapshot: MigrationSnapshot {
                    sources: vec![MigrationSource {
                        source: old.record.source.clone(),
                        lower: 1_000,
                        upper: 10_000,
                        prefix_hash: "0".repeat(64),
                    }],
                    binding_checkpoint: 3,
                    history: fixed(),
                },
            }),
        })
        .unwrap();
    for input in [old, fresh] {
        writer
            .record(LedgerEntry::InputNotice {
                entry: Box::new(NoticeEntry::Materialized {
                    evidence: Box::new(input),
                }),
            })
            .unwrap();
    }
    assert_eq!(
        writer
            .reconcile_notice_migration(true, &[], &[old_key.clone(), fresh_key.clone()], &[])
            .await
            .unwrap(),
        vec![fresh_key]
    );
    let ledger = writer.store().ledger();
    assert!(ledger.input_notices().imported.is_empty());
    assert!(ledger.input_notices().unresolved.is_empty());
    assert!(ledger.excluded(&old_key).is_none());
    assert_eq!((ledger.anchor(), ledger.next_serial()), (10, 0));
    assert_eq!(fixture.port.reads.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.port.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn incomplete_cohort_and_raw_straddling_membership_never_promote_unique_matches() {
    let fixture = Fixture::new();
    let input = evidence("p", "unique", 9_999);
    assert!(input.record.end > 10_000);
    let fresh = evidence("q", "unique", 20_000);
    let key = input.unit_key.clone().unwrap();
    let fresh_key = fresh.unit_key.clone().unwrap();
    fixture.message(40, "-# ⌨️ unique");
    let mut writer = fixture.writer();
    seed(&mut writer, &[input, fresh], fixed());
    let frozen = writer.store().ledger().input_notices().migration.clone();
    let ready = writer
        .reconcile_notice_migration(false, &[], &[key.clone(), fresh_key.clone()], &[])
        .await
        .unwrap();
    assert_eq!(ready, vec![fresh_key.clone()]);
    let ledger = writer.store().ledger();
    assert_eq!(ledger.input_notices().unresolved.len(), 1);
    assert!(ledger.excluded(&key).is_none());
    assert_eq!(ledger.input_notices().migration, frozen);
    let reads = fixture.port.reads.load(Ordering::SeqCst);
    assert_eq!(
        writer
            .reconcile_notice_migration(
                true,
                std::slice::from_ref(&fresh_key),
                std::slice::from_ref(&fresh_key),
                &[]
            )
            .await
            .unwrap(),
        vec![fresh_key]
    );
    assert_eq!(fixture.port.reads.load(Ordering::SeqCst), reads);
    assert_eq!(fixture.port.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn refused_or_over_budget_legacy_history_becomes_durable_debt_once() {
    for unavailable in [false, true] {
        let fixture = Fixture::new();
        fixture.port.unreadable.store(unavailable, Ordering::SeqCst);
        fixture.port.endless.store(!unavailable, Ordering::SeqCst);
        let input = evidence("p", "unique", 0);
        let key = input.unit_key.clone().unwrap();
        let mut writer = fixture.writer();
        seed(
            &mut writer,
            &[input],
            MigrationHistory::Fixed {
                lower: 10,
                upper: 10_000,
            },
        );
        assert!(
            writer
                .reconcile_notice_migration(true, &[], std::slice::from_ref(&key), &[])
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(writer.store().ledger().input_notices().unresolved.len(), 1);
        let reads = fixture.port.reads.load(Ordering::SeqCst);
        assert!(reads > 0 && reads <= confirm::MAX_PAGES);
        drop(writer);
        let mut writer = fixture.writer();
        assert!(
            writer
                .reconcile_notice_migration(true, &[], std::slice::from_ref(&key), &[])
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(fixture.port.reads.load(Ordering::SeqCst), reads);
        assert_eq!(fixture.port.posts.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn failed_migration_record_stops_before_import_or_ready_success() {
    let fixture = Fixture::new();
    fixture.message(40, "-# ⌨️ unique");
    let input = evidence("p", "unique", 0);
    let key = input.unit_key.clone().unwrap();
    let mut writer = fixture.writer();
    seed(&mut writer, &[input], fixed());
    let ledger = fixture.runtime.path().join("o_store/7/ledger.jsonl");
    let _fault = fault::plant(
        &ledger,
        fault::Step::Append(fault::Keep::Nothing),
        std::io::ErrorKind::StorageFull,
        None,
    );
    assert_eq!(
        writer
            .reconcile_notice_migration(true, &[], std::slice::from_ref(&key), &[])
            .await,
        Err(Step::Stopped)
    );
    assert!(writer.is_stopped());
    assert!(writer.store().ledger().input_notices().imported.is_empty());
    assert_eq!(writer.store().ledger().next_serial(), 0);
    assert_eq!(fixture.port.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn unavailable_frozen_history_never_becomes_a_notice_receipt() {
    let fixture = Fixture::new();
    fixture.message(40, "-# ⌨️ same");
    let input = evidence("p", "same", 0);
    let key = input.unit_key.clone().unwrap();
    let mut writer = fixture.writer();
    seed(
        &mut writer,
        &[input],
        MigrationHistory::Unavailable {
            reason: "history upper could not be frozen".into(),
        },
    );
    assert!(
        writer
            .reconcile_notice_migration(true, &[], std::slice::from_ref(&key), &[])
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(writer.store().ledger().input_notices().unresolved.len(), 1);
    let verdict = confirm::settle_notice(
        &*fixture.port,
        7,
        10,
        "-# ⌨️ same",
        false,
        &[],
        writer.store().ledger().input_notices(),
    )
    .await;
    assert!(matches!(verdict, confirm::Verdict::Unresolved(_)));
    assert_eq!(fixture.port.reads.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.port.posts.load(Ordering::SeqCst), 0);
}

#[tokio::test(start_paused = true)]
async fn notice_settlement_rejects_undiscovered_frozen_legacy_receipts_but_accepts_new_receipts() {
    let fixture = Fixture::new();
    let input = evidence("p", "same", 0);
    let key = input.unit_key.clone().unwrap();
    let mut writer = fixture.writer();
    seed(
        &mut writer,
        &[input],
        MigrationHistory::Fixed {
            lower: 10,
            upper: 50,
        },
    );
    assert!(
        writer
            .reconcile_notice_migration(true, &[], std::slice::from_ref(&key), &[])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        writer
            .store()
            .ledger()
            .input_notices()
            .reserved_ids
            .is_empty()
    );
    drop(writer);
    fixture.message(40, "-# ⌨️ same");
    for actual in [None, Some(60)] {
        if let Some(id) = actual {
            fixture.message(id, "-# ⌨️ same");
        }
        let mut writer = fixture.writer();
        let verdict = confirm::settle_notice(
            &*fixture.port,
            7,
            10,
            "-# ⌨️ same",
            false,
            &[],
            writer.store().ledger().input_notices(),
        )
        .await;
        match actual {
            Some(id) => assert_eq!(verdict, confirm::Verdict::Posted(id)),
            None => assert!(!matches!(verdict, confirm::Verdict::Posted(_))),
        }
        drop(writer);
    }
    assert_eq!(fixture.port.posts.load(Ordering::SeqCst), 0);
}
