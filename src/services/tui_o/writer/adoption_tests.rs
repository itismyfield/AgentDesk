use super::*;
use crate::services::tui_o::channel_policy::{Adoption, Candidate};
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::store::{OStore, StoreConfig};
use crate::services::tui_o::writer::activation::{self, ActivationFacts, test_hook};
use crate::services::tui_o::writer::binding::{BindingCause, BindingEvidence};
use std::io::Write;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::watch;

const TMUX: &str = "adoption-unit";

/// Legacy's state as each test sets it; every fact passes unless the test changes it.
struct Legacy {
    cursor: Mutex<Option<u64>>,
    path: PathBuf,
    frontier: Mutex<Option<u64>>,
    tail: AtomicBool,
}

impl LegacyView for Legacy {
    fn started(&self) -> bool {
        true
    }

    fn cursor(&self, tmux: &str) -> LegacyCursor {
        assert_eq!(
            tmux, TMUX,
            "the cursor is read for the session the log binds"
        );
        let offset = self.cursor.lock().unwrap().expect("cursor set");
        let path = self.path.clone();
        LegacyCursor::Bound { path, offset }
    }

    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        *self.frontier.lock().unwrap()
    }

    fn tail_running(&self, _: &str) -> bool {
        self.tail.load(Ordering::Acquire)
    }
}

struct Log(Vec<BindingEvent>);

impl BindingEvents for Log {
    fn binding_events_since(&self, _: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        Ok(self.0.iter().filter(|e| e.seq > after).cloned().collect())
    }

    fn subscribe(&self, _: u64) -> watch::Receiver<u64> {
        watch::channel(self.0.len() as u64).1
    }
}

fn bound(seq: u64, channel: u64, old: Option<&SourceId>, new: &SourceId) -> BindingEvent {
    let received_at = chrono::Utc::now();
    let cause = match old {
        None => BindingCause::Startup,
        Some(_) => BindingCause::Clear,
    };
    BindingEvent {
        seq,
        channel_id: channel,
        provider: ShadowProvider::Claude,
        tmux_session: TMUX.into(),
        execution_nonce: "unit".into(),
        record: BindingRecord::Bound {
            old: old.cloned(),
            new: BindingTarget::Source(new.clone()),
            cause,
            parent_hint: None,
            evidence: BindingEvidence {
                hook_event: "SessionStart".into(),
                received_at,
                reclaims: false,
            },
        },
        committed_at: received_at,
    }
}

fn turn(key: &str) -> String {
    let rows = [
        serde_json::json!({"type":"user", "uuid":format!("{key}-q"), "message":{"content":"question"}}),
        serde_json::json!({"type":"assistant", "uuid":format!("{key}-a"), "apiBlockIndex":0,
            "message":{"id":key, "content":[{"type":"text", "text":format!("answer {key}")}]}}),
        serde_json::json!({"type":"system", "subtype":"turn_duration", "durationMs":5}),
    ];
    rows.iter().map(|row| format!("{row}\n")).collect()
}

fn append(path: &Path, text: &str) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(text.as_bytes()).unwrap();
}

/// A channel whose one bound transcript holds a closed turn, with Legacy's cursor at its end and
/// the delivery frontier covering it.
struct Channel {
    dir: tempfile::TempDir,
    channel: u64,
    source: SourceId,
    legacy: Legacy,
    log: Log,
    candidate: Candidate,
}

impl Channel {
    fn new(channel: u64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("current.jsonl");
        std::fs::write(&path, turn("first")).unwrap();
        let source = source_id_for("unit", &path).unwrap();
        let end = std::fs::metadata(&path).unwrap().len();
        let legacy = Legacy {
            cursor: Mutex::new(Some(end)),
            path,
            frontier: Mutex::new(Some(end)),
            tail: AtomicBool::new(false),
        };
        let log = Log(vec![bound(1, channel, None, &source)]);
        let candidate = Candidate::new(Adoption::Pending);
        Self {
            dir,
            channel,
            source,
            legacy,
            log,
            candidate,
        }
    }

    fn end(&self) -> u64 {
        std::fs::metadata(&self.source.path).unwrap().len()
    }

    fn store(&self) -> OStore {
        let root = self.dir.path().join("runtime");
        std::fs::create_dir_all(&root).unwrap();
        let store = OStore::open_if_enabled(&StoreConfig { enabled: true }, &root);
        store.unwrap().unwrap()
    }

    fn pin(&self) -> Result<Snapshot, String> {
        pin(&self.legacy, &self.log.0, self.channel).map_err(String::from)
    }

    /// The host's first activation over `snapshot`, with no gateway or local blocker.
    fn activate(&self, store: &OStore, snapshot: &Snapshot) -> Result<(), String> {
        let (legacy, log, channel) = (&self.legacy, &self.log, self.channel);
        let sources = || snapshot.recheck(legacy, log, channel).map_err(String::from);
        let facts = Ok(ActivationFacts::default());
        activation::activate_with(
            store,
            channel,
            facts,
            || Ok(false),
            &self.candidate,
            sources,
        )
    }

    fn assert_released(&self, store: &OStore, result: Result<(), String>, reason: &str) {
        let detail = result.expect_err("the adoption is refused");
        assert!(
            detail.contains(reason),
            "refused for {reason}, not: {detail}"
        );
        assert_eq!(self.candidate.peek(), Adoption::Released);
        assert_eq!(store.read_init(self.channel).unwrap(), None, "no init");
    }
}

#[test]
fn a_length_change_under_the_lock_alone_releases_the_channel() {
    let channel = Channel::new(641001);
    let store = channel.store();
    let snapshot = channel.pin().unwrap();
    let modified = std::fs::metadata(&channel.source.path).unwrap().modified();
    append(&channel.source.path, &turn("late"));
    let file = std::fs::File::options()
        .append(true)
        .open(&channel.source.path);
    file.unwrap().set_modified(modified.unwrap()).unwrap();
    let result = channel.activate(&store, &snapshot);
    channel.assert_released(&store, result, "length moved");
}

#[test]
fn an_open_turn_before_legacys_cursor_is_not_adopted() {
    let channel = Channel::new(641002);
    let row = serde_json::json!({"type":"assistant", "uuid":"open-a", "apiBlockIndex":0,
        "message":{"id":"open", "content":[{"type":"text", "text":"still streaming"}]}});
    append(&channel.source.path, &format!("{row}\n"));
    // Legacy delivered the closed turn only, so the open one is also undelivered.
    *channel.legacy.cursor.lock().unwrap() = Some(channel.end());
    let detail = channel.pin().expect_err("an open turn is refused");
    assert!(detail.contains("still open"), "{detail}");
}

#[test]
fn a_running_legacy_tail_releases_the_channel() {
    let channel = Channel::new(641004);
    let store = channel.store();
    let snapshot = channel.pin().unwrap();
    channel.legacy.tail.store(true, Ordering::Release);
    let result = channel.activate(&store, &snapshot);
    channel.assert_released(&store, result, "tail is running");
}

#[test]
fn o_starts_at_legacys_cursor_when_a_unit_lands_after_the_recheck() {
    let channel = Channel::new(641005);
    let store = channel.store();
    let start = channel.end();
    let head = hex::encode(Sha256::digest(std::fs::read(&channel.source.path).unwrap()));
    let snapshot = channel.pin().unwrap();
    assert_eq!(
        snapshot.abandoned(),
        None,
        "Legacy delivered everything before its cursor"
    );
    let path = channel.source.path.clone();
    test_hook::set(channel.channel, test_hook::Step::BeforeWrite, move || {
        append(&path, &turn("late"));
        Ok(())
    });
    let asked = std::time::Instant::now();
    channel.activate(&store, &snapshot).unwrap();
    eprintln!("adoption over a held transcript took {:?}", asked.elapsed());
    assert_eq!(channel.candidate.peek(), Adoption::Committed);
    let init = store.read_init(channel.channel).unwrap().unwrap();
    let [source] = init.sources.as_slice() else {
        panic!("{:?}", init.sources);
    };
    assert_eq!(source.source_id, channel.source);
    assert_eq!(
        (source.delivery_start, source.prefix_hash.as_str()),
        (start, head.as_str()),
        "O starts at Legacy's cursor, over the bytes before it"
    );
}

/// A channel rotated once: the old transcript it bound first, then its current one.
fn rotated(channel: u64, past_count: usize) -> (Channel, Vec<SourceId>) {
    let mut adopted = Channel::new(channel);
    let mut past = Vec::new();
    for index in 0..past_count {
        let path = adopted.dir.path().join(format!("past-{index}.jsonl"));
        std::fs::write(&path, turn(&format!("past{index}"))).unwrap();
        past.push(source_id_for("unit", &path).unwrap());
    }
    let mut events = Vec::new();
    let mut old: Option<&SourceId> = None;
    for source in past.iter().chain([&adopted.source]) {
        events.push(bound(events.len() as u64 + 1, channel, old, source));
        old = Some(source);
    }
    adopted.log = Log(events);
    (adopted, past)
}

#[test]
fn a_past_source_starts_at_its_length_when_the_channel_is_adopted() {
    let (channel, past) = rotated(641006, 1);
    let store = channel.store();
    let snapshot = channel.pin().unwrap();
    channel.activate(&store, &snapshot).unwrap();
    let init = store.read_init(channel.channel).unwrap().unwrap();
    let old = init.sources.iter().find(|s| s.source_id == past[0]);
    let old = old.expect("the past source is attached");
    let bytes = std::fs::read(&past[0].path).unwrap();
    let whole = (bytes.len() as u64, hex::encode(Sha256::digest(&bytes)));
    assert_eq!((old.delivery_start, old.prefix_hash.clone()), whole);
    assert_eq!(init.sources.len(), 2, "{:?}", init.sources);
}

#[test]
fn past_sources_over_the_boot_budget_are_not_adopted() {
    let (channel, _) = rotated(641007, PAST_BUDGET_SOURCES + 1);
    let detail = channel.pin().expect_err("the budget refuses the channel");
    assert!(detail.contains("past sources exceed budget"), "{detail}");
}

fn prompt(key: &str) -> serde_json::Value {
    serde_json::json!({"type":"user", "uuid":format!("{key}-q"), "message":{"content":key}})
}

fn answer(key: &str) -> serde_json::Value {
    serde_json::json!({"type":"assistant", "uuid":format!("{key}-a"), "apiBlockIndex":0,
        "message":{"id":key, "content":[{"type":"text", "text":format!("answer {key}")}]}})
}

fn system(subtype: &str) -> serde_json::Value {
    serde_json::json!({"type":"system", "subtype":subtype})
}

/// A channel whose transcript is `rows`, with Legacy's cursor at its end and its delivered
/// frontier at the end of the first `delivered` rows.
fn delivered(channel: u64, rows: &[serde_json::Value], delivered: usize) -> Channel {
    let adopted = Channel::new(channel);
    let lines: Vec<String> = rows.iter().map(|row| format!("{row}\n")).collect();
    std::fs::write(&adopted.source.path, lines.concat()).unwrap();
    let frontier = lines[..delivered].iter().map(String::len).sum::<usize>();
    *adopted.legacy.cursor.lock().unwrap() = Some(adopted.end());
    *adopted.legacy.frontier.lock().unwrap() = Some(frontier as u64);
    adopted
}

/// How far past Legacy's frontier the undelivered records the adopted `channel` reports start.
fn abandoned_past_frontier(channel: &Channel) -> u64 {
    let snapshot = channel
        .pin()
        .expect("undelivered records do not refuse the channel");
    let frontier = channel.legacy.frontier.lock().unwrap().unwrap();
    match snapshot.abandoned() {
        Some(WriterAlarm::Abandoned { source, from, to }) => {
            assert_eq!((source, to), (channel.source.clone(), channel.end()));
            from - frontier
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn assistant_text_past_legacys_frontier_is_reported_undelivered() {
    // The TUI can write late text after the stop hook, where Legacy's reader ends the turn.
    let late = [answer("first"), system("stop_hook_summary"), answer("late")];
    let rows = [&[prompt("q")], &late[..], &[system("turn_duration")]].concat();
    let channel = delivered(641008, &rows, 3);
    assert_eq!(abandoned_past_frontier(&channel), 0);
}

/// The length of `row` as a transcript line.
fn line_len(row: &serde_json::Value) -> u64 {
    format!("{row}\n").len() as u64
}

#[test]
fn a_prompt_past_legacys_frontier_is_reported_undelivered() {
    let first = [prompt("q"), answer("first"), system("stop_hook_summary")];
    let next = [
        system("turn_duration"),
        prompt("next"),
        system("turn_duration"),
    ];
    let channel = delivered(641009, &[first, next].concat(), 3);
    let quiet = line_len(&system("turn_duration"));
    assert_eq!(abandoned_past_frontier(&channel), quiet);
}

#[test]
fn a_turn_legacys_reader_has_not_ended_is_reported_undelivered() {
    // A turn closed by its duration alone never reaches the stop hook Legacy's reader ends at.
    let error = serde_json::json!({"type":"assistant", "uuid":"error-a", "isApiErrorMessage":true,
        "message":{"model":"<synthetic>", "content":[{"type":"text", "text":"API Error"}]}});
    let first = [prompt("q"), answer("first"), system("stop_hook_summary")];
    let next = [
        system("turn_duration"),
        prompt("again"),
        error,
        system("turn_duration"),
    ];
    let channel = delivered(641010, &[&first[..], &next].concat(), 3);
    let quiet = line_len(&system("turn_duration"));
    assert_eq!(abandoned_past_frontier(&channel), quiet);
}
