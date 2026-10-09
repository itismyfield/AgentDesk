//! A managed writer stops with its actor and admitted POST joined, and only then frees its channel
//! for the next writer; a supervisor runs one writer generation of a channel at a time.

use super::*;
use crate::services::cluster::home_supervisor::{
    Generation, HomeBundle, Phase, Refused, Settled, StopReason, Supervisor,
};
use crate::services::tui_o::writer::host::{ManagedWriterHandle, WriterStopped, start_managed};

fn managed(
    harness: &Harness,
    io: &Arc<TestIo>,
    ready: &Arc<Readiness>,
) -> Vec<ManagedWriterHandle> {
    p5::set_test_root(Some(harness._runtime.path()));
    let parts = || HostParts {
        io: Arc::clone(io),
        runtime_root: Some(root(harness)),
        gate: Arc::clone(&harness.gate),
        readiness: Arc::clone(ready),
    };
    start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership(),
        parts,
    )
}

/// A hosted channel's writer, its first piece posted.
async fn hosting() -> (
    Harness,
    PathBuf,
    Arc<TestIo>,
    Arc<Readiness>,
    ManagedWriterHandle,
) {
    let (harness, path) = fresh(startup);
    harness.gate.acquired();
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let mut writers = managed(&harness, &io, &ready);
    assert_eq!(writers.len(), 1);
    polls(3).await;
    append(&path, &row("m1", "first"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first"]);
    let writer = writers.pop().unwrap();
    (harness, path, io, ready, writer)
}

// A stopped writer posts nothing more and frees its channel: the next start hosts one writer,
// which delivers what arrived meanwhile exactly once.
#[tokio::test(start_paused = true)]
async fn a_stopped_writer_frees_its_channel_for_exactly_one_new_writer() {
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (harness, path, io, ready, writer) = hosting().await;
    assert!(
        managed(&harness, &io, &ready).is_empty(),
        "no second writer"
    );

    let stopped = writer.stop_and_join().await;
    assert_eq!(stopped, Ok(WriterStopped { channel: CHANNEL }));
    assert!(!ready.is_hosted(CHANNEL) && !ready.accepts(CHANNEL));
    append(&path, &row("m2", "second"));
    polls(3).await;
    assert_eq!(
        harness.port.posts(),
        ["first"],
        "nothing posts once stopped"
    );

    let again = managed(&harness, &io, &ready);
    assert_eq!(again.len(), 1);
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first", "second"]);
    assert!(ready.accepts(CHANNEL));
    for writer in again {
        writer.stop_and_join().await.unwrap();
    }
}

// The wait for a stop may be cancelled: the channel stays hosted while the admitted POST runs, no
// new writer starts beside the old actor, and it frees once that actor ended.
#[tokio::test(start_paused = true)]
async fn a_cancelled_stop_frees_the_channel_only_after_its_post_in_flight_settled() {
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (harness, path, io, ready, writer) = hosting().await;
    harness.port.lazy.store(true, Ordering::SeqCst);
    let (release, hold) = tokio::sync::oneshot::channel();
    *harness.port.hold.lock().unwrap() = Some(hold);
    append(&path, &row("m2", "held"));
    polls(2).await;
    assert_eq!(*harness.port.started.lock().unwrap(), ["http"]);

    let waited = tokio::time::timeout(POLL_INTERVAL, writer.stop_and_join()).await;
    assert!(waited.is_err(), "the stop waits for the POST in flight");
    polls(2).await;
    assert!(ready.is_hosted(CHANNEL), "hosted while its POST runs");
    assert!(!ready.accepts(CHANNEL), "but it takes no work");
    assert!(
        managed(&harness, &io, &ready).is_empty(),
        "no writer beside it"
    );

    release.send(()).unwrap();
    polls(2).await;
    assert!(!ready.is_hosted(CHANNEL), "freed once its actor ended");
    assert_eq!(harness.port.posts(), ["first", "held"]);
    let again = managed(&harness, &io, &ready);
    assert_eq!(again.len(), 1);
    for writer in again {
        writer.stop_and_join().await.unwrap();
    }
}

async fn scheduled_until(mut completed: impl FnMut() -> bool) {
    for _ in 0..100 {
        if completed() {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert!(completed(), "the scheduled transition did not finish");
}

#[tokio::test(start_paused = true)]
async fn an_unpolled_stop_waiter_still_stops_and_releases_its_writer() {
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (harness, path, io, ready, writer) = hosting().await;
    drop(writer.stop_and_join());
    scheduled_until(|| !ready.is_hosted(CHANNEL)).await;
    assert!(!ready.accepts(CHANNEL));
    append(&path, &row("m2", "after the stop"));
    tokio::time::advance(POLL_INTERVAL * 2).await;
    assert_eq!(harness.port.posts(), ["first"]);
    let again = managed(&harness, &io, &ready);
    assert_eq!(again.len(), 1);
    for writer in again {
        writer.stop_and_join().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn failed_preparation_does_not_reserve_a_channel_without_a_host() {
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (harness, _) = fresh(startup);
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let failed = start_managed(
        ShadowProvider::Claude,
        true,
        cutover::boot_ownership(),
        || HostParts {
            io: Arc::clone(&io),
            runtime_root: None,
            gate: Arc::clone(&harness.gate),
            readiness: Arc::clone(&ready),
        },
    );
    assert!(failed.is_empty());
    assert_eq!(io.alarms.halted().len(), 1);
    assert!(!ready.is_hosted(CHANNEL), "no host exists to release it");
    let retried = managed(&harness, &io, &ready);
    assert_eq!(retried.len(), 1, "a corrected preparation can retry");
    for writer in retried {
        writer.stop_and_join().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn a_normal_host_early_return_releases_its_unused_claim() {
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (harness, _) = fresh(startup);
    let era = harness._runtime.path().join("o_store/o_era");
    std::fs::write(&era, b"invalid era\n").unwrap();
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let ended = managed(&harness, &io, &ready);
    assert_eq!(ended.len(), 1);
    scheduled_until(|| !io.alarms.halted().is_empty()).await;
    scheduled_until(|| !ready.is_hosted(CHANNEL)).await;
    assert!(!ready.accepts(CHANNEL));
    assert!(io.calls().is_empty(), "no actor or port was prepared");
    std::fs::remove_file(era).unwrap();
    let retried = managed(&harness, &io, &ready);
    assert_eq!(retried.len(), 1);
    for writer in ended {
        writer.stop_and_join().await.unwrap();
    }
    assert!(
        ready.is_hosted(CHANNEL),
        "old cleanup leaves the retry hosted"
    );
    for writer in retried {
        writer.stop_and_join().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn a_panicked_actor_never_reports_writer_stopped_or_releases_its_claim() {
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (harness, path, io, ready, writer) = hosting().await;
    let (panicked, witnessed) = tokio::sync::oneshot::channel();
    let panicked = Mutex::new(Some(panicked));
    *harness.port.on_post.lock().unwrap() = Some(Box::new(move || {
        panicked.lock().unwrap().take().unwrap().send(()).unwrap();
        panic!("actor POST hook panicked");
    }));
    append(&path, &row("m2", "panic"));
    tokio::time::advance(POLL_INTERVAL * 2).await;
    witnessed.await.unwrap();
    let stopped = writer.stop_and_join().await;
    assert!(
        stopped.is_err(),
        "an abnormal actor end is not WriterStopped"
    );
    assert!(ready.is_hosted(CHANNEL), "its unconfirmed end stays fenced");
    assert!(!ready.accepts(CHANNEL));
    assert!(managed(&harness, &io, &ready).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_stop_during_port_wait_releases_the_claim_without_starting_an_actor() {
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (harness, _) = fresh(startup);
    harness.gate.acquired();
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    io.port_down.store(true, Ordering::SeqCst);
    let mut writers = managed(&harness, &io, &ready);
    assert_eq!(writers.len(), 1);
    scheduled_until(|| io.calls().contains(&("port", 0))).await;
    drop(writers.pop().unwrap().stop_and_join());
    scheduled_until(|| !ready.is_hosted(CHANNEL)).await;
    assert!(!ready.accepts(CHANNEL));
    assert!(!io.calls().contains(&("lease", 0)), "no actor was started");
    assert!(harness.port.posts().is_empty());
    io.port_down.store(false, Ordering::SeqCst);
    let again = managed(&harness, &io, &ready);
    assert_eq!(again.len(), 1);
    for writer in again {
        writer.stop_and_join().await.unwrap();
    }
}

struct Writers(Vec<ManagedWriterHandle>);

impl HomeBundle for Writers {
    async fn stop_and_join(self, _: StopReason) -> Settled {
        for writer in self.0 {
            if let Err(detail) = writer.stop_and_join().await {
                return Settled::Stuck(detail);
            }
        }
        Settled::Joined
    }
}

// Overlapping starts of a channel build one writer generation; a replacement joins the old writer
// before the new one is built, and a stale generation's stop leaves the current one running.
#[tokio::test(start_paused = true)]
async fn a_supervised_channel_runs_one_writer_generation_at_a_time() {
    let _selected = test_override::force_candidates(&[(CHANNEL, ClaudeTui)]);
    let (harness, path) = fresh(startup);
    harness.gate.acquired();
    p5::set_test_root(Some(harness._runtime.path()));
    let (io, ready) = (TestIo::over(&harness), Arc::new(Readiness::default()));
    let supervisor = Arc::new(Supervisor::<Writers>::default());
    // Each build records whether the channel was still hosted when it began.
    let builds = Arc::new(Mutex::new(Vec::new()));
    let build = || {
        let (io, ready, builds) = (Arc::clone(&io), Arc::clone(&ready), Arc::clone(&builds));
        let (root, gate) = (root(&harness), Arc::clone(&harness.gate));
        move |generation: Generation| async move {
            builds
                .lock()
                .unwrap()
                .push((generation, ready.is_hosted(CHANNEL)));
            let parts = || HostParts {
                io,
                runtime_root: Some(root),
                gate,
                readiness: ready,
            };
            let owned = cutover::boot_ownership();
            let writers = start_managed(ShadowProvider::Claude, true, owned, parts);
            let hosted = writers.len() == 1;
            hosted
                .then_some(Writers(writers))
                .ok_or("not hosted".into())
        }
    };

    let first = supervisor.start(CHANNEL, build());
    let second = supervisor.start(CHANNEL, build());
    assert_eq!(first.await, Ok(1));
    assert_eq!(second.await, Err(Refused::Busy(Phase::Starting(1))));
    polls(3).await;
    append(&path, &row("m1", "first"));
    polls(3).await;

    assert_eq!(supervisor.start(CHANNEL, build()).await, Ok(2));
    assert_eq!(*builds.lock().unwrap(), [(1, false), (2, false)]);
    let stale = supervisor.stop(CHANNEL, Some(1), StopReason::RowGone).await;
    assert_eq!(stale, Some(Phase::Running(2)));
    polls(3).await;
    append(&path, &row("m2", "second"));
    polls(3).await;
    assert_eq!(harness.port.posts(), ["first", "second"]);
    assert!(ready.accepts(CHANNEL), "the current generation still runs");

    let stopped = supervisor.stop(CHANNEL, None, StopReason::Sigterm).await;
    assert_eq!(stopped, None);
    assert!(!ready.is_hosted(CHANNEL));
}
