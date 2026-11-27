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
