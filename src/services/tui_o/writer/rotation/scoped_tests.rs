use super::*;
use std::io::Write;

struct EmptyLog;

impl BindingEvents for EmptyLog {
    fn binding_events_since(&self, _: u64, _: u64) -> Result<Vec<BindingEvent>, String> {
        Ok(Vec::new())
    }

    fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
        watch::channel(0).1
    }
}

fn capture(path: &std::path::Path) -> (SourceId, SourceCapture) {
    let (dev, ino) =
        crate::services::tui_o::shadow::capture::file_identity(&std::fs::metadata(path).unwrap());
    let source = SourceId {
        session_id: "scoped-partial".into(),
        path: path.into(),
        dev,
        ino,
    };
    let capture = SourceCapture::open(source.clone(), 0).unwrap();
    (source, capture)
}

#[test]
fn rescope_waits_for_actual_partial_native_record_to_finish() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("partial.jsonl");
    std::fs::write(&path, b"{\"row\":1}").unwrap();
    let (source, mut capture) = capture(&path);
    assert!(matches!(
        capture.poll(MAX_READ_BYTES),
        CaptureOutcome::Batch(_)
    ));
    assert!(capture.read_through() > capture.captured_through());
    let mut sources = Sources::new(17, ShadowProvider::Codex, Arc::new(EmptyLog));
    sources.readers.push(Reader::new(source, Some(capture)));
    assert!(
        !sources.quiescent_for_rescope(),
        "an uncommitted partial native row must hold rescope"
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    let capture = sources.readers[0].capture.as_mut().unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("completed native record must be captured");
    };
    assert_eq!(batch.records.len(), 1);
    assert_eq!(capture.read_through(), capture.captured_through());
    assert!(sources.quiescent_for_rescope());
}

#[test]
fn rescope_reset_keeps_reader_policy_binding_port_and_suspension() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sealed.jsonl");
    std::fs::write(&path, b"{\"row\":1}\n").unwrap();
    let (source, mut capture) = capture(&path);
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("the complete native row must produce a real pending batch")
    };
    let offsets = (
        capture.read_through(),
        capture.captured_through(),
        capture.prefix_hash(),
    );
    let bindings = Arc::new(EmptyLog);
    let suspension = super::super::super::suspension::Suspension::default();
    suspension.extend("new-incarnation".into(), [source.clone()]);
    let mut sources = Sources::new(19, ShadowProvider::Codex, Arc::clone(&bindings))
        .with_suspension(suspension.clone())
        .with_readers(ReaderConfig { dormant: true });
    sources
        .readers
        .push(Reader::new(source.clone(), Some(capture)));
    assert!(sources.quiescent_for_rescope());
    let held = |sources: &Sources<EmptyLog>, axis: &str| {
        assert!(
            !sources.quiescent_for_rescope(),
            "{axis} must forbid reader reset"
        );
        assert_eq!(
            sources.readers.len(),
            1,
            "{axis} dropped the retained reader"
        );
        let reader = &sources.readers[0];
        assert_eq!(reader.source, source, "{axis} changed the source identity");
        let capture = reader
            .capture
            .as_ref()
            .expect("the retained FD must stay open");
        assert_eq!(
            (
                capture.read_through(),
                capture.captured_through(),
                capture.prefix_hash()
            ),
            offsets,
            "{axis} changed native offsets/prefix"
        );
        assert!(sources.suspension.contains(&source));
        assert_eq!(
            sources.suspension.nonce().as_deref(),
            Some("new-incarnation")
        );
    };
    sources.readers[0].pending = Some(batch);
    held(&sources, "unspooled capture batch");
    assert!(sources.readers[0].pending.take().is_some());
    assert!(sources.quiescent_for_rescope());

    sources.readers[0].drain_to = Some(offsets.1 + 1);
    held(&sources, "successor drain obligation");
    sources.readers[0].drain_to = None;
    assert!(sources.quiescent_for_rescope());

    let (_worker, result) = mpsc::channel();
    sources.validation = Some(Validation {
        cursor: Cursor {
            source: source.clone(),
            captured_through: offsets.1,
            prefix_hash: offsets.2.clone(),
            retired: false,
            transition_cleanup: None,
        },
        fingerprint: Fingerprint::path(&source).unwrap(),
        observed_at: SystemTime::now(),
        result,
        ready: Arc::default(),
    });
    held(&sources, "pending descriptor validation");
    sources.validation = None;
    assert!(sources.quiescent_for_rescope());

    sources.refined.push(source.clone());
    held(&sources, "retained refinement work");
    sources.refined.clear();
    assert!(sources.quiescent_for_rescope());
    sources.dirty = true;
    held(&sources, "unflushed source boundary");
    sources.dirty = false;
    assert!(sources.quiescent_for_rescope());

    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{\"row\":2}")
        .unwrap();
    let capture = sources.readers[0].capture.as_mut().unwrap();
    assert!(matches!(
        capture.poll(MAX_READ_BYTES),
        CaptureOutcome::Batch(_)
    ));
    let partial_offsets = (capture.read_through(), capture.captured_through());
    assert!(partial_offsets.0 > partial_offsets.1);
    assert!(
        !sources.quiescent_for_rescope(),
        "partial bytes must forbid reset"
    );
    assert_eq!(sources.readers.len(), 1);
    let capture = sources.readers[0].capture.as_ref().unwrap();
    assert_eq!(
        (capture.read_through(), capture.captured_through()),
        partial_offsets
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"\n")
        .unwrap();
    let capture = sources.readers[0].capture.as_mut().unwrap();
    let CaptureOutcome::Batch(completed) = capture.poll(MAX_READ_BYTES) else {
        panic!("the incomplete row must become one complete capture")
    };
    assert_eq!(completed.records.len(), 1);
    assert!(sources.quiescent_for_rescope());
    // Only the positive, fully quiescent caller may drop and rebuild these readers.
    sources.reset_for_rescope();
    assert!(
        sources.readers.is_empty(),
        "reset must drop the prior source readers"
    );
    assert_eq!(sources.reader_config, Some(ReaderConfig { dormant: true }));
    assert!(Arc::ptr_eq(&sources.bindings, &bindings));
    assert!(sources.suspension.contains(&source));
    assert_eq!(
        sources.suspension.nonce().as_deref(),
        Some("new-incarnation")
    );
    assert_eq!(
        (sources.channel, sources.provider),
        (19, ShadowProvider::Codex)
    );
}

#[cfg(unix)]
mod contracts {
    use super::*;
    use crate::services::tui_o::ownership::OwnershipGate;
    use crate::services::tui_o::store::rotation::{CHECKPOINT_FILE, HotCandidate};
    use crate::services::tui_o::store::{Initialized, OStore, StoreConfig};
    use crate::services::tui_o::writer::{PostOutcome, SeenMessage};

    struct NoIo;
    impl DiscordPort for NoIo {
        fn bot_id(&self) -> u64 {
            1
        }
        fn post(
            &self,
            _: u64,
            _: String,
        ) -> impl std::future::Future<Output = PostOutcome> + Send + 'static {
            async { panic!("no POST before source rejection") }
        }
        async fn history_after(&self, _: u64, _: u64) -> Result<Vec<SeenMessage>, String> {
            panic!("no history before source rejection")
        }
        async fn message(&self, _: u64, _: u64) -> Result<Option<SeenMessage>, String> {
            panic!("no message read before source rejection")
        }
    }
    impl DeliveryLease for NoIo {
        type Held = ();
        fn try_acquire(&self, _: u64, _: u64) -> Option<()> {
            panic!("no lease before source rejection")
        }
    }
    impl AlarmSink for NoIo {
        fn raise(&self, _: u64, _: WriterAlarm) {
            panic!("source rejection must not stop or mutate the writer")
        }
    }

    struct Fixture {
        root: tempfile::TempDir,
        store: ChannelStore,
        old: SourceId,
    }
    impl Fixture {
        fn new(checkpoint: bool) -> Self {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("old.jsonl");
            std::fs::write(&path, b"old source must stay unread\n").unwrap();
            let (old, _) = capture(&path);
            let disk = OStore::open_if_enabled(&StoreConfig { enabled: true }, root.path())
                .unwrap()
                .unwrap();
            let era = disk
                .begin_era(&[29], Utc::now(), |channel| {
                    Ok(Initialized {
                        channel,
                        sources: Vec::new(),
                        initial_anchor: 0,
                        build_digest: "scoped-contract".into(),
                        at: Utc::now(),
                    })
                })
                .unwrap();
            let mut store = disk.open_channel(&era, 29).unwrap().unwrap();
            store.attach_source(&old).unwrap();
            if checkpoint {
                store.set_binding_checkpoint(1).unwrap();
            }
            Self { root, store, old }
        }
        fn checkpoint(&self) -> std::path::PathBuf {
            self.root.path().join("o_store/29").join(CHECKPOINT_FILE)
        }
        fn sources(&self) -> Sources<EmptyLog> {
            let suspension = super::super::super::super::suspension::Suspension::default();
            suspension.extend("current-nonce".into(), [self.old.clone()]);
            Sources::new(29, ShadowProvider::Codex, Arc::new(EmptyLog)).with_suspension(suspension)
        }
        fn new_source(&self) -> SourceId {
            SourceId {
                session_id: "current".into(),
                path: self.root.path().join("never-open-new.jsonl"),
                dev: self.old.dev,
                ino: self.old.ino + 1,
            }
        }
    }

    #[test]
    fn suspended_resume_never_opens_an_old_native_reader() {
        let fixture = Fixture::new(true);
        let mut sources = fixture.sources();
        let old = fixture.old.clone();
        let before = fixture.store.cursor(&old).unwrap().clone();
        let io = crate::services::codex::actual_submit_tests::source_io::Watch::start(&old.path);
        let mut writer = ChannelWriter::new(
            fixture.store,
            Arc::new(OwnershipGate::default()),
            Arc::new(NoIo),
            NoIo,
            NoIo,
        );
        let mut deriver = UnitDeriver::new(29, ShadowProvider::Codex);
        let mut owed = std::collections::VecDeque::new();
        sources
            .resume(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        assert!(
            sources.readers.is_empty(),
            "a suspended source acquired a reader"
        );
        assert!(owed.is_empty());
        assert_eq!(writer.store().cursor(&old), Some(&before));
        assert_eq!(io.stats(), Default::default());
    }

    #[test]
    fn suspended_old_parent_and_new_bind_references_fail_before_file_open_or_rotation() {
        for role in ["old", "parent", "new"] {
            let fixture = Fixture::new(true);
            let mut sources = fixture.sources();
            let new = fixture.new_source();
            let old = (role == "old").then_some(&fixture.old);
            let parent = (role == "parent").then_some(&fixture.old);
            let selected = if role == "new" {
                fixture.old.clone()
            } else {
                new
            };
            let event = BindingEvent {
                seq: 2,
                channel_id: 29,
                provider: ShadowProvider::Codex,
                tmux_session: "scoped-unit-no-live-target".into(),
                execution_nonce: "current-nonce".into(),
                record: BindingRecord::Bound {
                    old: old.cloned(),
                    new: BindingTarget::Source(selected.clone()),
                    cause: BindingCause::Startup,
                    parent_hint: parent.cloned(),
                    evidence: crate::services::tui_o::writer::binding::BindingEvidence {
                        hook_event: "session_start".into(),
                        received_at: Utc::now(),
                        reclaims: false,
                    },
                },
                committed_at: Utc::now(),
            };
            let before = fixture.store.rotation().unwrap();
            let old_cursor = fixture.store.cursor(&fixture.old).unwrap().clone();
            std::fs::remove_file(&fixture.old.path).unwrap();
            let mut writer = ChannelWriter::new(
                fixture.store,
                Arc::new(OwnershipGate::default()),
                Arc::new(NoIo),
                NoIo,
                NoIo,
            );
            let error = sources
                .bind(
                    &mut writer,
                    &event,
                    old,
                    selected,
                    BindingCause::Startup,
                    parent,
                )
                .unwrap_err();
            assert!(
                matches!(error, WriterAlarm::Halted { ref detail } if detail.contains("suspended source")),
                "{role}: {error:?}"
            );
            assert_eq!(
                writer.store().rotation().unwrap(),
                before,
                "{role} must preserve rotation"
            );
            assert_eq!(writer.store().cursor(&fixture.old).unwrap(), &old_cursor);
            assert!(sources.readers.is_empty());
        }
    }

    #[test]
    fn suspended_incoming_dev_renumber_still_resolves_to_the_stored_denial() {
        let fixture = Fixture::new(true);
        let sources = fixture.sources();
        let mut incoming = fixture.old.clone();
        incoming.dev = incoming.dev.wrapping_add(7);
        let error = sources
            .guard_selected_reference(&fixture.store, &incoming)
            .unwrap_err();
        assert!(
            matches!(error, WriterAlarm::Halted { ref detail } if detail.contains("suspended source"))
        );
        assert!(
            sources
                .guard_selected_reference(&fixture.store, &fixture.new_source())
                .is_ok()
        );
    }

    #[test]
    fn scoped_old_cursor_without_checkpoint_or_with_unreadable_checkpoint_stays_held() {
        let fixture = Fixture::new(false);
        let sources = fixture.sources();
        let before = fixture.store.cursor(&fixture.old).unwrap().clone();
        assert!(sources.check_scoped_metadata(&fixture.store).is_err());
        assert_eq!(fixture.store.cursor(&fixture.old).unwrap(), &before);
        std::fs::write(fixture.checkpoint(), b"not checkpoint JSON").unwrap();
        assert!(sources.check_scoped_metadata(&fixture.store).is_err());
        assert_eq!(fixture.store.cursor(&fixture.old).unwrap(), &before);
        assert!(sources.readers.is_empty());
    }

    #[test]
    fn selected_parent_and_content_candidate_cannot_reopen_a_suspended_source() {
        for hot in [false, true] {
            let mut fixture = Fixture::new(true);
            let sources = fixture.sources();
            let new = fixture.new_source();
            let mut rotation = Rotation::default();
            rotation.links.insert(
                source_key(&new),
                SourceLink {
                    source: new,
                    seq: 2,
                    parent: (!hot).then_some(fixture.old.clone()),
                    committed_at: Utc::now(),
                    boundary: Boundary::Undecided,
                    hot_candidate: hot.then(|| HotCandidate {
                        channel: 29,
                        pane: "unit".into(),
                        nonce: "current-nonce".into(),
                        parent: fixture.old.clone(),
                    }),
                },
            );
            fixture.store.write_rotation(&rotation).unwrap();
            std::fs::remove_file(&fixture.old.path).unwrap();
            assert!(sources.check_scoped_metadata(&fixture.store).is_err());
            assert_eq!(fixture.store.rotation().unwrap(), rotation);
        }
    }

    #[test]
    fn suspended_content_boundary_and_malformed_rotation_stay_held() {
        let mut fixture = Fixture::new(true);
        let sources = fixture.sources();
        let mut rotation = Rotation::default();
        rotation.links.insert(
            source_key(&fixture.old),
            SourceLink {
                source: fixture.old.clone(),
                seq: 1,
                parent: None,
                committed_at: Utc::now(),
                boundary: Boundary::Pending {
                    candidates: vec![0],
                },
                hot_candidate: Some(HotCandidate {
                    channel: 29,
                    pane: "unit".into(),
                    nonce: "old".into(),
                    parent: fixture.old.clone(),
                }),
            },
        );
        fixture.store.write_rotation(&rotation).unwrap();
        assert!(sources.check_scoped_metadata(&fixture.store).is_err());
        assert_eq!(fixture.store.rotation().unwrap(), rotation);
        std::fs::write(
            fixture.root.path().join("o_store/29/boundary"),
            b"not rotation JSON",
        )
        .unwrap();
        assert!(sources.check_scoped_metadata(&fixture.store).is_err());
    }

    #[test]
    fn default_empty_selection_keeps_ordinary_metadata_and_reference_behavior() {
        let fixture = Fixture::new(false);
        let sources = Sources::new(29, ShadowProvider::Codex, Arc::new(EmptyLog));
        assert!(sources.check_scoped_metadata(&fixture.store).is_ok());
        assert!(
            sources
                .guard_selected_reference(&fixture.store, &fixture.old)
                .is_ok()
        );
        assert!(sources.resume_selected(&fixture.old));
        assert!(sources.quiescent_for_rescope());
    }
}
