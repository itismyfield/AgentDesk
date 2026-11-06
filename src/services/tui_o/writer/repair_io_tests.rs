//! Failed final boot writes keep readiness and stale-halt clearing behind durability.
use super::*;
use crate::services::tui_o::alarm::{AlarmHealth, AlarmRouter};
use crate::services::tui_o::store::cleanup_witness::CleanupWitness;
use crate::services::tui_o::store::fault::{self, Keep, Step as FaultStep};
use crate::services::tui_o::store::rotation::{Rotation, Successor};
use crate::services::tui_o::store::spool::source_key;
use crate::services::tui_o::writer::actor::{Owing, spawn_projecting};
use crate::services::tui_o::writer::input_notices::admission::tests::bind as bind_notice;
use crate::services::tui_o::writer::pieces::UnitDeriver;
use crate::services::tui_o::writer::rotation::Sources;
use std::collections::BTreeMap;
use std::io::ErrorKind::StorageFull;
use std::time::{Duration, SystemTime};

fn child_case(name: &str) -> bool {
    const CHILD: &str = "ADK_TEST_REPAIR_IO_CHILD";
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return true;
    }
    let root = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name.split_once("::").unwrap().1, "--nocapture"])
        .env(CHILD, name)
        .env("AGENTDESK_ROOT_DIR", root.path())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("1 passed; 0 failed; 0 ignored;"),
        "{stdout}"
    );
    false
}

async fn until(reason: &str, predicate: impl Fn() -> bool) {
    for _ in 0..1500 {
        if predicate() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(predicate(), "{reason}");
}

fn channel_dir(h: &Harness) -> PathBuf {
    h._runtime.path().join("o_store").join(CHANNEL.to_string())
}

fn evidence(h: &Harness) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(path: &Path, found: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(path).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, found);
            } else {
                found.insert(path.clone(), std::fs::read(path).unwrap());
            }
        }
    }
    let mut found = BTreeMap::new();
    for dir in ["cursor", "spool"] {
        walk(&channel_dir(h).join(dir), &mut found);
    }
    found
}

fn health_io(h: &Harness) -> (Arc<TestIo>, Arc<AlarmHealth>) {
    let health = Arc::new(AlarmHealth::default());
    let mut io = TestIo::over(h);
    Arc::get_mut(&mut io).unwrap().alarms.2 =
        Some(Arc::new(AlarmRouter::new(None, None, health.clone())));
    (io, health)
}

fn reports(io: &TestIo) -> Vec<&'static str> {
    io.alarms
        .1
        .lock()
        .unwrap()
        .iter()
        .map(|(_, state)| *state)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn failed_notice_materialization_resume_keeps_halt_until_durable_recovery() {
    if !child_case(concat!(
        module_path!(),
        "::failed_notice_materialization_resume_keeps_halt_until_durable_recovery"
    )) {
        return;
    }
    let (h, path, source) = switched_over(b"");
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    let _confirmed = crate::services::tui_o::turn_mode::TestConfirmation::new(CHANNEL);
    let _bindings = bind_notice(
        h._runtime.path(),
        CHANNEL,
        &source,
        "repair-notice-pane",
        "73320000000000000000000000000001",
    );
    h.gate.acquired();
    let (io, health) = health_io(&h);
    let ready = Arc::new(Readiness::default());
    let tasks = hosted(&h, &io, true, &ready);
    until(
        "the initial notice-enabled host must boot successfully",
        || ready.accepts(CHANNEL),
    )
    .await;
    assert_eq!(
        std::mem::take(&mut *io.alarms.1.lock().unwrap()),
        [(CHANNEL, "cleared")],
        "separate exactly one successful initial boot clear"
    );
    assert!(h.channel().ledger().input_notices().migration.is_some());
    assert!(h.channel().ledger().input_notices().evidence.is_empty());
    let ledger = channel_dir(&h).join("ledger.jsonl");
    let full = fault::plant(&ledger, FaultStep::Append(Keep::Nothing), StorageFull, None);
    let prompt = "a retained native repair notice";
    let mut native = serde_json::to_vec(&serde_json::json!({"type":"user", "uuid":"repair-native-input", "message":{"role":"user", "content":prompt}})).unwrap();
    native.push(b'\n');
    append(&path, &native);
    until(
        "the live Materialized append must first halt on ENOSPC",
        || {
            !io.alarms.halted().is_empty()
                && !ready.accepts(CHANNEL)
                && reports(&io).contains(&"pending")
        },
    )
    .await;
    assert_eq!(
        h.channel().cursor(&source).unwrap().captured_through,
        std::fs::metadata(&path).unwrap().len(),
        "raw input must reach the durable spool before its notice evidence fails"
    );
    assert!(h.channel().ledger().input_notices().evidence.is_empty());
    let halted = format!("tui_o:halted:{CHANNEL}");
    let pending = format!("tui_o:resume_pending:{CHANNEL}");
    for sample in 0..350 {
        let active = health.current_at(std::time::Instant::now());
        assert!(
            active.contains(&halted) || active.contains(&pending),
            "every failed-recovery observation retains health: sample={sample}, active={active:?}"
        );
        assert!(
            !ready.accepts(CHANNEL),
            "a failed notice resume must never admit input"
        );
        let reports = reports(&io);
        assert!(
            !reports.contains(&"cleared"),
            "notice evidence failed after replay: clear must stay at zero, {reports:?}"
        );
        assert!(
            !reports.contains(&"settled"),
            "a failed replay must not publish a resumed success, {reports:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        io.alarms.halted().len() >= 2,
        "the fixture must include an actual failed in-process resume"
    );
    assert!(io.leases.lock().unwrap().len() >= 2);
    assert!(h.port.posts().is_empty());
    assert!(h.channel().ledger().input_notices().evidence.is_empty());
    drop(full);
    until(
        "space recovery must persist and post the single notice",
        || ready.accepts(CHANNEL) && h.port.posts().len() == 1,
    )
    .await;
    polls(5).await;
    let expected = crate::services::discord::input_notice_content(prompt).unwrap();
    assert_eq!(h.port.posts(), [expected]);
    assert_eq!(h.channel().ledger().input_notices().evidence.len(), 1);
    assert_eq!(
        reports(&io)
            .iter()
            .filter(|state| **state == "cleared")
            .count(),
        1
    );
    assert!(reports(&io).contains(&"settled"));
    abort(tasks);
    until(
        "the old writer must release its lease before restart",
        || Arc::strong_count(&h.lease) == 2,
    )
    .await;
    let (io, _) = health_io(&h);
    let ready = Arc::new(Readiness::default());
    let tasks = hosted(&h, &io, true, &ready);
    until("restart must recover the durable Posted notice", || {
        ready.accepts(CHANNEL)
    })
    .await;
    polls(5).await;
    assert_eq!(h.port.posts().len(), 1, "restart must add no notice POST");
    abort(tasks);
}

fn retained_pair() -> (Harness, SourceId, SourceId, Arc<FakeBindings>) {
    let (h, path, old) = switched_over(b"");
    append(
        &path,
        b"{\"type\":\"progress\",\"uuid\":\"repair-retained\"}\n",
    );
    let mut capture = SourceCapture::open(old.clone(), 0).unwrap();
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("retained record must be captured")
    };
    h.channel()
        .append_spool(&batch, &capture.prefix_hash())
        .unwrap();
    let path = h._runtime.path().join("repair-latest.jsonl");
    std::fs::write(&path, b"").unwrap();
    let latest = source_id_for("repair-latest", &path).unwrap();
    let mut store = h.channel();
    store.attach_source(&latest).unwrap();
    let mut rotation = Rotation::default();
    rotation.successors.insert(
        source_key(&old),
        Successor {
            source: latest.clone(),
            seq: Some(2),
            tmux_session: Some("tmux".into()),
            drain_to: Some(store.cursor(&old).unwrap().captured_through),
            proof: None,
        },
    );
    store.write_rotation(&rotation).unwrap();
    store.set_binding_checkpoint(2).unwrap();
    let bindings = Arc::new(FakeBindings::new());
    bindings.commit(bound(
        1,
        None,
        BindingTarget::Source(old.clone()),
        BindingCause::Startup,
        None,
    ));
    bindings.commit(bound(
        2,
        Some(&old),
        BindingTarget::Source(latest.clone()),
        BindingCause::Startup,
        None,
    ));
    h.gate.acquired();
    (h, old, latest, bindings)
}

#[tokio::test(start_paused = true)]
async fn cleanup_retire_io_failure_during_resume_never_publishes_ready() {
    if !child_case(concat!(
        module_path!(),
        "::cleanup_retire_io_failure_during_resume_never_publishes_ready"
    )) {
        return;
    }
    let (h, old, _, bindings) = retained_pair();
    let now = SystemTime::now();
    std::fs::File::open(&old.path)
        .unwrap()
        .set_modified(now - Duration::from_secs(48 * 3600))
        .unwrap();
    let cursor = h.channel().cursor(&old).unwrap().clone();
    let witness = CleanupWitness {
        source: old.clone(),
        captured_through: cursor.captured_through,
        prefix_hash: cursor.prefix_hash.clone(),
        fingerprint: crate::services::tui_o::shadow::capture::Fingerprint::path(&old).unwrap(),
        observed_at: now,
    };
    assert!(witness.eligible(now) && witness.bound_to(&cursor));
    h.channel().prepare_cleanup_witnesses().unwrap();
    h.channel().write_cleanup_witness(&witness).unwrap();
    std::fs::remove_file(&old.path).unwrap();
    let before = evidence(&h);
    assert!(
        before
            .values()
            .any(|bytes| bytes.windows(15).any(|part| part == b"repair-retained")),
        "nonempty spool evidence must be retained"
    );
    let alarms = Raised::default();
    let writer = ChannelWriter::new(
        h.channel(),
        h.gate.clone(),
        h.port.clone(),
        h.lease.clone(),
        alarms.clone(),
    );
    let (stop, stopped) = watch::channel(false);
    let (resumed, ready) = watch::channel(false);
    let config = WriterConfig {
        enabled: true,
        producer_absent: false,
    };
    let full = fault::plant(
        &channel_dir(&h).join("cursor"),
        FaultStep::Write,
        StorageFull,
        None,
    );
    let task = spawn_projecting(
        &config,
        Some(ReaderConfig { dormant: true }),
        writer,
        ShadowProvider::Claude,
        bindings,
        (stopped, resumed, watch::channel(None).0, Owing::default()),
    )
    .unwrap();
    until(
        "cleanup-retire storage failure must halt before readiness",
        || *ready.borrow() || task.is_finished(),
    )
    .await;
    assert!(
        !*ready.borrow(),
        "cleanup write failure must never publish resumed=true"
    );
    assert!(
        !alarms
            .1
            .lock()
            .unwrap()
            .iter()
            .any(|(_, state)| *state == "cleared")
    );
    assert!(
        task.is_finished(),
        "the original I/O failure must stop the actor"
    );
    let cause = task
        .await
        .unwrap()
        .expect("failed retirement must retain the original stop cause");
    assert_eq!(cause.io, Some(StorageFull));
    assert!(
        matches!(cause.alarm, WriterAlarm::Halted { ref detail } if detail.contains("cleanup retire")),
        "{:?}",
        cause.alarm
    );
    assert_eq!(evidence(&h), before);
    assert_eq!(h.channel().cursor(&old).unwrap(), &cursor);
    assert!(h.port.posts().is_empty());
    drop((stop, full));
}

#[tokio::test(start_paused = true)]
async fn truncated_binding_tail_does_not_park_a_missing_applied_current() {
    if !child_case(concat!(
        module_path!(),
        "::truncated_binding_tail_does_not_park_a_missing_applied_current"
    )) {
        return;
    }
    let (h, old, current, _) = retained_pair();
    let log = Arc::new(FakeBindings::new());
    log.commit(bound(
        1,
        None,
        BindingTarget::Source(old.clone()),
        BindingCause::Startup,
        None,
    ));
    std::fs::remove_file(&current.path).unwrap();
    let before = evidence(&h);
    let mut writer = h.writer();
    let mut sources = Sources::new(CHANNEL, ShadowProvider::Claude, log);
    let result = sources.resume(
        &mut writer,
        &mut UnitDeriver::new(CHANNEL, ShadowProvider::Claude),
        &mut VecDeque::new(),
    );
    let _selected = test_override::force_channels(&[(CHANNEL, ClaudeTui)]);
    p5_log(h._runtime.path(), CHANNEL, &startup(old));
    let (io, _) = health_io(&h);
    let ready = Arc::new(Readiness::default());
    let tasks = hosted(&h, &io, true, &ready);
    until(
        "shortened log must halt the missing durable current",
        || ready.accepts(CHANNEL) || !io.alarms.halted().is_empty(),
    )
    .await;
    assert!(
        result.is_err(),
        "checkpoint beyond tail makes current Unknown, requiring absence"
    );
    assert!(!sources.is_parked(&current));
    assert!(!ready.accepts(CHANNEL));
    assert!(!io.alarms.halted().is_empty());
    assert!(!reports(&io).contains(&"cleared"));
    assert_eq!(evidence(&h), before);
    abort(tasks);
}
