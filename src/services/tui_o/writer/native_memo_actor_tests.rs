//! A publishing actor skips quiescent native offers without changing which records it posts.
use super::*;
use crate::services::tui_o::writer::actor::native_input::{
    EvidencePort, NativePort, Snapshot, spawn_native,
};
use crate::services::tui_o::writer::input_facts::native::Observation;
use crate::services::tui_o::writer::input_notices::NoticeOrigin;
use std::sync::atomic::{AtomicUsize, Ordering};

type Held<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;

/// A port with no registrations that counts the snapshots the actor asks for.
#[derive(Default)]
struct Unregistered {
    snapshots: AtomicUsize,
    registry_revision: std::sync::atomic::AtomicU64,
}
impl EvidencePort for Unregistered {
    fn snapshot(&self, _: NoticeOrigin) -> Held<'_, Result<Snapshot, String>> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        let revision = self.registry_revision.load(Ordering::SeqCst);
        let (generation, registrations) = (1, Vec::new());
        Box::pin(async move {
            Ok(Snapshot {
                generation,
                registrations,
                registry_revisions: vec![revision],
                has_open_barrier: false,
            })
        })
    }
    fn snapshot_since<'a>(
        &'a self,
        origin: NoticeOrigin,
        known: Option<&'a [u64]>,
    ) -> Held<'a, Result<Option<Snapshot>, String>> {
        Box::pin(async move {
            if known == Some(&[self.registry_revision.load(Ordering::SeqCst)]) {
                return Ok(None);
            }
            self.snapshot(origin).await.map(Some)
        })
    }
    fn offer(
        &self,
        _: Observation,
        _: Observation,
        _: usize,
        _: NoticeOrigin,
    ) -> Held<'_, Result<bool, String>> {
        Box::pin(async { Err("nothing is registered".into()) })
    }
    fn retry(&self) -> Held<'_, ()> {
        Box::pin(async {})
    }
}

fn spawn_on(harness: &Harness, port: &Arc<Unregistered>) -> (watch::Sender<bool>, Actor) {
    let mut writer = harness.writer();
    let bindings = startup_log(&mut writer);
    let (stop, stopped) = watch::channel(false);
    let config = crate::services::tui_o::writer::WriterConfig {
        enabled: true,
        producer_absent: false,
    };
    let projections = (
        stopped,
        watch::channel(false).0,
        watch::channel(None).0,
        crate::services::tui_o::writer::actor::Owing::default(),
    );
    let inner = port.clone() as Arc<dyn EvidencePort>;
    let native = Some(NativePort { inner });
    let provider = ShadowProvider::Claude;
    let task = spawn_native(
        &config,
        None,
        writer,
        provider,
        bindings,
        projections,
        native,
    );
    (stop, task.unwrap())
}

#[tokio::test(start_paused = true)]
async fn memo_hits_keep_every_post_exactly_once() {
    let (harness, path, _) = switched_over(&row("m0", "before the switch"));
    harness.gate.acquired();
    let port = Arc::new(Unregistered::default());
    let (stop, task) = spawn_on(&harness, &port);
    append(&path, &row("m1", "first"));
    polls(6).await;
    let after_first = port.snapshots.load(Ordering::SeqCst);
    polls(6).await;
    assert_eq!(port.snapshots.load(Ordering::SeqCst), after_first);
    port.registry_revision.fetch_add(1, Ordering::SeqCst);
    polls(6).await;
    let after_registry_change = port.snapshots.load(Ordering::SeqCst);
    assert!(after_registry_change > after_first);
    append(&path, &row("m2", "second"));
    polls(6).await;
    assert!(port.snapshots.load(Ordering::SeqCst) > after_registry_change);
    halt(stop, task).await;
    let (stop, task) = spawn_on(&harness, &port);
    append(&path, &row("m3", "third"));
    polls(6).await;
    halt(stop, task).await;
    assert_eq!(harness.port.posts(), ["first", "second", "third"]);
    assert_eq!(harness.alarms.taken(), []);
}

#[tokio::test(start_paused = true)]
async fn a_cut_source_is_not_observed_on_the_poll_that_halts_its_actor() {
    let (harness, path, _) = switched_over(&row("m0", "before the switch"));
    harness.gate.acquired();
    let port = Arc::new(Unregistered::default());
    let (_stop, task) = spawn_on(&harness, &port);
    append(&path, &row("m1", "first"));
    polls(6).await;
    let observed = port.snapshots.load(Ordering::SeqCst);
    let file = std::fs::File::options().write(true).open(&path).unwrap();
    file.set_len(1).unwrap();
    polls(3).await;
    assert!(task.is_finished(), "the cut halts the actor");
    assert_eq!(port.snapshots.load(Ordering::SeqCst), observed);
}

struct IdleBindings {
    log: FakeBindings,
    source_checks: AtomicUsize,
}

impl BindingEvents for IdleBindings {
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String> {
        self.log.binding_events_since(channel, after)
    }

    fn subscribe(&self, channel: u64) -> watch::Receiver<u64> {
        self.source_checks.fetch_add(1, Ordering::SeqCst);
        self.log.subscribe(channel)
    }
}

fn start_idle(
    channel: u64,
) -> (
    Harness,
    PathBuf,
    SourceId,
    Arc<IdleBindings>,
    watch::Sender<bool>,
    Actor,
) {
    let (harness, path, source) = switched_over_on(channel, &row("old", "already delivered"));
    harness.gate.acquired();
    let bindings = Arc::new(IdleBindings {
        log: FakeBindings::new(),
        source_checks: AtomicUsize::new(0),
    });
    let mut startup = bound(
        1,
        None,
        BindingTarget::Source(source.clone()),
        BindingCause::Startup,
        None,
    );
    startup.channel_id = channel;
    bindings.log.commit(startup);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    (harness, path, source, bindings, stop, task)
}

async fn idle_window() {
    polls(10).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
}

#[tokio::test(start_paused = true)]
async fn idle_tick_unchanged_source_skips_work_at_the_head_of_the_loop() {
    let (_harness, _, _, bindings, stop, task) = start_idle(838501);
    polls(2).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let before = bindings.source_checks.load(Ordering::SeqCst);
    assert!(before > 2, "startup reached the source observation path");
    polls(3).await;
    assert_eq!(bindings.source_checks.load(Ordering::SeqCst), before);
    halt(stop, task).await;
}

async fn restart_with_retired_source(
    channel: u64,
) -> (
    Harness,
    PathBuf,
    SourceId,
    Arc<IdleBindings>,
    watch::Sender<bool>,
    Actor,
) {
    let (harness, path, old, bindings, stop, task) = start_idle(channel);
    let next = path.with_file_name("current.jsonl");
    std::fs::write(&next, row("current", "current output")).unwrap();
    let next = source_id_for("current", &next).unwrap();
    let mut hop = bound(
        2,
        Some(&old),
        BindingTarget::Source(next.clone()),
        BindingCause::Clear,
        Some(&old),
    );
    hop.channel_id = channel;
    bindings.log.commit(hop);
    polls(16).await;
    halt(stop, task).await;
    assert!(path.exists());
    let store = harness.channel();
    assert!(store.cursor(&old).unwrap().retired);
    assert!(!store.cursor(&next).unwrap().retired);
    drop(store);
    let (stop, task) = spawn_with(harness.writer(), ShadowProvider::Claude, bindings.clone());
    idle_window().await;
    (harness, path, old, bindings, stop, task)
}

#[tokio::test(start_paused = true)]
async fn idle_tick_with_a_retired_source_skips_unchanged_observations_after_restart() {
    let (_harness, _, _, bindings, stop, task) = restart_with_retired_source(838509).await;
    let before = bindings.source_checks.load(Ordering::SeqCst);
    polls(3).await;
    assert_eq!(bindings.source_checks.load(Ordering::SeqCst), before);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn idle_tick_unretires_a_growing_source_at_the_next_observation() {
    let (harness, path, old, bindings, stop, task) = restart_with_retired_source(838510).await;
    let before = bindings.source_checks.load(Ordering::SeqCst);
    polls(3).await;
    let unchanged = bindings.source_checks.load(Ordering::SeqCst);
    append(&path, &row("regrowth", "late output"));
    polls(1).await;
    assert!(!harness.channel().cursor(&old).unwrap().retired);
    assert!(
        bindings.source_checks.load(Ordering::SeqCst) > unchanged,
        "regrowth reached the observation path"
    );
    assert_eq!(unchanged, before, "the previous unchanged ticks skipped");
    polls(3).await;
    assert_eq!(harness.port.posts(), ["current output", "late output"]);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn idle_tick_with_a_retired_codex_source_skips_until_the_turn_ends() {
    let channel = 838511;
    let provider = ShadowProvider::Codex;
    let (harness, path, old) = switched_over_on(channel, b"");
    let current = path.with_file_name("current.jsonl");
    std::fs::write(&current, b"").unwrap();
    let current_source = source_id_for("current", &current).unwrap();
    let mut store = harness.channel();
    store.attach_source(&current_source).unwrap();
    store.set_retired(&old, true).unwrap();
    store.set_binding_checkpoint(2).unwrap();
    drop(store);

    let bindings = Arc::new(IdleBindings {
        log: FakeBindings::new(),
        source_checks: AtomicUsize::new(0),
    });
    for (seq, source) in [(1, old), (2, current_source)] {
        let mut startup = bound(
            seq,
            None,
            BindingTarget::Source(source),
            BindingCause::Startup,
            None,
        );
        startup.channel_id = channel;
        startup.provider = provider;
        bindings.log.commit(startup);
    }
    harness.gate.acquired();
    let write = |record: serde_json::Value| append(&current, format!("{record}\n").as_bytes());
    let (stop, task) = spawn_with(harness.writer(), provider, bindings.clone());
    write(
        serde_json::json!({"type":"event_msg","timestamp":"2026-10-19T00:00:00Z","payload":{"type":"task_started","turn_id":"turn"}}),
    );
    write(
        serde_json::json!({"type":"response_item","payload":{"type":"message","role":"assistant","id":"answer","content":[{"type":"output_text","text":"answer"}]}}),
    );
    idle_window().await;
    halt(stop, task).await;
    assert_eq!(harness.port.posts(), ["answer"]);

    let (stop, task) = spawn_with(harness.writer(), provider, bindings.clone());
    idle_window().await;
    let before = bindings.source_checks.load(Ordering::SeqCst);
    polls(3).await;
    assert_eq!(bindings.source_checks.load(Ordering::SeqCst), before);
    write(
        serde_json::json!({"type":"event_msg","timestamp":"2026-10-19T00:00:07Z","payload":{"type":"task_complete","turn_id":"turn"}}),
    );
    polls(3).await;
    halt(stop, task).await;
    assert_eq!(harness.port.posts(), ["answer", "-# ⏱️ 7초 (09:00)"]);
}

#[tokio::test(start_paused = true)]
async fn idle_tick_hook_bind_wakes_and_posts_once_in_source_order() {
    let channel = 838502;
    let (harness, path, source, bindings, stop, task) = start_idle(channel);
    idle_window().await;
    append(&path, &row("tail", "old tail"));
    let next = path.with_file_name("next.jsonl");
    std::fs::write(&next, row("new", "new head")).unwrap();
    let next = source_id_for("next", &next).unwrap();
    let mut hop = bound(
        2,
        Some(&source),
        BindingTarget::Source(next),
        BindingCause::Clear,
        Some(&source),
    );
    hop.channel_id = channel;
    let before = bindings.source_checks.load(Ordering::SeqCst);
    bindings.log.commit(hop);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(bindings.source_checks.load(Ordering::SeqCst) > before);
    polls(16).await;
    assert_eq!(harness.port.posts(), ["old tail", "new head"]);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn idle_tick_durable_input_registration_wakes_the_waiting_actor() {
    use crate::services::tui_input::human_frame::history::v2::native_support::Fixture;

    let channel = 838503;
    let (_harness, _, _, bindings, stop, task) = start_idle(channel);
    idle_window().await;
    let before = bindings.source_checks.load(Ordering::SeqCst);
    {
        let root = crate::config::TestRuntimeRootGuard::new();
        Fixture::at(root.path().join("runtime/test-frames.json"), channel, true);
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(bindings.source_checks.load(Ordering::SeqCst) > before);
    halt(stop, task).await;
}

#[tokio::test(start_paused = true)]
async fn idle_tick_transcript_only_change_spools_and_posts_within_fifteen_seconds() {
    let (harness, path, source, bindings, stop, task) = start_idle(838504);
    polls(16).await;
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let before = bindings.source_checks.load(Ordering::SeqCst);
    append(&path, &row("first", "first"));
    append(&path, &row("second", "second"));
    polls(1).await;
    assert!(
        bindings.source_checks.load(Ordering::SeqCst) > before,
        "the next one-second poll observes a transcript-only change"
    );
    polls(1).await;
    assert_eq!(harness.port.posts(), ["first", "second"]);
    polls(16).await;
    assert_eq!(harness.port.posts(), ["first", "second"]);
    halt(stop, task).await;
    assert_eq!(
        harness.channel().cursor(&source).unwrap().captured_through,
        std::fs::metadata(path).unwrap().len()
    );
}

#[test]
fn idle_tick_spool_stamp_moves_only_when_capture_commits() {
    let body = row("old", "already delivered");
    let (harness, path, source) = switched_over_on(838505, &body);
    let mut store = harness.channel();
    let generation = store.spool_generation();
    store.attach_source(&source).unwrap();
    assert_eq!(store.spool_generation(), generation);
    let mut capture = SourceCapture::open(source, body.len() as u64).unwrap();
    append(&path, &row("new", "next output"));
    let CaptureOutcome::Batch(batch) = capture.poll(MAX_READ_BYTES) else {
        panic!("transcript append did not produce a batch");
    };
    store.append_spool(&batch, &capture.prefix_hash()).unwrap();
    assert_eq!(store.spool_generation(), generation + 1);
}

#[tokio::test(start_paused = true)]
async fn idle_tick_same_length_replacement_is_rejected_without_delivery() {
    let (harness, path, _, _, _stop, task) = start_idle(838506);
    idle_window().await;
    let replacement = path.with_file_name("replacement.jsonl");
    std::fs::copy(&path, &replacement).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&replacement)
        .unwrap()
        .set_modified(std::fs::metadata(&path).unwrap().modified().unwrap())
        .unwrap();
    std::fs::rename(replacement, path).unwrap();
    polls(15).await;
    assert!(
        task.is_finished(),
        "a replaced transcript must halt capture"
    );
    assert!(task.await.unwrap().is_some());
    assert!(harness.port.posts().is_empty());
}
