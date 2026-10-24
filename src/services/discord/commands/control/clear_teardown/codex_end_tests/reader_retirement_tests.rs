//! Consumer integration for the real clear and replacement driver fixtures in the parent.

use super::*;
use std::collections::VecDeque;
use std::time::SystemTime;

use crate::services::tui_o::ownership::OwnershipGate;
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::shadow::{ShadowProvider, SourceId};
use crate::services::tui_o::store::rotation::Successor;
use crate::services::tui_o::store::spool::{Cursor, source_key};
use crate::services::tui_o::store::{InitSource, Initialized, OStore, StoreConfig};
use crate::services::tui_o::writer::binding::{
    BindingCause, BindingEvents, BindingLog, BindingRecord, BindingTarget,
};
use crate::services::tui_o::writer::deliver::ChannelWriter;
use crate::services::tui_o::writer::pieces::UnitDeriver;
use crate::services::tui_o::writer::rotation::{RETIRE_QUIET, Sources, probe};
use crate::services::tui_o::writer::{
    AlarmSink, DeliveryLease, DiscordPort, PostOutcome, ReaderConfig, SeenMessage, WriterAlarm,
};

/// These tests exercise capture and retirement without a Discord client or any delivery.
struct NoDiscord;
impl DiscordPort for NoDiscord {
    fn bot_id(&self) -> u64 {
        42
    }

    fn post(
        &self,
        _channel: u64,
        _content: String,
    ) -> impl std::future::Future<Output = PostOutcome> + Send + 'static {
        std::future::ready(PostOutcome::Refused(403))
    }

    fn history_after(
        &self,
        _channel: u64,
        _after: u64,
    ) -> impl std::future::Future<Output = Result<Vec<SeenMessage>, String>> + Send {
        std::future::ready(Ok(Vec::new()))
    }

    fn message(
        &self,
        _channel: u64,
        _id: u64,
    ) -> impl std::future::Future<Output = Result<Option<SeenMessage>, String>> + Send {
        std::future::ready(Ok(None))
    }
}

struct NoLease;
impl DeliveryLease for NoLease {
    type Held = ();
    fn try_acquire(&self, _channel: u64, _serial: u64) -> Option<Self::Held> {
        panic!("reader retirement cannot acquire a delivery lease")
    }
}

#[derive(Clone, Default)]
struct ReaderAlarms(Arc<Mutex<Option<usize>>>);
impl AlarmSink for ReaderAlarms {
    fn raise(&self, _channel: u64, alarm: WriterAlarm) {
        panic!("unexpected reader alarm: {alarm:?}")
    }

    fn reconcile_reader_count(&self, _channel: u64, count: usize) {
        *self.0.lock().unwrap() = Some(count);
    }
}

type Writer = ChannelWriter<NoDiscord, NoLease, ReaderAlarms>;

fn initial(source: SourceId, through: u64) -> InitSource {
    let bytes = std::fs::read(&source.path).unwrap();
    InitSource {
        source_id: source,
        delivery_start: through,
        prefix_hash: hex::encode(Sha256::digest(&bytes[..through as usize])),
    }
}

fn cursor(writer: &mut Writer, source: &SourceId) -> Cursor {
    writer.store().cursor(source).unwrap().clone()
}

async fn validation_finished(sources: &Sources<BindingLog>) {
    for _ in 0..1000 {
        if probe::ready(sources) {
            return;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("configured reader prefix validation did not finish");
}

/// The generic publisher has already resolved a real Pending, but the native first prompt is
/// still absent. Only Sources::follow may complete the retained producer Candidate here.
pub(super) fn idle_follow_seals_generic_resolution(
    pane: &Pane,
    context: &BindingContext,
    path: &Path,
    prompt: &str,
) {
    runtime().block_on(async {
        let root = crate::config::runtime_root().unwrap();
        let record = codex_end::read(&root, pane.channel).unwrap().remove(0);
        assert!(record.successor.is_none());
        let rows_before = p5::records_strict(pane.channel).unwrap().unwrap();
        let resolved_seq = rows_before.last().unwrap().seq;
        let p5::BindingTarget::Resolved { source: new, .. } = &rows_before.last().unwrap().new
        else {
            panic!("generic publisher must resolve the exact Pending");
        };
        let new = new.clone();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root)
            .unwrap()
            .unwrap();
        let old_len = std::fs::metadata(&record.source.path).unwrap().len();
        store
            .begin_era(&[pane.channel], chrono::Utc::now(), |channel| {
                Ok(Initialized {
                    channel,
                    sources: vec![initial(record.source.clone(), old_len)],
                    initial_anchor: 100,
                    build_digest: "actual-generic-pending-resolution".into(),
                    at: chrono::Utc::now(),
                })
            })
            .unwrap();
        let era = store.read_era().unwrap().unwrap();
        let channel = store.open_channel(&era, pane.channel).unwrap().unwrap();
        let mut writer = ChannelWriter::new(
            channel,
            Arc::new(OwnershipGate::default()),
            Arc::new(NoDiscord),
            NoLease,
            ReaderAlarms::default(),
        );
        let mut sources = Sources::new(pane.channel, ShadowProvider::Codex, Arc::new(BindingLog));
        let mut deriver = UnitDeriver::new(pane.channel, ShadowProvider::Codex);
        let mut owed = VecDeque::new();
        sources
            .resume(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        sources.follow(&mut writer).unwrap();
        assert_eq!(
            writer.store().binding_checkpoint().unwrap(),
            Some(resolved_seq)
        );
        let rotation = writer.store().rotation().unwrap();
        let old_key = source_key(&record.source);
        assert_eq!(rotation.successors[&old_key].source, new);
        assert_eq!(rotation.successors[&old_key].seq, Some(rows_before[1].seq));
        assert_eq!(rotation.successors[&old_key].proof, None);
        assert!(
            successor(pane).is_none(),
            "the missing native first prompt remains pending"
        );

        use std::io::Write;
        writeln!(
            std::fs::OpenOptions::new().append(true).open(path).unwrap(),
            "{}",
            serde_json::json!({"type":"response_item", "payload":{
                "type":"message", "role":"user", "content":prompt
            }})
        )
        .unwrap();
        // No publisher and no direct producer retry runs after this native append. The log is
        // already at its current checkpoint, so this call exercises the idle follow branch.
        sources.follow(&mut writer).unwrap();
        assert_eq!(
            p5::records_strict(pane.channel).unwrap().unwrap(),
            rows_before
        );
        assert_eq!(
            writer.store().binding_checkpoint().unwrap(),
            Some(resolved_seq)
        );
        let seal =
            successor(pane).expect("idle Sources::follow must retry the real Done Candidate");
        assert_eq!(seal["execution_nonce"], context.execution_nonce);
        assert_eq!(seal["binding_seq"], resolved_seq);
        assert_eq!(
            writer.store().rotation().unwrap().successors[&old_key].proof,
            Some(resolved_seq),
            "the idle retry's actual Seal must durably prove the Pending-to-Resolved edge"
        );
        assert!(
            !cursor(&mut writer, &record.source).retired,
            "idle proof still waits for capture and quiet"
        );
    });
}

/// Both calls below receive only records produced by actual clear/replacement plus cold_driver.
/// Each policy gets an independent OStore and an exact byte copy of that durable evidence.
pub(super) fn actual_end_retires_only_its_old_reader(pane: &Pane) {
    let record = codex_end::read_durable(&crate::config::runtime_root().unwrap(), pane.channel)
        .unwrap()
        .remove(0);
    let seal = record
        .successor
        .as_ref()
        .expect("actual fresh launch sealed");
    let events = BindingLog.binding_events_since(pane.channel, 0).unwrap();
    let event = events
        .iter()
        .find(|event| event.seq == seal.binding_seq)
        .unwrap();
    assert_eq!(event.execution_nonce, seal.execution_nonce);
    assert!(
        matches!(&event.record, BindingRecord::Bound {
        old: Some(old), new: BindingTarget::Source(new), cause: BindingCause::Startup, ..
    } if old == &record.source && new == &seal.source),
        "real fresh launch must make the exact Startup edge"
    );

    for configured in [false, true] {
        runtime().block_on(async {
            tokio::time::pause();
            let root = tempfile::tempdir().unwrap();
            let proof_dir = root.path().join(codex_end::DIR_NAME);
            std::fs::create_dir(&proof_dir).unwrap();
            let proof_name = format!("{}.json", pane.channel);
            std::fs::copy(
                crate::config::runtime_root().unwrap().join(codex_end::DIR_NAME).join(&proof_name),
                proof_dir.join(proof_name),
            ).unwrap();

            // An earlier unproven reader has retained output. The new execution proves only
            // its own predecessor, even though both readers name the same captured successor.
            let backlog_path = root.path().join("existing-unproven-backlog.jsonl");
            let backlog_bytes = format!("{}\n", serde_json::json!({
                "type":"response_item", "payload":{"type":"message", "role":"assistant",
                "id":"retained-backlog", "content":[{"type":"output_text", "text":"retained backlog"}]}
            }));
            std::fs::write(&backlog_path, &backlog_bytes).unwrap();
            let backlog = source_id_for("existing-unproven", &backlog_path).unwrap();
            let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path()).unwrap().unwrap();
            let sources = vec![
                initial(record.source.clone(), std::fs::metadata(&record.source.path).unwrap().len()),
                initial(backlog.clone(), 0),
            ];
            store.begin_era(&[pane.channel], chrono::Utc::now(), |channel| Ok(Initialized {
                channel, sources: sources.clone(), initial_anchor: 100,
                build_digest: "actual-codex-end-retirement".into(), at: chrono::Utc::now(),
            })).unwrap();
            let era = store.read_era().unwrap().unwrap();
            let mut channel = store.open_channel(&era, pane.channel).unwrap().unwrap();
            let unproven = Successor {
                source: seal.source.clone(), seq: Some(seal.binding_seq),
                tmux_session: Some("previous-unproven-pane".into()),
                drain_to: Some(backlog_bytes.len() as u64), proof: None,
            };
            let mut rotation = channel.rotation().unwrap();
            rotation.successors.insert(source_key(&backlog), unproven.clone());
            channel.write_rotation(&rotation).unwrap();
            let alarms = ReaderAlarms::default();
            let mut writer = ChannelWriter::new(channel, Arc::new(OwnershipGate::default()), Arc::new(NoDiscord), NoLease, alarms.clone());
            let mut sources = Sources::new(pane.channel, ShadowProvider::Codex, Arc::new(BindingLog));
            if configured {
                sources = sources.with_readers(ReaderConfig { dormant: false });
            }
            let mut deriver = UnitDeriver::new(pane.channel, ShadowProvider::Codex);
            let mut owed = VecDeque::new();
            let t0 = SystemTime::now();
            sources.resume_at(&mut writer, &mut deriver, &mut owed, t0).unwrap();
            sources.follow(&mut writer).unwrap();

            // Read the file back through the store before capture/tend: in-memory proof alone
            // must never satisfy the integration oracle or allow retirement.
            let rotation = writer.store().rotation().unwrap();
            assert_eq!(rotation.successors[&source_key(&record.source)].proof,
                Some(seal.binding_seq), "actual {:?} must durably prove its Startup hop (configured={configured})", record.basis);
            assert_eq!(rotation.successors[&source_key(&backlog)], unproven);
            assert!(!cursor(&mut writer, &record.source).retired, "proof alone cannot retire");
            assert!(probe::reading(&sources, &record.source));
            assert_eq!(probe::jobs_started(&sources), 0, "proof precedes asynchronous prefix validation");

            sources.capture(&mut writer, &mut deriver, &mut owed).unwrap();
            assert_eq!(cursor(&mut writer, &seal.source).captured_through,
                std::fs::metadata(&seal.source.path).unwrap().len(), "the actual fresh transcript must be captured");
            assert_eq!(cursor(&mut writer, &backlog).captured_through, backlog_bytes.len() as u64);
            let retained = cursor(&mut writer, &backlog);
            assert!(!retained.retired && retained.transition_cleanup.is_none());

            sources.tend_at(&mut writer, t0).unwrap();
            if configured {
                assert!(probe::validating(&sources));
                validation_finished(&sources).await;
                assert!(!cursor(&mut writer, &record.source).retired, "finished validation alone cannot retire");
                sources.tend_at(&mut writer, t0).unwrap();
            }
            assert!(!cursor(&mut writer, &record.source).retired, "the quiet interval is still required");
            tokio::time::advance(RETIRE_QUIET - Duration::from_millis(1)).await;
            let before_quiet = t0 + RETIRE_QUIET - Duration::from_millis(1);
            sources.tend_at(&mut writer, before_quiet).unwrap();
            assert!(!cursor(&mut writer, &record.source).retired, "one millisecond before the quiet boundary remains read");

            tokio::time::advance(Duration::from_millis(1)).await;
            let quiet = t0 + RETIRE_QUIET;
            sources.tend_at(&mut writer, quiet).unwrap();
            if configured {
                // Validation jobs can also serve the existing unproven backlog. Consume those
                // without allowing a ready result itself to retire the proven reader.
                for _ in 0..6 {
                    if cursor(&mut writer, &record.source).retired {
                        break;
                    }
                    assert!(probe::validating(&sources), "quiet proven reader must await prefix validation");
                    validation_finished(&sources).await;
                    assert!(!cursor(&mut writer, &record.source).retired, "only tend may consume validated evidence");
                    sources.tend_at(&mut writer, quiet).unwrap();
                }
            }
            assert!(cursor(&mut writer, &record.source).retired, "the proven old source retires after all gates (configured={configured})");
            assert!(!probe::reading(&sources, &record.source));
            assert!(probe::reading(&sources, &seal.source));
            assert_eq!(*alarms.0.lock().unwrap(), Some(2));
            assert_eq!(cursor(&mut writer, &backlog), retained, "unproven retained backlog keeps its cursor and cleanup history");
            assert_eq!(writer.store().rotation().unwrap().successors[&source_key(&backlog)], unproven);
            assert_eq!(std::fs::read_to_string(backlog_path).unwrap(), backlog_bytes);
        });
    }
}
