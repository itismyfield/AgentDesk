use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use serde_json::json;
use sha2::{Digest, Sha256};

use super::super::effort_restart_baseline_tests::{Fixture, LABEL, append, native_end, prompt};
use super::super::*;
use super::*;
use crate::services::tui_o::writer::input_facts::{Ordered, TurnState};

fn fold(fixture: &Fixture) -> InputFacts {
    let mut facts = prepare(
        InputFacts::open(fixture.binding.clone()),
        &fixture.live(),
        &fixture.key,
    )
    .unwrap();
    facts.poll(u64::MAX).unwrap();
    facts
}

fn open_id(facts: &mut InputFacts) -> Option<String> {
    match facts.poll(u64::MAX).unwrap().state {
        TurnState::Open { native_turn_id } => native_turn_id,
        state => panic!("expected open, got {state:?}"),
    }
}

#[test]
fn old_end_cannot_close_new_open() {
    for (channel, id) in [(8_109_220_010, "new"), (8_109_220_011, "old")] {
        let fixture = Fixture::new(channel, &[prompt("old")]);
        let cutoff = std::fs::metadata(&fixture.binding.source.path)
            .unwrap()
            .len();
        fixture.store_end(&fixture.end("old", cutoff, cutoff));
        append(&fixture.binding.source.path, &[prompt(id)]);
        let mut facts = fold(&fixture);
        assert_eq!(
            facts
                .events()
                .iter()
                .filter(|event| matches!(event, Ordered::Opened { .. }))
                .count(),
            2
        );
        assert!(
            !facts
                .events()
                .iter()
                .any(|event| matches!(event, Ordered::Closed { .. }))
        );
        assert_eq!(open_id(&mut facts).as_deref(), Some(id));
        assert!(!facts.restart_ended());
        let watch = fixture.boot();
        lock(&watch).grew_at = Instant::now();
        assert_eq!(fixture.ask(&watch).0, observed(Activity::Busy, "open"));
    }

    let fixture = Fixture::new(8_109_220_012, &[prompt("old")]);
    let cutoff = std::fs::metadata(&fixture.binding.source.path)
        .unwrap()
        .len();
    let end = fixture.end("old", cutoff, cutoff);
    fixture.store_end(&end);
    let scripted = Scripted::new(&fixture, vec![end]);
    let ports: Arc<dyn Ports> = scripted.clone();
    let watch = Arc::new(Mutex::new(Watch::default()));
    let ask = || {
        judge(
            &watch,
            &ports,
            fixture.key.provider,
            fixture.key.channel,
            Target::Bound(LABEL.into()),
        )
    };
    assert_eq!(ask().0, observed(Activity::Unknown, "catching_up"));
    scripted.run();
    assert_eq!(ask().0, observed(Activity::Idle, "effort_restart_ended"));
    append(&fixture.binding.source.path, &[prompt("old")]);
    assert_eq!(ask().0, observed(Activity::Unknown, "catching_up"));
    scripted.run();
    assert_eq!(ask().0, observed(Activity::Busy, "open"));
}

#[test]
fn wrong_nonce_or_cutoff_cannot_close() {
    let fixture = Fixture::new(
        8_109_220_020,
        &[
            prompt("old"),
            json!({"type":"summary","summary":"metadata"}),
        ],
    );
    let open_after = format!("{}\n", prompt("old")).len() as u64;
    let cutoff = std::fs::metadata(&fixture.binding.source.path)
        .unwrap()
        .len();
    let valid = fixture.end("old", open_after, cutoff);
    let history = fixture
        .live()
        .binding_events(fixture.key.channel, fixture.key.provider)
        .unwrap();
    let mut wrong_nonce = valid.clone();
    wrong_nonce.execution_nonce = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into();
    let mut wrong_cutoff = valid.clone();
    wrong_cutoff.cutoff -= 1;
    wrong_cutoff.prefix_hash = hex::encode(Sha256::digest(
        &std::fs::read(&fixture.binding.source.path).unwrap()[..wrong_cutoff.cutoff as usize],
    ));
    let mut past_eof = valid.clone();
    past_eof.cutoff += 1;
    let mut wrong_opener = valid.clone();
    wrong_opener.open_after -= 1;
    let mut wrong_inode = valid.clone();
    wrong_inode.source.ino ^= 1;
    for end in [
        wrong_nonce,
        wrong_cutoff,
        past_eof,
        wrong_opener,
        wrong_inode,
    ] {
        let checked = crate::services::tui_o::writer::restart_end::validated(
            vec![end.clone()],
            &history,
            &fixture.binding,
        )
        .unwrap();
        let mut facts = InputFacts::open(fixture.binding.clone()).unwrap();
        facts.set_restart_ends(checked);
        assert_eq!(open_id(&mut facts).as_deref(), Some("old"), "{end:?}");
        assert!(!facts.restart_ended(), "{end:?}");
        assert_eq!(facts.last_close_aborted(), None);
    }
}

#[test]
fn physical_end_replays_after_typed_block_without_fabricating_native_close() {
    let blocked = json!({"type":"user","uuid":"blocked","message":{"content":[{"type":"tool_result","content":"unkeyed"}]}});
    let fixture = Fixture::new(8_109_220_030, &[prompt("old"), blocked]);
    let open_after = format!("{}\n", prompt("old")).len() as u64;
    let cutoff = std::fs::metadata(&fixture.binding.source.path)
        .unwrap()
        .len();
    fixture.store_end(&fixture.end("old", open_after, cutoff));
    let watch = fixture.boot();
    let (answer, stamp) = fixture.ask(&watch);
    assert_eq!(answer, observed(Activity::Idle, "effort_restart_ended"));
    assert!(stamp.unwrap().closed.is_err());
    append(
        &fixture.binding.source.path,
        &[prompt("next"), native_end()],
    );
    let (answer, stamp) = fixture.ask(&fixture.boot());
    assert_eq!(answer, observed(Activity::Idle, "closed"));
    assert!(!stamp.unwrap().closed.unwrap().aborted);
}

struct Scripted {
    history: Vec<BindingEvent>,
    ends: Mutex<Vec<RestartEnd>>,
    version: AtomicU64,
    reads: AtomicUsize,
    fail_reads: AtomicBool,
    jobs: Mutex<Vec<Job>>,
}

impl Ports for Scripted {
    fn binding_seq(&self, _: u64) -> Result<u64, String> {
        Ok(1)
    }
    fn binding_events(&self, _: u64, _: ShadowProvider) -> Result<Vec<BindingEvent>, String> {
        Ok(self.history.clone())
    }
    fn session_present(&self, _: &str) -> bool {
        true
    }
    fn final_ready(&self, _: &str) -> bool {
        false
    }
    fn pane_busy(&self, _: &str) -> bool {
        false
    }
    fn liveness(&self, _: ShadowProvider, _: u64, _: &str) -> SessionLiveness {
        SessionLiveness::Alive
    }
    fn spawn(&self, job: Job) -> bool {
        lock(&self.jobs).push(job);
        true
    }
    fn restart_version(&self, _: u64) -> u64 {
        self.version.load(Ordering::SeqCst)
    }
    fn restart_ends(&self, _: u64) -> Result<Vec<RestartEnd>, String> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        if self.fail_reads.load(Ordering::SeqCst) {
            return Err("transient restart witness read failure".into());
        }
        Ok(lock(&self.ends).clone())
    }
}

impl Scripted {
    fn new(fixture: &Fixture, ends: Vec<RestartEnd>) -> Arc<Self> {
        Arc::new(Self {
            history: fixture
                .live()
                .binding_events(fixture.key.channel, fixture.key.provider)
                .unwrap(),
            ends: Mutex::new(ends),
            version: AtomicU64::new(0),
            reads: AtomicUsize::new(0),
            fail_reads: AtomicBool::new(false),
            jobs: Mutex::new(Vec::new()),
        })
    }

    fn run(&self) {
        let jobs = std::mem::take(&mut *lock(&self.jobs));
        assert_eq!(jobs.len(), 1);
        jobs.into_iter().for_each(|job| job());
    }
}

#[test]
fn new_witness_is_loaded_only_on_rebuild_worker() {
    let fixture = Fixture::new(8_109_220_040, &[prompt("old")]);
    let cutoff = std::fs::metadata(&fixture.binding.source.path)
        .unwrap()
        .len();
    let scripted = Arc::new(Scripted {
        history: fixture
            .live()
            .binding_events(fixture.key.channel, fixture.key.provider)
            .unwrap(),
        ends: Mutex::new(Vec::new()),
        version: AtomicU64::new(0),
        reads: AtomicUsize::new(0),
        fail_reads: AtomicBool::new(false),
        jobs: Mutex::new(Vec::new()),
    });
    let ports: Arc<dyn Ports> = scripted.clone();
    let watch = Arc::new(Mutex::new(Watch::default()));
    let ask = || {
        judge(
            &watch,
            &ports,
            fixture.key.provider,
            fixture.key.channel,
            Target::Bound(LABEL.into()),
        )
    };
    assert_eq!(ask().0, observed(Activity::Unknown, "catching_up"));
    assert_eq!(scripted.reads.load(Ordering::SeqCst), 0);
    scripted.run();
    assert_eq!(ask().0, observed(Activity::Busy, "open"));
    lock(&watch).grew_at = Instant::now() - STALE_OPEN_AFTER - Duration::from_secs(1);
    assert_eq!(
        ask().0,
        observed(Activity::Unknown, "open_without_progress")
    );
    lock(&scripted.ends).push(fixture.end("old", cutoff, cutoff));
    scripted.version.fetch_add(1, Ordering::SeqCst);
    let before = scripted.reads.load(Ordering::SeqCst);
    assert_eq!(ask().0, observed(Activity::Unknown, "catching_up"));
    assert_eq!(scripted.reads.load(Ordering::SeqCst), before);
    scripted.run();
    assert_eq!(scripted.reads.load(Ordering::SeqCst), before + 1);
    assert_eq!(ask().0, observed(Activity::Idle, "effort_restart_ended"));
}

#[test]
fn physical_idle_rechecks_changed_prefix_on_worker() {
    let fixture = Fixture::new(
        8_109_220_060,
        &[
            prompt("old"),
            json!({"type":"summary","summary":"z".repeat(8192)}),
        ],
    );
    let open_after = format!("{}\n", prompt("old")).len() as u64;
    let cutoff = std::fs::metadata(&fixture.binding.source.path)
        .unwrap()
        .len();
    let scripted = Arc::new(Scripted {
        history: fixture
            .live()
            .binding_events(fixture.key.channel, fixture.key.provider)
            .unwrap(),
        ends: Mutex::new(vec![fixture.end("old", open_after, cutoff)]),
        version: AtomicU64::new(0),
        reads: AtomicUsize::new(0),
        fail_reads: AtomicBool::new(false),
        jobs: Mutex::new(Vec::new()),
    });
    let ports: Arc<dyn Ports> = scripted.clone();
    let watch = Arc::new(Mutex::new(Watch::default()));
    let ask = || {
        judge(
            &watch,
            &ports,
            fixture.key.provider,
            fixture.key.channel,
            Target::Bound(LABEL.into()),
        )
    };
    assert_eq!(ask().0, observed(Activity::Unknown, "catching_up"));
    scripted.run();
    assert_eq!(ask().0, observed(Activity::Idle, "effort_restart_ended"));
    assert!(closed_at_eof(&lock(&watch)));

    let bytes = std::fs::read_to_string(&fixture.binding.source.path).unwrap();
    assert!(bytes.contains("\"uuid\":\"old\""));
    let rewritten = bytes.replacen("\"uuid\":\"old\"", "\"uuid\":\"new\"", 1);
    assert_eq!(rewritten.len(), bytes.len());
    std::fs::write(&fixture.binding.source.path, rewritten).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&fixture.binding.source.path)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(1))
        .unwrap();
    assert!(!closed_at_eof(&lock(&watch)));
    let before = scripted.reads.load(Ordering::SeqCst);
    assert_eq!(ask().0, observed(Activity::Unknown, "catching_up"));
    assert_eq!(scripted.reads.load(Ordering::SeqCst), before);
    scripted.run();
    assert_eq!(scripted.reads.load(Ordering::SeqCst), before + 1);
    assert_eq!(ask().0, observed(Activity::Busy, "open"));
}

#[test]
fn transient_witness_read_reuses_the_existing_worker_retry() {
    let fixture = Fixture::new(8_109_220_070, &[prompt("old")]);
    let cutoff = std::fs::metadata(&fixture.binding.source.path)
        .unwrap()
        .len();
    let scripted = Scripted::new(&fixture, vec![fixture.end("old", cutoff, cutoff)]);
    scripted.fail_reads.store(true, Ordering::SeqCst);
    let ports: Arc<dyn Ports> = scripted.clone();
    let watch = Arc::new(Mutex::new(Watch::default()));
    let ask = || {
        judge(
            &watch,
            &ports,
            fixture.key.provider,
            fixture.key.channel,
            Target::Bound(LABEL.into()),
        )
    };
    assert_eq!(ask().0, observed(Activity::Unknown, "catching_up"));
    scripted.run();
    assert_eq!(ask().0, observed(Activity::Unknown, "binding_unreadable"));
    assert_eq!(scripted.reads.load(Ordering::SeqCst), 1);
    scripted.fail_reads.store(false, Ordering::SeqCst);
    match &mut lock(&watch).outcome {
        Outcome::Unreadable { retry_at } => *retry_at = Instant::now(),
        _ => panic!("witness I/O must retain the existing retry"),
    }
    assert_eq!(ask().0, observed(Activity::Unknown, "catching_up"));
    scripted.run();
    assert_eq!(ask().0, observed(Activity::Idle, "effort_restart_ended"));
    assert_eq!(scripted.reads.load(Ordering::SeqCst), 2);
}

#[test]
fn durable_end_survives_a_volume_dev_renumber() {
    let fixture = Fixture::new(8_109_220_080, &[prompt("old")]);
    let cutoff = std::fs::metadata(&fixture.binding.source.path)
        .unwrap()
        .len();
    let end = fixture.end("old", cutoff, cutoff);
    fixture.store_end(&end);
    let history = fixture
        .live()
        .binding_events(fixture.key.channel, fixture.key.provider)
        .unwrap();
    let shift = 1 << 40;
    let _reboot = crate::services::tui_o::shadow::capture::renumber::shift(
        &fixture.binding.source.path,
        shift,
    );
    for refreshed in [false, true] {
        let mut binding = fixture.binding.clone();
        if refreshed {
            binding.source.dev ^= shift;
        }
        let checked = crate::services::tui_o::writer::restart_end::validated(
            vec![end.clone()],
            &history,
            &binding,
        )
        .unwrap();
        assert_eq!(checked.len(), 1);
        assert_eq!(checked[0].source, binding.source);
        let mut facts = InputFacts::open(binding).unwrap();
        facts.set_restart_ends(checked);
        assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Idle);
        assert!(facts.restart_ended());
        assert_eq!(facts.last_close_aborted(), None);
    }
    assert_eq!(
        fixture.ask(&fixture.boot()).0,
        observed(Activity::Idle, "effort_restart_ended")
    );
}

#[test]
fn mid_offset_unknown_does_not_borrow_an_unseen_opener() {
    let fixture = Fixture::new(8_109_220_050, &[prompt("old")]);
    let open_after = std::fs::metadata(&fixture.binding.source.path)
        .unwrap()
        .len();
    let cutoff = append(
        &fixture.binding.source.path,
        &[json!({"type":"summary","summary":"metadata"})],
    );
    let end = fixture.end("old", open_after, cutoff);
    let history = fixture
        .live()
        .binding_events(fixture.key.channel, fixture.key.provider)
        .unwrap();
    let checked = crate::services::tui_o::writer::restart_end::validated(
        vec![end],
        &history,
        &fixture.binding,
    )
    .unwrap();
    let mut facts = InputFacts::open_at(fixture.binding.clone(), open_after).unwrap();
    facts.set_restart_ends(checked.clone());
    assert_eq!(facts.poll(u64::MAX).unwrap().state, TurnState::Unknown);
    assert!(!facts.restart_ended());
    append(&fixture.binding.source.path, &[prompt("new")]);
    let mut after = InputFacts::open_at(fixture.binding.clone(), cutoff).unwrap();
    after.set_restart_ends(checked.clone());
    assert_eq!(open_id(&mut after).as_deref(), Some("new"));
    assert!(!after.restart_ended());
    let end = std::fs::metadata(&fixture.binding.source.path)
        .unwrap()
        .len();
    let mut past = InputFacts::open_at(fixture.binding.clone(), end).unwrap();
    past.set_restart_ends(checked);
    assert_eq!(past.poll(u64::MAX).unwrap().state, TurnState::Unknown);
    assert!(!past.restart_ended());
}

#[test]
fn validated_prefix_cannot_change_before_first_poll() {
    let fixture = Fixture::new(8_109_220_090, &[prompt("old")]);
    let cutoff = std::fs::metadata(&fixture.binding.source.path)
        .unwrap()
        .len();
    let end = fixture.end("old", cutoff, cutoff);
    let history = fixture
        .live()
        .binding_events(fixture.key.channel, fixture.key.provider)
        .unwrap();
    let checked = crate::services::tui_o::writer::restart_end::validated(
        vec![end],
        &history,
        &fixture.binding,
    )
    .unwrap();
    assert_eq!(checked.len(), 1);
    let mut facts = InputFacts::open(fixture.binding.clone()).unwrap();
    facts.set_restart_ends(checked);

    let bytes = std::fs::read_to_string(&fixture.binding.source.path).unwrap();
    let rewritten = bytes.replacen("work", "edit", 1);
    assert_ne!(bytes, rewritten);
    assert_eq!(bytes.len(), rewritten.len());
    std::fs::write(&fixture.binding.source.path, rewritten).unwrap();
    assert_eq!(open_id(&mut facts).as_deref(), Some("old"));
    assert!(!facts.restart_ended());
    assert_eq!(facts.last_close_aborted(), None);
    assert!(
        !facts
            .events()
            .iter()
            .any(|event| matches!(event, Ordered::Closed { .. }))
    );
}

#[test]
fn validated_prefix_cannot_change_between_polls() {
    let records = [
        prompt("old"),
        json!({"type":"summary","summary":"z".repeat(8192)}),
        json!({"type":"summary","summary":"suffix"}),
    ];
    let fixture = Fixture::new(8_109_220_100, &records);
    let open_after = format!("{}\n", records[0]).len() as u64;
    let first = open_after + format!("{}\n", records[1]).len() as u64;
    let cutoff = std::fs::metadata(&fixture.binding.source.path)
        .unwrap()
        .len();
    fixture.store_end(&fixture.end("old", open_after, cutoff));
    let mut facts = prepare(
        InputFacts::open(fixture.binding.clone()),
        &fixture.live(),
        &fixture.key,
    )
    .unwrap();
    assert_eq!(facts.poll(first).unwrap().through, first);
    assert!(!facts.restart_ended());
    assert!(!facts.restart_needs_reload());

    let bytes = std::fs::read_to_string(&fixture.binding.source.path).unwrap();
    let rewritten = bytes.replacen("work", "edit", 1);
    assert_ne!(bytes, rewritten);
    assert_eq!(bytes.len(), rewritten.len());
    std::fs::write(&fixture.binding.source.path, rewritten).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&fixture.binding.source.path)
        .unwrap()
        .set_modified(SystemTime::now() + Duration::from_secs(1))
        .unwrap();
    assert_eq!(open_id(&mut facts).as_deref(), Some("old"));
    assert!(!facts.restart_ended());
    assert!(facts.restart_needs_reload());
    assert_eq!(facts.last_close_aborted(), None);
}

#[test]
fn append_after_validation_reloads_on_the_existing_worker() {
    let fixture = Fixture::new(8_109_220_110, &[prompt("old")]);
    let cutoff = std::fs::metadata(&fixture.binding.source.path)
        .unwrap()
        .len();
    let end = fixture.end("old", cutoff, cutoff);
    fixture.store_end(&end);
    let mut facts = prepare(
        InputFacts::open(fixture.binding.clone()),
        &fixture.live(),
        &fixture.key,
    )
    .unwrap();
    append(
        &fixture.binding.source.path,
        &[json!({"type":"summary","summary":"appended"})],
    );
    assert_eq!(open_id(&mut facts).as_deref(), Some("old"));
    assert!(facts.restart_needs_reload());
    assert!(!facts.restart_ended());

    let scripted = Scripted::new(&fixture, vec![end]);
    let ports: Arc<dyn Ports> = scripted.clone();
    let watch = Arc::new(Mutex::new(Watch {
        key: Some(fixture.key.clone()),
        outcome: Outcome::Source {
            facts: Box::new(facts),
            from_start: true,
        },
        ..Watch::default()
    }));
    let ask = || {
        judge(
            &watch,
            &ports,
            fixture.key.provider,
            fixture.key.channel,
            Target::Bound(LABEL.into()),
        )
    };
    assert_eq!(ask().0, observed(Activity::Unknown, "catching_up"));
    assert_eq!(scripted.reads.load(Ordering::SeqCst), 0);
    scripted.run();
    assert_eq!(scripted.reads.load(Ordering::SeqCst), 1);
    assert_eq!(ask().0, observed(Activity::Idle, "effort_restart_ended"));
}
