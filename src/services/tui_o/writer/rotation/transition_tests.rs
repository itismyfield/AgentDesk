use super::*;
use crate::services::tui_o::writer::binding::BindingEvidence;

const CHANNEL: u64 = 7;

fn source(session: &str, ino: u64) -> SourceId {
    let path = std::path::PathBuf::from(format!("/t/{session}.jsonl"));
    let (session_id, dev) = (session.to_owned(), 1);
    SourceId {
        session_id,
        path,
        dev,
        ino,
    }
}

fn event(seq: u64, tmux: &str, record: BindingRecord) -> BindingEvent {
    BindingEvent {
        seq,
        channel_id: CHANNEL,
        provider: ShadowProvider::Claude,
        tmux_session: tmux.into(),
        execution_nonce: "nonce".into(),
        record,
        committed_at: Utc::now(),
    }
}

fn hop(seq: u64, tmux: &str, old: &SourceId, new: BindingTarget) -> BindingEvent {
    let evidence = BindingEvidence {
        hook_event: String::new(),
        received_at: Utc::now(),
        reclaims: false,
    };
    let record = BindingRecord::Bound {
        old: Some(old.clone()),
        new,
        cause: BindingCause::Startup,
        parent_hint: None,
        evidence,
    };
    event(seq, tmux, record)
}

fn moved(seq: u64, tmux: &str, old: &SourceId, new: &SourceId) -> BindingEvent {
    hop(seq, tmux, old, BindingTarget::Source(new.clone()))
}

fn startup(seq: u64, new: &SourceId) -> BindingEvent {
    let mut first = moved(seq, "tmux", new, new);
    if let BindingRecord::Bound { old, .. } = &mut first.record {
        *old = None;
    }
    first
}

/// A rotation whose one successor from `a` to `b` was stored without a seq or pane.
fn legacy(a: &SourceId, b: &SourceId) -> Rotation {
    let mut rotation = Rotation::default();
    let next = Successor {
        source: b.clone(),
        seq: None,
        tmux_session: None,
        drain_to: None,
        proof: None,
    };
    rotation.successors.insert(source_key(a), next);
    rotation
}

// Every applied hop of a legacy successor stays a candidate, so a history naming it more than
// once never reads as one unique hop.
#[test]
fn transition_legacy_hop_candidates_keep_every_applied_match() {
    let (a, b, c) = (source("a", 1), source("b", 2), source("c", 3));
    let rotation = legacy(&a, &b);
    let key = source_key(&a);
    let pending = BindingTarget::Pending {
        payload_session_id: "b".into(),
        payload_transcript_path: b.path.clone(),
    };
    let resolved = |seq| BindingRecord::Resolved {
        resolves_seq: seq,
        source: b.clone(),
    };
    let elsewhere = BindingRecord::Resolved {
        resolves_seq: 2,
        source: c.clone(),
    };
    let cases: [(&str, Vec<BindingEvent>, Vec<(u64, String)>); 6] = [
        (
            "one",
            vec![startup(1, &a), moved(2, "p", &a, &b)],
            vec![(2, "p".into())],
        ),
        (
            "same pair twice",
            vec![startup(1, &a), moved(2, "p", &a, &b), moved(3, "p", &a, &b)],
            vec![(2, "p".into()), (3, "p".into())],
        ),
        (
            "another pane",
            vec![startup(1, &a), moved(2, "p", &a, &b), moved(3, "q", &a, &b)],
            vec![(2, "p".into()), (3, "q".into())],
        ),
        (
            "resolved twice",
            vec![
                startup(1, &a),
                hop(2, "p", &a, pending.clone()),
                event(3, "p", resolved(2)),
                event(4, "p", resolved(2)),
            ],
            vec![(2, "p".into()), (2, "p".into())],
        ),
        (
            "resolved to two files",
            vec![
                startup(1, &a),
                hop(2, "p", &a, pending),
                event(3, "p", resolved(2)),
                event(4, "p", elsewhere),
            ],
            vec![(2, "p".into()), (2, "p".into())],
        ),
        (
            "to another file",
            vec![startup(1, &a), moved(2, "p", &a, &c)],
            vec![],
        ),
    ];
    for (case, events, expected) in cases {
        let checkpoint = events.last().unwrap().seq;
        let found = legacy_hop_candidates(
            &events,
            checkpoint,
            &rotation,
            CHANNEL,
            ShadowProvider::Claude,
        );
        assert_eq!(found.unwrap()[&key], expected, "{case}");
    }
}

// A history the readers could not have applied whole is an error, not fewer candidates.
#[test]
fn transition_legacy_hop_candidates_refuse_a_history_short_of_the_checkpoint() {
    let (a, b) = (source("a", 1), source("b", 2));
    let rotation = legacy(&a, &b);
    let whole = vec![startup(1, &a), moved(2, "p", &a, &b)];
    let mut other_channel = whole.clone();
    other_channel[1].channel_id = CHANNEL + 1;
    let mut codex = whole.clone();
    codex[1].provider = ShadowProvider::Codex;
    let cases = [
        ("gap", vec![whole[1].clone()], 2),
        ("short", whole.clone(), 3),
        ("channel", other_channel, 2),
        ("provider", codex, 2),
    ];
    for (case, events, checkpoint) in cases {
        let found = legacy_hop_candidates(
            &events,
            checkpoint,
            &rotation,
            CHANNEL,
            ShadowProvider::Claude,
        );
        assert!(found.is_err(), "{case}: {found:?}");
    }
}

struct NoLog(watch::Sender<u64>);

impl BindingEvents for NoLog {
    fn binding_events_since(&self, _: u64, _: u64) -> Result<Vec<BindingEvent>, String> {
        Ok(Vec::new())
    }

    fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
        self.0.subscribe()
    }
}

/// A drained reader of a one-line file, holding an FD on it when `fd` is set, quiet, draining
/// and dormant, while a validation of `validated` runs.
fn reading(
    dir: &std::path::Path,
    config: Option<ReaderConfig>,
    fd: bool,
    validated: &str,
) -> (Sources<NoLog>, [SourceId; 2]) {
    let file = |name: &str| {
        let path = dir.join(format!("{name}.jsonl"));
        std::fs::write(&path, b"{}\n").unwrap();
        crate::services::tui_o::shadow::binding_reader::source_id_for(name, &path).unwrap()
    };
    let (a, b) = (file("a"), file("b"));
    let mut sources = Sources::new(
        CHANNEL,
        ShadowProvider::Claude,
        Arc::new(NoLog(watch::channel(0).0)),
    );
    if let Some(config) = config {
        sources = sources.with_readers(config);
    }
    let mut reader = Reader::new(
        a.clone(),
        fd.then(|| SourceCapture::open(a.clone(), 3).unwrap()),
    );
    reader.captured_any = true;
    (reader.quiet, reader.drain_to, reader.dormant) =
        (Some((3, Instant::now())), Some(3), Some(Instant::now()));
    sources.readers.push(reader);
    let validated = if validated == "a" { &a } else { &b };
    let cursor = Cursor {
        source: validated.clone(),
        captured_through: 3,
        prefix_hash: String::new(),
        retired: false,
        transition_cleanup: None,
    };
    sources.validation = Some(Validation {
        cursor,
        fingerprint: Fingerprint::path(validated).unwrap(),
        observed_at: SystemTime::now(),
        result: mpsc::channel().1,
        ready: Arc::default(),
    });
    (sources, [a, b])
}

// A configured reader watches the path without its FD; a legacy reader keeps its FD for the
// retired watch, and one that held none is reported unwatched rather than watched.
#[tokio::test(start_paused = true)]
async fn transition_watch_closes_only_a_configured_readers_fd_and_names_an_unwatched_one() {
    let configured = Some(ReaderConfig { dormant: false });
    let dormant = Some(ReaderConfig { dormant: true });
    let cases = [
        ("legacy_fd", None, true, Watch::Fd, true),
        ("legacy_without_fd", None, false, Watch::Unwatched, false),
        ("configured_fd", configured, true, Watch::Path, false),
        ("configured_without_fd", dormant, false, Watch::Path, false),
    ];
    for (case, config, fd, watch, keeps_fd) in cases {
        let dir = tempfile::tempdir().unwrap();
        let (mut sources, [a, _]) = reading(dir.path(), config, fd, "a");
        let armed = sources.arm_transition_watch(&a);
        let reader = &sources.readers[0];
        assert_eq!(reader.capture.is_some(), keeps_fd, "[{case}] fd");
        assert_eq!(armed, Some(watch), "[{case}] watch");
        assert_eq!(
            reader.watch_until,
            Some(Instant::now() + RETIRED_WATCH),
            "[{case}] until"
        );
        let cleared = (
            reader.quiet,
            reader.drain_to,
            reader.dormant,
            reader.idle.is_none(),
        );
        assert_eq!(cleared, (None, None, None, true), "[{case}] cleared");
        assert!(reader.captured_any, "[{case}] captured_any");
        assert!(sources.validation.is_none(), "[{case}] its validation");
    }
    let dir = tempfile::tempdir().unwrap();
    let (mut sources, [a, b]) = reading(dir.path(), None, true, "b");
    assert_eq!(sources.arm_transition_watch(&a), Some(Watch::Fd));
    let kept = sources
        .validation
        .as_ref()
        .map(|job| job.cursor.source.clone());
    assert_eq!(kept, Some(b.clone()), "another source's validation stays");
    assert_eq!(sources.arm_transition_watch(&b), None, "no reader");
}
