//! A confirmed `!clear` end proves the hop of the fresh launch that follows it, so O retires the
//! old source through its usual gates; any record that does not name exactly that hop proves none.

use std::time::{Duration, SystemTime};

use super::*;
use crate::services::tui_o::store::clear_end::{self, ClearEnd};
use crate::services::tui_o::writer::ReaderConfig;
use crate::services::tui_o::writer::rotation::{RETIRE_QUIET, Sources, probe};
use crate::services::tui_prompt_dedupe::binding_context::{
    BindingContext, PreparedIncarnation, stable_host_identity,
};

#[path = "clear_end_empty_session_tests.rs"]
mod empty_session;

/// The runtime root execution contexts are read under, held with the env lock it needs.
struct Contexts {
    _env: (tempfile::TempDir, [crate::config::TestEnvVarGuard; 2]),
    _lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
}

fn contexts() -> Contexts {
    let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let env =
        crate::services::tui_prompt_dedupe::binding_context::tests::fixture_after_shared_test_env_lock();
    Contexts {
        _env: env,
        _lock: lock,
    }
}

fn nonce() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Publishes execution `nonce`'s launch context for pane "tmux" of the channel.
fn launched(nonce: &str, expected: &str, mode: &str) {
    PreparedIncarnation::create(BindingContext {
        schema: 1,
        provider: "claude".into(),
        created_at: Utc::now(),
        execution_nonce: nonce.into(),
        tmux_session: "tmux".into(),
        channel_id: Some(CHANNEL),
        owner_runtime_root: String::new(),
        host: None,
        expected_native_session_id: Some(expected.into()),
        launch_mode: mode.into(),
        provider_root: None,
        first_prompt_digest: None,
        source_policy: None,
    })
    .unwrap();
}

/// A bind of `new` in pane "tmux" by execution `nonce` that no provider hook made.
fn hop(seq: u64, old: &SourceId, new: &SourceId, cause: BindingCause, nonce: &str) -> BindingEvent {
    let mut event = hop_at(seq, "tmux", Some(old), new, cause, "");
    event.execution_nonce = nonce.into();
    event
}

fn clear_end(source: &SourceId, ended: &str, binding_seq: u64) -> ClearEnd {
    ClearEnd {
        channel: CHANNEL,
        provider: "claude".into(),
        tmux_session: "tmux".into(),
        execution_nonce: ended.into(),
        source: source.clone(),
        binding_seq,
        host: stable_host_identity(),
        ended_at: Utc::now(),
        basis: "confirmed_clear".into(),
    }
}

/// Publishes `record` under the runtime root the transcripts beside `path` share with the store.
fn publish(path: &Path, record: ClearEnd) {
    clear_end::append(path.parent().unwrap(), record).unwrap();
}

/// A clear of execution `ended` writing `old`, then a fresh launch binding a new transcript at
/// `seq`; returns the new source and the launch's execution.
fn clear_and_launch(
    bindings: &FakeBindings,
    (old_path, old, ended): (&Path, &SourceId, &str),
    seq: u64,
) -> (SourceId, String) {
    publish(old_path, clear_end(old, ended, seq - 1));
    let (name, session) = (format!("h{seq}"), format!("s{seq}"));
    let file = format!("{name}.jsonl");
    let (_, new) = transcript(old_path, &file, &session, &row(&name, &name));
    let launch = nonce();
    launched(&launch, &session, "fresh");
    bindings.commit(hop(seq, old, &new, BindingCause::Startup, &launch));
    (new, launch)
}

fn engine(harness: &Harness, bindings: Arc<FakeBindings>, at: SystemTime) -> ReaderEngine {
    let policy = Some(ReaderConfig { dormant: true });
    reader_engine_at(harness, bindings, policy, at)
}

/// Waits for the started prefix validation without consuming it.
async fn validated(sources: &Sources<FakeBindings>) {
    for _ in 0..1000 {
        if probe::ready(sources) {
            return;
        }
        tokio::task::yield_now().await;
        std::thread::sleep(Duration::from_millis(1));
    }
    panic!("prefix validation did not finish");
}

/// Tends once, and once more after any validation it started finished.
async fn tend_validated(sources: &mut Sources<FakeBindings>, writer: &mut Writer, at: SystemTime) {
    sources.tend_at(writer, at).unwrap();
    if probe::validating(sources) {
        validated(sources).await;
        sources.tend_at(writer, at).unwrap();
    }
}

fn too_many(alarm: &WriterAlarm) -> bool {
    matches!(alarm, WriterAlarm::TooManyReaders { .. })
}

#[tokio::test(start_paused = true)]
async fn a_confirmed_clear_end_proves_the_fresh_hop_and_the_old_source_retires() {
    let _contexts = contexts();
    let (harness, a_path, a, bindings) = started(&row("m0", "before the clear"));
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    polls(3).await;
    clear_and_launch(&bindings, (&a_path, &a, "nonce"), 2);
    polls(3).await;
    assert_eq!(successor(&harness, &a).proof, Some(2));
    polls(12).await;
    assert!(retired(&harness, &a), "the proven old source retires");
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn clear_ends_that_do_not_name_the_fresh_hop_prove_nothing() {
    let _contexts = contexts();
    let cases = [
        "control",
        "resumed_then_compacted",
        "no_context",
        "other_expected",
        "resume_mode",
        "other_pane",
        "other_source",
        "same_nonce",
        "rebound_first",
        "codex",
        "legacy",
    ];
    for case in cases {
        let (harness, a_path, a, bindings) = started(&row("m0", "before the clear"));
        let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "after the clear"));
        let mut record = clear_end(&a, "ended", 1);
        let launch = nonce();
        let (mode, expected) = match case {
            // The next launch resumed the old session, which logs no bind; its compact moved on.
            "resumed_then_compacted" => ("resume", "s1"),
            "other_expected" => ("fresh", "s9"),
            "resume_mode" => ("resume", "s2"),
            _ => ("fresh", "s2"),
        };
        if case != "no_context" {
            launched(&launch, expected, mode);
        }
        if case == "same_nonce" {
            record.execution_nonce = launch.clone();
        }
        match case {
            "other_pane" => record.tmux_session = "other".into(),
            "other_source" => record.source = transcript(&a_path, "c.jsonl", "s1", b"").1,
            _ => {}
        }
        let cause = match case {
            "resumed_then_compacted" => BindingCause::Compact,
            _ => BindingCause::Startup,
        };
        let seq = if case == "rebound_first" {
            bindings.commit(resumed(2, "tmux", &a));
            3
        } else {
            2
        };
        bindings.commit(hop(seq, &a, &b, cause, &launch));
        let provider = match case {
            "codex" => ShadowProvider::Codex,
            _ => ShadowProvider::Claude,
        };
        let mut writer = harness.writer();
        let mut sources = Sources::new(CHANNEL, provider, bindings.clone());
        let (mut deriver, mut owed) = (UnitDeriver::new(CHANNEL, provider), VecDeque::new());
        sources
            .resume(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        if case == "legacy" {
            // A successor stored before hops kept their seq and pane.
            sources.follow(&mut writer).unwrap();
            let mut rotation = writer.store().rotation().unwrap();
            rotation
                .successors
                .insert(source_key(&a), Successor::from(b.clone()));
            writer.store().write_rotation(&rotation).unwrap();
            drop((sources, writer));
            writer = harness.writer();
            sources = Sources::new(CHANNEL, provider, bindings.clone());
            sources
                .resume(&mut writer, &mut deriver, &mut owed)
                .unwrap();
        }
        publish(&a_path, record);
        sources.follow(&mut writer).unwrap();
        let proof = writer.store().rotation().unwrap().successors[&source_key(&a)].proof;
        let wanted = (case == "control").then_some(seq);
        assert_eq!(proof, wanted, "{case}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_late_clear_end_retires_the_old_source_only_on_the_tend_consuming_its_validation() {
    let _contexts = contexts();
    let (harness, a_path, a, bindings) = started(&row("m0", "before the clear"));
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "after the clear"));
    let launch = nonce();
    launched(&launch, "s2", "fresh");
    bindings.commit(hop(2, &a, &b, BindingCause::Startup, &launch));
    let t0 = SystemTime::now();
    let (mut sources, mut writer, mut deriver, mut owed) = engine(&harness, bindings, t0);
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    tend_validated(&mut sources, &mut writer, t0).await;
    assert_eq!(successor(&harness, &a).proof, None);
    publish(&a_path, clear_end(&a, "nonce", 1));
    sources.follow(&mut writer).unwrap();
    assert_eq!(successor(&harness, &a).proof, Some(2), "the record proves");
    sources.tend_at(&mut writer, t0).unwrap();
    tokio::time::advance(RETIRE_QUIET + Duration::from_secs(1)).await;
    let quiet = t0 + RETIRE_QUIET + Duration::from_secs(1);
    sources.tend_at(&mut writer, quiet).unwrap();
    assert!(
        !retired(&harness, &a) && probe::validating(&sources),
        "the first quiet tend only starts the prefix validation"
    );
    validated(&sources).await;
    assert!(
        !retired(&harness, &a),
        "a finished validation alone retires nothing"
    );
    sources.tend_at(&mut writer, quiet).unwrap();
    assert!(
        retired(&harness, &a),
        "the tend consuming the validation retires"
    );
    assert!(!probe::reading(&sources, &a), "and closes the old FD");
    assert_eq!(harness.alarms.readers(), Some(1));
}

#[tokio::test(start_paused = true)]
async fn spaced_confirmed_clears_never_exceed_three_readers() {
    let _contexts = contexts();
    let (harness, a_path, a, bindings) = started(&row("m0", "before the clears"));
    let t0 = SystemTime::now();
    let (mut sources, mut writer, mut deriver, mut owed) = engine(&harness, bindings.clone(), t0);
    let (mut old, mut ended, mut at) = (a, "nonce".to_owned(), t0);
    for seq in 2..=4 {
        let (new, launch) = clear_and_launch(&bindings, (&a_path, &old, &ended), seq);
        sources.follow(&mut writer).unwrap();
        sources
            .capture(&mut writer, &mut deriver, &mut owed)
            .unwrap();
        for _ in 0..3 {
            tend_validated(&mut sources, &mut writer, at).await;
            tokio::time::advance(RETIRE_QUIET + Duration::from_secs(1)).await;
            at += RETIRE_QUIET + Duration::from_secs(1);
        }
        tend_validated(&mut sources, &mut writer, at).await;
        assert!(
            retired(&harness, &old),
            "clear {seq} retires its old source"
        );
        assert!(harness.alarms.readers() <= Some(3), "clear {seq}");
        (old, ended) = (new, launch);
    }
    assert!(!harness.alarms.taken().iter().any(too_many));
}

#[tokio::test(start_paused = true)]
async fn burst_confirmed_clears_return_within_three_readers_once_validated() {
    let _contexts = contexts();
    let (harness, a_path, a, bindings) = started(&row("m0", "before the clears"));
    let (mut old, mut ended) = (a, "nonce".to_owned());
    for seq in 2..=4 {
        let (new, launch) = clear_and_launch(&bindings, (&a_path, &old, &ended), seq);
        (old, ended) = (new, launch);
    }
    let t0 = SystemTime::now();
    let (mut sources, mut writer, mut deriver, mut owed) = engine(&harness, bindings, t0);
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    // Four readers at once may alarm; the limit holds again once a proven one retires.
    tend_validated(&mut sources, &mut writer, t0).await;
    tokio::time::advance(RETIRE_QUIET + Duration::from_secs(1)).await;
    let quiet = t0 + RETIRE_QUIET + Duration::from_secs(1);
    for _ in 0..3 {
        tend_validated(&mut sources, &mut writer, quiet).await;
    }
    assert!(
        harness.alarms.readers() <= Some(3),
        "{:?}",
        harness.alarms.readers()
    );
}

#[tokio::test(start_paused = true)]
async fn clear_ends_published_while_o_was_down_prove_on_resume_and_wake_the_old_reader() {
    let _contexts = contexts();
    let (harness, a_path, a, bindings) = started(&row("m0", "before the clear"));
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "after the clear"));
    let launch = nonce();
    launched(&launch, "s2", "fresh");
    bindings.commit(hop(2, &a, &b, BindingCause::Startup, &launch));
    let t0 = SystemTime::now();
    let (mut sources, mut writer, mut deriver, mut owed) = engine(&harness, bindings.clone(), t0);
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    tend_validated(&mut sources, &mut writer, t0).await;
    drop((sources, writer, deriver, owed));
    publish(&a_path, clear_end(&a, "nonce", 1));
    let (sources, ..) = engine(&harness, bindings.clone(), t0);
    assert_eq!(
        successor(&harness, &a).proof,
        Some(2),
        "resume applies the record"
    );
    assert!(
        probe::reading(&sources, &a),
        "the proof wakes the reader boot put to sleep"
    );
    drop(sources);
    let (sources, ..) = engine(&harness, bindings, t0);
    assert!(
        probe::reading(&sources, &a),
        "a persisted proof wakes it again"
    );
}

#[tokio::test(start_paused = true)]
async fn a_clear_end_for_a_vanished_old_source_leaves_it_to_missing_cleanup() {
    let _contexts = contexts();
    let (harness, a_path, a, bindings) = started(&row("m0", "before the clear"));
    let t0 = SystemTime::now();
    let old = std::fs::File::open(&a_path).unwrap();
    old.set_modified(t0 - Duration::from_secs(48 * 3600))
        .unwrap();
    let (_, b) = transcript(&a_path, "b.jsonl", "s2", &row("n1", "after the clear"));
    let launch = nonce();
    launched(&launch, "s2", "fresh");
    bindings.commit(hop(2, &a, &b, BindingCause::Startup, &launch));
    let (mut sources, mut writer, mut deriver, mut owed) = engine(&harness, bindings, t0);
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    tend_validated(&mut sources, &mut writer, t0).await;
    std::fs::remove_file(&a_path).unwrap();
    publish(&a_path, clear_end(&a, "nonce", 1));
    sources.follow(&mut writer).unwrap();
    assert_eq!(
        successor(&harness, &a).proof,
        None,
        "a missing old source is not proven"
    );
    let later = t0 + Duration::from_secs(60);
    tokio::time::advance(Duration::from_secs(60)).await;
    sources.tend_at(&mut writer, later).unwrap();
    assert!(
        retired(&harness, &a),
        "missing cleanup retires it without halting"
    );
}

#[tokio::test(start_paused = true)]
async fn a_proven_old_source_that_vanishes_halts_instead_of_retiring() {
    let _contexts = contexts();
    let (harness, a_path, a, bindings) = started(&row("m0", "before the clear"));
    let t0 = SystemTime::now();
    let old = std::fs::File::open(&a_path).unwrap();
    old.set_modified(t0 - Duration::from_secs(48 * 3600))
        .unwrap();
    clear_and_launch(&bindings, (&a_path, &a, "nonce"), 2);
    let (mut sources, mut writer, mut deriver, mut owed) = engine(&harness, bindings, t0);
    assert_eq!(successor(&harness, &a).proof, Some(2));
    sources
        .capture(&mut writer, &mut deriver, &mut owed)
        .unwrap();
    tend_validated(&mut sources, &mut writer, t0).await;
    std::fs::remove_file(&a_path).unwrap();
    tokio::time::advance(Duration::from_secs(60)).await;
    let tended = sources.tend_at(&mut writer, t0 + Duration::from_secs(60));
    assert!(
        matches!(tended, Err(WriterAlarm::Halted { .. })),
        "{tended:?}"
    );
    assert!(
        !retired(&harness, &a),
        "a proven source is never retired unread"
    );
}
