use super::*;
use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::store::ledger::{
    LedgerEntry,
    input_notices::{MigrationHistory, MigrationSnapshot, MigrationSource, NoticeEntry},
};
use crate::services::tui_o::store::{InitSource, Initialized, OStore, StoreConfig};
use crate::services::tui_o::writer::{
    PostOutcome, SeenMessage,
    binding::ChannelBindingLog,
    input_notices::{Projection, admission::tests::bind},
};
use sha2::{Digest, Sha256};
use std::{future::Future, io::Write, sync::Mutex};
const CHANNEL: u64 = 7332099;
#[derive(Default)]
struct Port {
    unreadable: std::sync::atomic::AtomicBool,
    messages: Mutex<Vec<SeenMessage>>,
    posts: Mutex<Vec<String>>,
}
impl DiscordPort for Port {
    fn bot_id(&self) -> u64 {
        42
    }
    fn post(&self, _: u64, content: String) -> impl Future<Output = PostOutcome> + Send + 'static {
        self.posts.lock().unwrap().push(content.clone());
        let mut h = self.messages.lock().unwrap();
        let m = SeenMessage {
            id: 1000 + h.len() as u64,
            author_id: 42,
            content,
        };
        h.push(m.clone());
        let uncertain = self.unreadable.load(std::sync::atomic::Ordering::SeqCst);
        async move {
            if uncertain {
                PostOutcome::Uncertain("response lost".into())
            } else {
                PostOutcome::Created(m)
            }
        }
    }
    async fn history_after(&self, _: u64, after: u64) -> Result<Vec<SeenMessage>, String> {
        if self.unreadable.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("HTTP 403".into());
        }
        Ok(self
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.id > after)
            .cloned()
            .collect())
    }
    async fn message(&self, _: u64, id: u64) -> Result<Option<SeenMessage>, String> {
        if self.unreadable.load(std::sync::atomic::Ordering::SeqCst) {
            return Err("HTTP 403".into());
        }
        Ok(self
            .messages
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.id == id)
            .cloned())
    }
}
thread_local! {
    static VISITS: std::cell::Cell<(usize, Option<usize>)> = const { std::cell::Cell::new((0, None)) };
}
struct VisitReset;
impl Drop for VisitReset {
    fn drop(&mut self) {
        VISITS.with(|visits| visits.set((0, None)));
    }
}
struct Lease;
impl DeliveryLease for Lease {
    type Held = ();
    fn try_acquire(&self, _: u64, _: u64) -> Option<()> {
        VISITS.with(|visits| {
            let (count, limit) = visits.get();
            visits.set((count + 1, limit));
            assert!(
                limit.is_none_or(|max| count < max),
                "the pass revisited held notice work beyond its initial length"
            );
        });
        Some(())
    }
}
#[derive(Clone, Default)]
struct Alarms(Arc<Mutex<Vec<WriterAlarm>>>);
impl AlarmSink for Alarms {
    fn raise(&self, _: u64, a: WriterAlarm) {
        self.0.lock().unwrap().push(a)
    }
}
type Host = Actor<Port, Lease, Alarms, ChannelBindingLog>;
fn row(id: &str, prompt: &str) -> Vec<u8> {
    let mut b = serde_json::to_vec(
        &serde_json::json!({"type":"user","uuid":id,"message":{"role":"user","content":prompt}}),
    )
    .unwrap();
    b.push(b'\n');
    b
}
fn body(id: &str, text: &str) -> Vec<u8> {
    let mut b=serde_json::to_vec(&serde_json::json!({"type":"assistant","uuid":id,"apiBlockIndex":0,"message":{"id":id,"content":[{"type":"text","text":text}]}})).unwrap();
    b.push(b'\n');
    b
}
fn append(path: &std::path::Path, b: &[u8]) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(b)
        .unwrap();
}
fn host(o: &OStore, port: Arc<Port>, alarms: Alarms, cap: u64) -> Host {
    let mut store = o
        .open_channel(&o.read_era().unwrap().unwrap(), CHANNEL)
        .unwrap()
        .unwrap();
    store.set_limits_for_test(1, cap);
    let gate = Arc::new(OwnershipGate::default());
    gate.acquired();
    let writer = ChannelWriter::new(store, gate, port, Lease, alarms);
    let bindings = Arc::new(ChannelBindingLog::new(CHANNEL, ShadowProvider::Claude));
    Actor {
        replay_deferred: false,
        writer,
        deriver: UnitDeriver::new(CHANNEL, ShadowProvider::Claude)
            .with_input_notices(Projection::for_tests()),
        owed: VecDeque::new(),
        verified: None,
        sources: Sources::new(CHANNEL, ShadowProvider::Claude, bindings.clone()),
        bindings,
        transitions: transition::Transitions::new(None, watch::channel(false).1),
        notice: None,
        waiting: Waiting::default(),
        clock: (tokio::time::Instant::now(), chrono::Utc::now()),
        scope_hold: None,
        boot_previous_nonce: None,
    }
}
fn resume(actor: &mut Host) {
    actor
        .sources
        .resume(&mut actor.writer, &mut actor.deriver, &mut actor.owed)
        .unwrap();
    actor.persist_notice_evidence();
}
#[tokio::test(start_paused = true)]
async fn same_source_over_cap_reclaims_raw_and_keeps_debt_after_actor_recreation() {
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let path = runtime.path().join("native.jsonl");
    let prompt = "legacy".repeat(100);
    let old = [row("p", &prompt), row("q", &prompt)].concat();
    std::fs::write(&path, &old).unwrap();
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for("native", &path).unwrap();
    let _binding = bind(
        runtime.path(),
        CHANNEL,
        &source,
        "notice-cap-pane",
        "73320000000000000000000000000099",
    );
    let o = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
        .unwrap()
        .unwrap();
    o.begin_era(&[CHANNEL], chrono::Utc::now(), |channel| {
        Ok(Initialized {
            channel,
            sources: vec![InitSource {
                source_id: source.clone(),
                delivery_start: 0,
                prefix_hash: hex::encode(Sha256::digest(b"")),
            }],
            initial_anchor: 10,
            build_digest: "fixture".into(),
            at: chrono::Utc::now(),
        })
    })
    .unwrap();
    let port = Arc::new(Port::default());
    port.messages.lock().unwrap().push(SeenMessage {
        id: 11,
        author_id: 42,
        content: crate::services::discord::input_notice_content(&prompt).unwrap(),
    });
    let alarms = Alarms::default();
    let mut actor = host(&o, port.clone(), alarms.clone(), u64::MAX);
    actor
        .writer
        .record(LedgerEntry::InputNotice {
            entry: Box::new(NoticeEntry::MigrationStarted {
                snapshot: MigrationSnapshot {
                    sources: vec![MigrationSource {
                        source: source.clone(),
                        lower: 0,
                        upper: old.len() as u64,
                        prefix_hash: hex::encode(Sha256::digest(&old)),
                    }],
                    binding_checkpoint: 1,
                    history: MigrationHistory::Fixed {
                        lower: 10,
                        upper: 11,
                    },
                },
            }),
        })
        .unwrap();
    resume(&mut actor);
    let fresh = actor.read_sources();
    actor.prepare_notices(&fresh).await;
    assert_eq!(
        actor
            .writer
            .store()
            .ledger()
            .input_notices()
            .unresolved
            .len(),
        2,
        "ambiguous inputs must become durable migration debt"
    );
    let cap = actor.writer.store().spool_bytes();
    assert!(cap > 0);
    actor.writer.store().set_limits_for_test(1, cap);
    let progress = crate::services::tui_o::writer::progress::Publisher::new(CHANNEL);
    for cycle in 0..3 {
        let bytes = [
            row(&format!("new-{cycle}"), &"new terminal".repeat(45)),
            body(&format!("body-{cycle}"), &"new answer".repeat(50)),
        ]
        .concat();
        assert!((bytes.len() as u64) < cap);
        append(&path, &bytes);
        let before = actor
            .writer
            .store()
            .cursor(&source)
            .unwrap()
            .captured_through;
        actor.read_sources();
        assert_eq!(
            actor
                .writer
                .store()
                .cursor(&source)
                .unwrap()
                .captured_through,
            before,
            "small cap must really hold the cumulative raw batch"
        );
        actor.collect_settled();
        let fresh = actor.read_sources();
        actor.prepare_notices(&fresh).await;
        actor.deliver_owed(&progress).await;
        actor.collect_settled();
        assert!(
            !actor.writer.is_stopped(),
            "later output must progress while legacy debt stays"
        );
        assert_eq!(
            actor
                .writer
                .store()
                .cursor(&source)
                .unwrap()
                .captured_through,
            std::fs::metadata(&path).unwrap().len()
        );
        assert_eq!(actor.notice_debt_extra(), 2);
        assert_eq!(actor.progress().debt, 2);
        assert_eq!(actor.undelivered(9).unwrap().owed, 2);
        assert_eq!(actor.obligations().unwrap().owed, 2);
        assert_eq!(
            actor
                .writer
                .store()
                .ledger()
                .input_notices()
                .unresolved
                .len(),
            2
        );
        assert!(actor.writer.store().spool_bytes() <= cap);
        let ledger = actor.writer.store().ledger();
        assert!(ledger.gc_segments(&source).len() > cycle);
        let raw: Vec<_> = ledger
            .input_notices()
            .evidence
            .values()
            .filter(|e| {
                e.unit_key
                    .as_ref()
                    .is_some_and(|k| k.native_key == "p" || k.native_key == "q")
            })
            .map(|e| e.raw_record.clone())
            .collect();
        assert_eq!(raw.len(), 2);
        let mut old_raw = 0;
        actor
            .writer
            .store()
            .for_each_frame(&source, |frame, _| {
                if frame.start() < old.len() as u64 {
                    old_raw += 1
                }
            })
            .unwrap();
        assert_eq!(old_raw, 0, "raw segments must actually disappear");
        drop(actor);
        actor = host(&o, port.clone(), alarms.clone(), cap);
        resume(&mut actor);
        actor.prepare_notices(&[]).await;
        assert_eq!(
            actor.notice_debt_extra(),
            2,
            "recreation must retain ledger debt with the same small cap"
        );
    }
    assert!(std::fs::metadata(&path).unwrap().len() > cap);
    let posted = port.posts.lock().unwrap();
    assert_eq!(posted.len(), 6);
    assert_eq!(
        posted
            .iter()
            .filter(|p| p.as_str() == "new answer".repeat(50))
            .count(),
        3
    );
    assert_eq!(posted.iter().filter(|p| p.starts_with("-# ⌨️")).count(), 3);
    assert!(
        alarms
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|a| matches!(a, WriterAlarm::SpoolFull))
    );
}

#[tokio::test(start_paused = true)]
async fn a_withheld_notice_keeps_debt_without_stalling_or_falsely_draining_body() {
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let _visits = VisitReset;
    let runtime = tempfile::tempdir().unwrap();
    let o = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
        .unwrap()
        .unwrap();
    o.begin_era(&[CHANNEL], chrono::Utc::now(), |channel| {
        Ok(Initialized {
            channel,
            sources: Vec::new(),
            initial_anchor: 10,
            build_digest: "fixture".into(),
            at: chrono::Utc::now(),
        })
    })
    .unwrap();
    let port = Arc::new(Port::default());
    let mut actor = host(&o, port.clone(), Alarms::default(), u64::MAX);
    let key = |id: &str, kind| UnitKey {
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        native_key: id.into(),
        kind,
    };
    actor
        .owed
        .push_back(Derived::Piece(super::super::super::pieces::PieceWork {
            unit_key: key(
                "held",
                crate::services::tui_o::shadow::UnitKind::InputNotice,
            ),
            index: 0,
            payload: "-# ⌨️ held".into(),
            notice_origin: None,
        }));
    let progress = crate::services::tui_o::writer::progress::Publisher::new(CHANNEL);
    for pass in 0..2 {
        actor
            .owed
            .push_back(Derived::Piece(super::super::super::pieces::PieceWork {
                unit_key: key(
                    &format!("body-{pass}"),
                    crate::services::tui_o::shadow::UnitKind::Body,
                ),
                index: 0,
                payload: format!("answer-{pass}"),
                notice_origin: None,
            }));
        VISITS.with(|visits| visits.set((0, Some(2))));
        actor.deliver_owed(&progress).await;
        assert_eq!(
            VISITS.with(|visits| visits.get().0),
            2,
            "each of the two initial items is visited once"
        );
        assert_eq!(
            actor.owed.len(),
            1,
            "the held input stays owed while the Body finishes"
        );
        assert_eq!(
            actor.writer.boot.done(),
            pass + 1,
            "Withheld is never a notice completion"
        );
        assert_eq!(actor.progress().debt, 1);
        assert_eq!(actor.writer.store().ledger().next_serial(), pass + 1);
    }
    assert_eq!(*port.posts.lock().unwrap(), ["answer-0", "answer-1"]);
    assert!(
        !actor.notice_raw_independent(),
        "missing materialization must still protect raw"
    );
}

#[tokio::test(start_paused = true)]
async fn queued_unresolved_notice_releases_its_raw_and_survives_recreation_beside_later_output() {
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let runtime = tempfile::tempdir().unwrap();
    let path = runtime.path().join("native.jsonl");
    let original = row("held", &"held request".repeat(100));
    std::fs::write(&path, &original).unwrap();
    let source =
        crate::services::tui_o::shadow::binding_reader::source_id_for("native", &path).unwrap();
    let _binding = bind(
        runtime.path(),
        CHANNEL,
        &source,
        "queued-notice-cap",
        "73320000000000000000000000000098",
    );
    let o = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
        .unwrap()
        .unwrap();
    o.begin_era(&[CHANNEL], chrono::Utc::now(), |channel| {
        Ok(Initialized {
            channel,
            sources: vec![InitSource {
                source_id: source.clone(),
                delivery_start: 0,
                prefix_hash: hex::encode(Sha256::digest(b"")),
            }],
            initial_anchor: 10,
            build_digest: "fixture".into(),
            at: chrono::Utc::now(),
        })
    })
    .unwrap();
    let port = Arc::new(Port::default());
    let mut actor = host(&o, port.clone(), Alarms::default(), u64::MAX);
    actor
        .writer
        .record(LedgerEntry::InputNotice {
            entry: Box::new(NoticeEntry::MigrationStarted {
                snapshot: MigrationSnapshot {
                    sources: Vec::new(),
                    binding_checkpoint: 1,
                    history: MigrationHistory::Fixed {
                        lower: 10,
                        upper: 10,
                    },
                },
            }),
        })
        .unwrap();
    resume(&mut actor);
    let fresh = actor.read_sources();
    actor.prepare_notices(&fresh).await;
    port.unreadable
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let progress = crate::services::tui_o::writer::progress::Publisher::new(CHANNEL);
    actor.deliver_owed(&progress).await;
    assert_eq!(actor.owed.len(), 1);
    assert_eq!(actor.writer.boot.done(), 0);
    assert!(matches!(
        actor.writer.store().ledger().piece(0).unwrap().outcome,
        Some(crate::services::tui_o::store::ledger::PieceOutcome::Unresolved(_))
    ));
    let cap = actor.writer.store().spool_bytes();
    actor.writer.store().set_limits_for_test(1, cap);
    port.unreadable
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let next = [
        row("later", &"new input".repeat(60)),
        body("later-body", &"new answer".repeat(50)),
    ]
    .concat();
    assert!((next.len() as u64) < cap);
    append(&path, &next);
    actor.read_sources();
    assert_eq!(
        actor
            .writer
            .store()
            .cursor(&source)
            .unwrap()
            .captured_through,
        original.len() as u64
    );
    assert!(actor.notice_raw_independent());
    actor.collect_settled();
    assert_eq!(
        actor.writer.store().retained_segments(&source),
        0,
        "an unacked but materialized notice must permit actual raw collection"
    );
    let fresh = actor.read_sources();
    actor.prepare_notices(&fresh).await;
    actor.deliver_owed(&progress).await;
    assert_eq!(
        actor
            .writer
            .store()
            .cursor(&source)
            .unwrap()
            .captured_through,
        std::fs::metadata(&path).unwrap().len()
    );
    assert_eq!(actor.owed.len(), 1);
    assert_eq!(actor.progress().debt, 1);
    assert_eq!(actor.writer.boot.done(), 2);
    assert_eq!(port.posts.lock().unwrap().len(), 3);
    drop(actor);
    actor = host(&o, port.clone(), Alarms::default(), cap);
    resume(&mut actor);
    actor.prepare_notices(&[]).await;
    actor.deliver_owed(&progress).await;
    assert_eq!(actor.progress().debt, 1);
    assert_eq!(actor.obligations().unwrap().owed, 1);
    assert_eq!(
        port.posts.lock().unwrap().len(),
        3,
        "reopening never republishes the uncertain input or old Body"
    );
    assert_eq!(actor.writer.store().ledger().next_serial(), 3);
}

#[tokio::test(start_paused = true)]
async fn refilter_after_notice_settlement_ends_the_pass_before_any_later_body_start() {
    let _guard = crate::config::TestRuntimeRootGuard::new();
    let _visits = VisitReset;
    let runtime = tempfile::tempdir().unwrap();
    let o = OStore::open_if_enabled(&StoreConfig { enabled: true }, runtime.path())
        .unwrap()
        .unwrap();
    o.begin_era(&[CHANNEL], chrono::Utc::now(), |channel| {
        Ok(Initialized {
            channel,
            sources: Vec::new(),
            initial_anchor: 10,
            build_digest: "fixture".into(),
            at: chrono::Utc::now(),
        })
    })
    .unwrap();
    let port = Arc::new(Port::default());
    let mut actor = host(&o, port.clone(), Alarms::default(), u64::MAX);
    let key = UnitKey {
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        native_key: "open-notice".into(),
        kind: crate::services::tui_o::shadow::UnitKind::InputNotice,
    };
    actor
        .writer
        .record(LedgerEntry::Prepared {
            serial: 0,
            unit_key: key.clone(),
            piece_index: 0,
            payload: "-# ⌨️ open".into(),
            anchor_id: 10,
            epoch: 1,
            local_retry: None,
            notice_origin: None,
        })
        .unwrap();
    actor
        .owed
        .push_back(Derived::Piece(super::super::super::pieces::PieceWork {
            unit_key: key,
            index: 0,
            payload: "-# ⌨️ open".into(),
            notice_origin: None,
        }));
    actor
        .owed
        .push_back(Derived::Piece(super::super::super::pieces::PieceWork {
            unit_key: UnitKey {
                channel_id: CHANNEL,
                provider: ShadowProvider::Claude,
                native_key: "later-body".into(),
                kind: crate::services::tui_o::shadow::UnitKind::Body,
            },
            index: 0,
            payload: "later answer".into(),
            notice_origin: None,
        }));
    actor.replay_deferred = true;
    let progress = crate::services::tui_o::writer::progress::Publisher::new(CHANNEL);
    VISITS.with(|visits| visits.set((0, Some(0))));
    actor.deliver_owed(&progress).await;
    assert!(!actor.replay_deferred);
    assert_eq!(actor.owed.len(), 2);
    assert_eq!(actor.writer.boot.done(), 0);
    assert!(port.posts.lock().unwrap().is_empty());
    assert!(matches!(
        actor.writer.store().ledger().piece(0).unwrap().outcome,
        Some(crate::services::tui_o::store::ledger::PieceOutcome::Unresolved(_))
    ));
    VISITS.with(|visits| visits.set((0, Some(1))));
    actor.deliver_owed(&progress).await;
    assert_eq!(actor.owed.len(), 1);
    assert_eq!(*port.posts.lock().unwrap(), ["later answer"]);
    assert_eq!(actor.writer.boot.done(), 1);
    assert_eq!(actor.progress().debt, 1);
}
