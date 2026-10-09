//! The supervisor carries out each transition itself: a cancelled caller, a stop during a start,
//! an unconfirmed end and every provider's exit are settled by it, not by whoever waited.

use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use tokio::sync::oneshot;

use super::*;

/// A bundle whose stop waits for `release` when given one and reports `settled`.
struct Bundle {
    stops: Arc<AtomicUsize>,
    release: Option<oneshot::Receiver<()>>,
    settled: Settled,
}

impl HomeBundle for Bundle {
    async fn stop_and_join(self, _: StopReason) -> Settled {
        if let Some(release) = self.release {
            let _ = release.await;
        }
        self.stops.fetch_add(1, Ordering::SeqCst);
        self.settled
    }
}

fn bundle(stops: &Arc<AtomicUsize>) -> Bundle {
    Bundle {
        stops: Arc::clone(stops),
        release: None,
        settled: Settled::Joined,
    }
}

/// A build that waits for the returned sender before it yields `built`.
fn gated(built: Bundle) -> (oneshot::Sender<()>, impl FnOnce(Generation) -> GatedBuild) {
    let (open, gate) = oneshot::channel();
    let build = move |_| -> GatedBuild {
        Box::pin(async move {
            let _ = gate.await;
            Ok(built)
        })
    };
    (open, build)
}

type GatedBuild = Pin<Box<dyn Future<Output = Result<Bundle, String>> + Send>>;

async fn settle() {
    tokio::time::sleep(Duration::from_millis(10)).await;
}

// Dropping the caller's future neither cancels the start nor the stop it asked for: the start
// still runs its bundle, and the stop still joins it and frees the channel.
#[tokio::test]
async fn a_cancelled_caller_leaves_the_transition_to_the_supervisor() {
    let supervisor = Arc::new(Supervisor::default());
    let stops = Arc::new(AtomicUsize::new(0));
    let (open, build) = gated(bundle(&stops));
    drop(supervisor.start(7, build));
    assert_eq!(supervisor.phase(7), Some(Phase::Starting(1)));
    open.send(()).unwrap();
    settle().await;
    assert_eq!(supervisor.phase(7), Some(Phase::Running(1)));

    let (release, held) = oneshot::channel();
    let other = Arc::new(Supervisor::default());
    let mut held_bundle = bundle(&stops);
    held_bundle.release = Some(held);
    other.start(8, |_| async { Ok(held_bundle) }).await.unwrap();
    let stopping = other.stop(8, None, StopReason::Sigterm);
    let waited = tokio::time::timeout(Duration::from_millis(10), stopping).await;
    assert!(waited.is_err(), "the stop waits for the bundle");
    assert_eq!(other.phase(8), Some(Phase::Stopping(1)));
    release.send(()).unwrap();
    settle().await;
    assert_eq!(other.phase(8), None);
    assert_eq!(stops.load(Ordering::SeqCst), 1);
}

// A stop that arrives while a generation starts stops the bundle it built instead of running it.
#[tokio::test]
async fn a_stop_during_a_start_stops_the_new_bundle_instead_of_running_it() {
    let supervisor = Arc::new(Supervisor::default());
    let stops = Arc::new(AtomicUsize::new(0));
    let (open, build) = gated(bundle(&stops));
    let starting = supervisor.start(7, build);
    let stopping = supervisor.stop(7, None, StopReason::Sigterm);
    open.send(()).unwrap();
    assert_eq!(starting.await, Err(Refused::Stopped(1)));
    assert_eq!(stopping.await, None);
    assert_eq!(stops.load(Ordering::SeqCst), 1);
}

// An end that was not confirmed, or a build that panicked, blocks the channel: no later
// generation starts beside what may still run.
#[tokio::test]
async fn an_unconfirmed_end_blocks_the_channel() {
    let supervisor = Arc::new(Supervisor::default());
    let stops = Arc::new(AtomicUsize::new(0));
    let mut stuck = bundle(&stops);
    stuck.settled = Settled::Stuck("actor still running".into());
    supervisor.start(7, |_| async { Ok(stuck) }).await.unwrap();
    let blocked = Phase::Blocked(1, "actor still running".into());
    assert_eq!(
        supervisor.stop(7, None, StopReason::Sigterm).await,
        Some(blocked)
    );
    let again = supervisor.start(7, |_| async { Ok(bundle(&Arc::default())) });
    assert_eq!(
        again.await,
        Err(Refused::Blocked(1, "actor still running".into()))
    );

    let panicked = supervisor.start(8, |_| async { panicking() });
    assert_eq!(panicked.await, Err(Refused::Lost));
    let phase = supervisor.phase(8);
    assert!(matches!(phase, Some(Phase::Blocked(2, _))), "{phase:?}");
}

fn panicking() -> Result<Bundle, String> {
    panic!("build panicked")
}

struct Recorded {
    stopped: Arc<Mutex<Vec<&'static str>>>,
    name: &'static str,
    delay: Duration,
}

impl HomeLifecycle for Recorded {
    fn pause_intake(&self, _: &str) {}

    fn resume_intake(&self, _: &str) {}

    fn stop_and_join(&self, _: StopReason) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            tokio::time::sleep(self.delay).await;
            self.stopped.lock().unwrap().push(self.name);
        })
    }
}

// With nothing registered, as with the switch off, the exit has nothing to wait for; with homes
// on two providers, it returns only after both stopped, the slower one included.
#[tokio::test(start_paused = true)]
async fn the_exit_waits_for_every_providers_homes() {
    stop_all_and_join(StopReason::CommittedRestart).await;
    lifecycle("claude").stop_and_join(StopReason::Sigterm).await;

    let stopped = Arc::new(Mutex::new(Vec::new()));
    for (name, delay) in [("claude", 50), ("codex", 0)] {
        let recorded = Recorded {
            stopped: Arc::clone(&stopped),
            name,
            delay: Duration::from_millis(delay),
        };
        register_lifecycle(name, Arc::new(recorded));
    }
    stop_all_and_join(StopReason::CommittedRestart).await;
    assert_eq!(*stopped.lock().unwrap(), ["codex", "claude"]);
}
