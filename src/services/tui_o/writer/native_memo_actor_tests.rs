//! A publishing actor observes its native source on every poll; the memo changes no post.
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
struct Unregistered(AtomicUsize);
impl EvidencePort for Unregistered {
    fn snapshot(&self, _: NoticeOrigin) -> Held<'_, Result<Snapshot, String>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let (generation, registrations) = (1, Vec::new());
        Box::pin(async move {
            Ok(Snapshot {
                generation,
                registrations,
            })
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

#[tokio::test(start_paused = true)]
async fn memo_hits_keep_every_post_exactly_once() {
    let (harness, path, _) = switched_over(&row("m0", "before the switch"));
    harness.gate.acquired();
    let port = Arc::new(Unregistered::default());
    let spawn = || {
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
    };
    let (stop, task) = spawn();
    append(&path, &row("m1", "first"));
    polls(6).await;
    append(&path, &row("m2", "second"));
    polls(6).await;
    halt(stop, task).await;
    let (stop, task) = spawn();
    append(&path, &row("m3", "third"));
    polls(6).await;
    halt(stop, task).await;
    // Three records changed the source; every other poll observed an input already reduced.
    assert!(port.0.load(Ordering::SeqCst) > 9);
    assert_eq!(harness.port.posts(), ["first", "second", "third"]);
    assert_eq!(harness.alarms.taken(), []);
}
