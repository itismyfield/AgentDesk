//! Current-source safety through retained cursors, real resume, and the actor's ready boundary.
use super::*;
use crate::services::tui_o::writer::actor::{Owing, spawn_projecting};
use crate::services::tui_o::writer::pieces::UnitDeriver;
use crate::services::tui_o::writer::rotation::Sources;
use std::collections::BTreeMap;

struct View {
    h: Harness,
    ids: [SourceId; 3],
    log: Arc<FakeBindings>,
}

impl View {
    fn new() -> Self {
        let (h, _, a) = switched_over(b"");
        let make = |session: &str| {
            let path = h._runtime.path().join(format!("{session}.jsonl"));
            std::fs::write(&path, b"").unwrap();
            source_id_for(session, &path).unwrap()
        };
        let ids = [a, make("b"), make("c")];
        // Keep a nonempty raw spool whose excluded record cannot manufacture an answer.
        append(
            &ids[0].path,
            b"{\"type\":\"progress\",\"uuid\":\"retained-evidence\"}\n",
        );
        let mut capture = SourceCapture::open(ids[0].clone(), 0).unwrap();
        let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
            panic!("raw evidence must be captured")
        };
        h.channel()
            .append_spool(&batch, &capture.prefix_hash())
            .unwrap();
        h.gate.acquired();
        Self {
            h,
            ids,
            log: Arc::new(FakeBindings::new()),
        }
    }

    fn bind(&self, seq: u64, old: Option<usize>, new: usize) {
        self.log.commit(bound(
            seq,
            old.map(|i| &self.ids[i]),
            BindingTarget::Source(self.ids[new].clone()),
            BindingCause::Startup,
            None,
        ));
    }

    fn attach(&self, index: usize, seq: Option<u64>) {
        let mut store = self.h.channel();
        store.attach_source(&self.ids[index]).unwrap();
        if let Some(seq) = seq {
            let mut rotation = store.rotation().unwrap();
            rotation.links.insert(
                source_key(&self.ids[index]),
                SourceLink {
                    source: self.ids[index].clone(),
                    seq,
                    parent: None,
                    hot_candidate: None,
                    committed_at: Utc::now(),
                    boundary: Boundary::Owed { from: 0 },
                },
            );
            store.write_rotation(&rotation).unwrap();
        }
    }

    fn checkpoint(&self, seq: u64) {
        self.h.channel().set_binding_checkpoint(seq).unwrap();
    }

    fn resume(&self, absent: bool) -> (Sources<FakeBindings>, Result<(), WriterAlarm>) {
        let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, self.log.clone())
            .with_producer_absent(absent);
        let result = sources.resume(
            &mut self.h.writer(),
            &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
            &mut VecDeque::new(),
        );
        (sources, result)
    }

    fn evidence(&self) -> BTreeMap<PathBuf, Vec<u8>> {
        fn visit(path: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
            for entry in std::fs::read_dir(path).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(&path, files);
                } else {
                    files.insert(path.clone(), std::fs::read(path).unwrap());
                }
            }
        }
        let base = self
            .h
            ._runtime
            .path()
            .join("o_store")
            .join(CHANNEL.to_string());
        let mut files = BTreeMap::new();
        for directory in ["cursor", "spool"] {
            visit(&base.join(directory), &mut files);
        }
        assert!(
            files
                .values()
                .any(|bytes| bytes.windows(17).any(|part| part == b"retained-evidence"))
        );
        files
    }

    fn cycle(&self, checkpoint: u64) {
        self.bind(1, None, 0);
        self.bind(2, Some(0), 1);
        self.bind(3, Some(1), 0);
        self.attach(1, Some(2));
        let mut store = self.h.channel();
        let mut rotation = store.rotation().unwrap();
        rotation.successors.insert(
            source_key(&self.ids[1]),
            Successor {
                source: self.ids[0].clone(),
                seq: Some(3),
                tmux_session: Some("tmux".into()),
                drain_to: Some(0),
                proof: None,
            },
        );
        store.write_rotation(&rotation).unwrap();
        self.checkpoint(checkpoint);
    }
}

fn actor(view: &View, absent: bool) -> (watch::Sender<bool>, watch::Receiver<bool>, Actor, Raised) {
    let alarms = Raised::default();
    let writer = ChannelWriter::new(
        view.h.channel(),
        view.h.gate.clone(),
        view.h.port.clone(),
        view.h.lease.clone(),
        alarms.clone(),
    );
    let (stop, stopped) = watch::channel(false);
    let (resumed, ready) = watch::channel(false);
    let config = WriterConfig {
        enabled: true,
        producer_absent: absent,
    };
    let task = spawn_projecting(
        &config,
        None,
        writer,
        ShadowProvider::Claude,
        view.log.clone(),
        (stopped, resumed, watch::channel(None).0, Owing::default()),
    )
    .unwrap();
    (stop, ready, task, alarms)
}

async fn until(reason: &str, predicate: impl Fn() -> bool) {
    for _ in 0..120 {
        if predicate() {
            return;
        }
        polls(1).await;
        tokio::task::yield_now().await;
    }
    assert!(predicate(), "{reason}");
}

pub(super) async fn rebound_absence_guard() {
    for observation in [ProducerObservation::Present, ProducerObservation::Unknown] {
        let view = View::new();
        view.cycle(3);
        std::fs::remove_file(&view.ids[0].path).unwrap();
        let evidence = view.evidence();
        let (sources, result) = view.resume(false);
        let selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
        install_boot(false);
        let io = TestIo::over(&view.h);
        *io.producer.lock().unwrap() = Some(observation);
        p5_log(
            view.h._runtime.path(),
            CHANNEL,
            &[
                logged(1, None, &view.ids[0]),
                logged(2, Some(&view.ids[0]), &view.ids[1]),
                logged(3, Some(&view.ids[1]), &view.ids[0]),
            ]
            .concat(),
        );
        let ready = Arc::new(Readiness::default());
        let tasks = hosted(&view.h, &io, true, &ready);
        until(
            "current loss must resolve as halt or expose bad readiness",
            || ready.accepts(CHANNEL) || !io.alarms.halted().is_empty(),
        )
        .await;
        eprintln!(
            "REBOUND_PROBE observation={observation:?} parked={} accepts={} halted={:?}",
            sources.is_parked(&view.ids[0]),
            ready.accepts(CHANNEL),
            io.alarms.halted()
        );
        assert!(
            result.is_err(),
            "current source must retain its reopen halt: {observation:?}"
        );
        assert!(
            !sources.is_parked(&view.ids[0]),
            "current A must never be historical"
        );
        assert!(!ready.accepts(CHANNEL));
        assert!(
            io.calls()
                .iter()
                .any(|(call, channel)| call.starts_with("producer-") && *channel == CHANNEL),
            "host must probe every missing current source"
        );
        assert!(!io.alarms.halted().is_empty());
        assert!(
            !io.alarms
                .1
                .lock()
                .unwrap()
                .iter()
                .any(|(_, state)| *state == "cleared")
        );
        assert_eq!(view.evidence(), evidence);
        abort(tasks);
        drop(selected);
    }
}

#[tokio::test(start_paused = true)]
async fn rebound_then_new_source_parks_both_predecessors() {
    if !child_case(concat!(
        module_path!(),
        "::rebound_then_new_source_parks_both_predecessors"
    )) {
        return;
    }
    for missing in 0..3 {
        let view = View::new();
        view.cycle(3);
        view.bind(4, Some(0), 2);
        view.attach(2, Some(4));
        let mut store = view.h.channel();
        let mut rotation = store.rotation().unwrap();
        rotation.successors.insert(
            source_key(&view.ids[0]),
            Successor {
                source: view.ids[2].clone(),
                seq: Some(4),
                tmux_session: Some("tmux".into()),
                drain_to: None,
                proof: None,
            },
        );
        store.write_rotation(&rotation).unwrap();
        view.checkpoint(4);
        std::fs::remove_file(&view.ids[missing].path).unwrap();
        let evidence = view.evidence();
        let (sources, result) = view.resume(false);
        assert_eq!(
            result.is_ok(),
            missing != 2,
            "only current C loss must halt: {result:?}"
        );
        assert_eq!(sources.is_parked(&view.ids[missing]), missing != 2);
        assert_eq!(view.evidence(), evidence);
    }
}

#[tokio::test(start_paused = true)]
async fn oldless_other_pane_orphan_parks_while_session_present() {
    if !child_case(concat!(
        module_path!(),
        "::oldless_other_pane_orphan_parks_while_session_present"
    )) {
        return;
    }
    let view = View::new();
    view.bind(1, None, 0);
    view.bind(2, None, 1);
    view.attach(1, None);
    view.checkpoint(2);
    assert!(view.h.channel().rotation().unwrap().successors.is_empty());
    std::fs::remove_file(&view.ids[0].path).unwrap();
    let evidence = view.evidence();
    let (sources, result) = view.resume(false);
    result.expect("oldless historical source needs no presence absence");
    assert!(sources.is_parked(&view.ids[0]));
    assert!(!sources.is_parked(&view.ids[1]));
    assert_eq!(view.evidence(), evidence);
}

#[tokio::test(start_paused = true)]
async fn interrupted_rebind_cycle_keeps_current_halted() {
    if !child_case(concat!(
        module_path!(),
        "::interrupted_rebind_cycle_keeps_current_halted"
    )) {
        return;
    }
    let view = View::new();
    view.cycle(2);
    std::fs::remove_file(&view.ids[0].path).unwrap();
    let evidence = view.evidence();
    let (sources, result) = view.resume(false);
    assert!(
        result.is_err(),
        "rotation ahead of checkpoint must not make current A historical"
    );
    assert!(!sources.is_parked(&view.ids[0]));
    assert_eq!(view.h.channel().binding_checkpoint().unwrap(), Some(2));
    assert_eq!(view.evidence(), evidence);
}

fn unapplied() -> View {
    let view = View::new();
    view.bind(1, None, 0);
    view.bind(2, Some(0), 1);
    view.checkpoint(1);
    assert!(view.h.channel().cursor(&view.ids[1]).is_none());
    std::fs::remove_file(&view.ids[0].path).unwrap();
    view
}

#[tokio::test(start_paused = true)]
async fn unapplied_new_bind_parks_old_and_attaches_before_ready() {
    if !child_case(concat!(
        module_path!(),
        "::unapplied_new_bind_parks_old_and_attaches_before_ready"
    )) {
        return;
    }
    let view = unapplied();
    append(
        &view.ids[1].path,
        &row("new-first", "the fresh first answer"),
    );
    let (stop, ready, task, alarms) = actor(&view, false);
    until("new durable bind must attach before ready", || {
        *ready.borrow() || task.is_finished()
    })
    .await;
    assert!(*ready.borrow(), "{:?}", alarms.halted());
    assert_eq!(view.h.channel().binding_checkpoint().unwrap(), Some(2));
    assert!(view.h.channel().cursor(&view.ids[1]).is_some());
    until("new source owes exactly one first reply", || {
        view.h.port.posts().len() == 1
    })
    .await;
    polls(5).await;
    assert_eq!(view.h.port.posts(), ["the fresh first answer"]);
    assert!(alarms.halted().is_empty());
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn unapplied_new_bind_whose_source_is_missing_never_publishes_ready() {
    if !child_case(concat!(
        module_path!(),
        "::unapplied_new_bind_whose_source_is_missing_never_publishes_ready"
    )) {
        return;
    }
    let view = unapplied();
    std::fs::remove_file(&view.ids[1].path).unwrap();
    let (stop, ready, task, alarms) = actor(&view, false);
    until("failed unapplied bind must halt", || task.is_finished()).await;
    for _ in 0..3 {
        assert!(
            !*ready.borrow(),
            "an attach failure must never publish resumed"
        );
        assert!(!alarms.halted().is_empty());
        assert!(
            !alarms
                .1
                .lock()
                .unwrap()
                .iter()
                .any(|(_, state)| *state == "cleared"),
            "stale halt must survive failed pre-ready follow"
        );
        polls(1).await;
    }
    assert!(view.h.port.posts().is_empty());
    assert!(task.await.unwrap().is_some());
    drop(stop);
}

#[tokio::test(start_paused = true)]
async fn unresolved_pending_suffix_keeps_missing_source_halted() {
    if !child_case(concat!(
        module_path!(),
        "::unresolved_pending_suffix_keeps_missing_source_halted"
    )) {
        return;
    }
    for absent in [false, true] {
        let view = View::new();
        view.bind(1, None, 0);
        let mut pending = bound(
            2,
            Some(&view.ids[0]),
            BindingTarget::Pending {
                payload_session_id: "waiting".into(),
                payload_transcript_path: view.h._runtime.path().join("waiting.jsonl"),
            },
            BindingCause::Startup,
            None,
        );
        pending.tmux_session = "waiting-pane".into();
        view.log.commit(pending);
        let mut other = bound(
            3,
            None,
            BindingTarget::Source(view.ids[2].clone()),
            BindingCause::Startup,
            None,
        );
        other.tmux_session = "other-pane".into();
        view.log.commit(other);
        view.attach(2, Some(3));
        view.checkpoint(1);
        std::fs::remove_file(&view.ids[0].path).unwrap();
        let (sources, result) = view.resume(absent);
        assert_eq!(
            result.is_ok(),
            absent,
            "unresolved suffix makes current Unknown: {result:?}"
        );
        assert_eq!(sources.is_parked(&view.ids[0]), absent);
    }
}

#[tokio::test(start_paused = true)]
async fn unreadable_or_gapped_log_requires_absence() {
    if !child_case(concat!(
        module_path!(),
        "::unreadable_or_gapped_log_requires_absence"
    )) {
        return;
    }
    for (failed, absent) in [(true, false), (true, true), (false, false), (false, true)] {
        let view = View::new();
        view.bind(1, None, 0);
        view.bind(3, None, 1);
        view.attach(1, Some(3));
        view.checkpoint(1);
        if failed {
            view.log.fail(Some("fixture log outage"));
        }
        std::fs::remove_file(&view.ids[0].path).unwrap();
        let (sources, result) = view.resume(absent);
        assert_eq!(
            result.is_ok(),
            absent,
            "failed={failed}, absent={absent}, {result:?}"
        );
        assert_eq!(sources.is_parked(&view.ids[0]), absent);
    }
}

#[tokio::test(start_paused = true)]
async fn store_without_checkpoint_requires_absence() {
    if !child_case(concat!(
        module_path!(),
        "::store_without_checkpoint_requires_absence"
    )) {
        return;
    }
    for absent in [false, true] {
        let view = View::new();
        view.bind(1, None, 0);
        view.bind(2, Some(0), 1);
        view.attach(1, Some(2));
        assert_eq!(view.h.channel().binding_checkpoint().unwrap(), None);
        std::fs::remove_file(&view.ids[0].path).unwrap();
        let (sources, result) = view.resume(absent);
        assert_eq!(
            result.is_ok(),
            absent,
            "a seeded baseline must not replace the missing checkpoint observation: {result:?}"
        );
        assert_eq!(sources.is_parked(&view.ids[0]), absent);
    }
}

#[tokio::test(start_paused = true)]
async fn parked_source_bound_again_halts_for_revalidation() {
    if !child_case(concat!(
        module_path!(),
        "::parked_source_bound_again_halts_for_revalidation"
    )) {
        return;
    }
    let view = View::new();
    view.bind(1, None, 0);
    view.bind(2, None, 1);
    view.attach(1, None);
    view.checkpoint(2);
    let held = view.ids[0].path.with_extension("held");
    std::fs::rename(&view.ids[0].path, &held).unwrap();
    let (stop, ready, task, alarms) = actor(&view, false);
    until("historical A must park before a later rebound", || {
        *ready.borrow() || task.is_finished()
    })
    .await;
    assert!(*ready.borrow(), "{:?}", alarms.halted());
    view.bind(3, Some(1), 0);
    until(
        "a parked target bound after the observed tail must halt",
        || task.is_finished(),
    )
    .await;
    assert!(!alarms.halted().is_empty());
    assert_eq!(view.h.channel().binding_checkpoint().unwrap(), Some(2));
    assert!(task.await.unwrap().is_some());
    drop(stop);
    std::fs::rename(&held, &view.ids[0].path).unwrap();
    let (stop, ready, task, alarms) = actor(&view, false);
    until("a fresh host must reopen the returned original", || {
        *ready.borrow() || task.is_finished()
    })
    .await;
    assert!(*ready.borrow(), "{:?}", alarms.halted());
    assert!(!task.is_finished());
    assert_eq!(view.h.channel().binding_checkpoint().unwrap(), Some(3));
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn parked_source_refined_within_resume_view_halts_before_ready() {
    if !child_case(concat!(
        module_path!(),
        "::parked_source_refined_within_resume_view_halts_before_ready"
    )) {
        return;
    }
    let view = View::new();
    view.bind(1, None, 0);
    let mut hot = bound(
        2,
        Some(&view.ids[0]),
        BindingTarget::Source(view.ids[1].clone()),
        BindingCause::Unknown,
        None,
    );
    hot.tmux_session = "refinement-pane".into();
    hot.execution_nonce = "refinement-execution".into();
    view.log.commit(hot);
    let mut explicit = bound(
        3,
        Some(&view.ids[1]),
        BindingTarget::Source(view.ids[1].clone()),
        BindingCause::Startup,
        None,
    );
    explicit.tmux_session = "refinement-pane".into();
    explicit.execution_nonce = "refinement-execution".into();
    view.log.commit(explicit);
    view.attach(1, None);
    let mut store = view.h.channel();
    let mut rotation = store.rotation().unwrap();
    rotation.links.insert(
        source_key(&view.ids[1]),
        SourceLink {
            source: view.ids[1].clone(),
            seq: 2,
            parent: None,
            committed_at: Utc::now(),
            boundary: Boundary::Pending {
                candidates: vec![0],
            },
            hot_candidate: Some(crate::services::tui_o::store::rotation::HotCandidate {
                channel: CHANNEL,
                pane: "refinement-pane".into(),
                nonce: "refinement-execution".into(),
                parent: view.ids[0].clone(),
            }),
        },
    );
    rotation.successors.insert(
        source_key(&view.ids[0]),
        Successor {
            source: view.ids[1].clone(),
            seq: Some(2),
            tmux_session: Some("refinement-pane".into()),
            drain_to: Some(store.cursor(&view.ids[0]).unwrap().captured_through),
            proof: None,
        },
    );
    store.write_rotation(&rotation).unwrap();
    drop(store);
    view.checkpoint(2);
    std::fs::remove_file(&view.ids[1].path).unwrap();
    let evidence = view.evidence();
    let (sources, result) = view.resume(true);
    result.expect(
        "a missing current source with no unsealed body may park before explicit refinement",
    );
    assert!(sources.is_parked(&view.ids[1]));
    let (stop, ready, task, alarms) = actor(&view, true);
    until(
        "refinement of a parked reader inside the observed tail must halt",
        || task.is_finished(),
    )
    .await;
    assert!(
        !*ready.borrow(),
        "a changed boundary needs a fresh resume before ready"
    );
    assert!(!alarms.halted().is_empty());
    assert!(
        !alarms
            .1
            .lock()
            .unwrap()
            .iter()
            .any(|(_, state)| *state == "cleared")
    );
    let rotation = view.h.channel().rotation().unwrap();
    let link = rotation.link(&view.ids[1]).unwrap();
    assert_eq!(
        link.seq, 3,
        "the explicit bind must really have refined the provisional link"
    );
    assert!(link.hot_candidate.is_none());
    assert_eq!(link.boundary, Boundary::Owed { from: 0 });
    assert_eq!(view.h.channel().binding_checkpoint().unwrap(), Some(2));
    assert_eq!(view.evidence(), evidence);
    assert!(task.await.unwrap().is_some());
    drop(stop);
}
