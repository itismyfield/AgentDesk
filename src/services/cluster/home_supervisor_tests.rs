//! The supervisor carries out each transition itself: a cancelled caller, a stop during a start,
//! an unconfirmed end and every provider's exit are settled by it, not by whoever waited.

use std::sync::atomic::AtomicUsize;
use std::time::Duration;

use tokio::sync::{Barrier, mpsc, oneshot};

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

#[derive(Default)]
struct TaskCounts {
    active: [AtomicUsize; 3],
    started: [AtomicUsize; 3],
    joined: [AtomicUsize; 3],
}

struct TaskBundle {
    stop: watch::Sender<bool>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
    counts: Arc<TaskCounts>,
}

impl TaskBundle {
    async fn build(counts: Arc<TaskCounts>) -> Self {
        let (stop, stopping) = watch::channel(false);
        let (ready, mut started) = mpsc::unbounded_channel();
        let mut tasks = Vec::new();
        for role in 0..3 {
            let (counts, ready) = (Arc::clone(&counts), ready.clone());
            let mut stopping = stopping.clone();
            tasks.push(tokio::spawn(async move {
                counts.active[role].fetch_add(1, Ordering::SeqCst);
                counts.started[role].fetch_add(1, Ordering::SeqCst);
                ready.send(()).unwrap();
                stopping.wait_for(|stop| *stop).await.unwrap();
                counts.active[role].fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for _ in 0..3 {
            started.recv().await.unwrap();
        }
        Self {
            stop,
            tasks,
            counts,
        }
    }
}

impl HomeBundle for TaskBundle {
    async fn stop_and_join(self, _: StopReason) -> Settled {
        self.stop.send_replace(true);
        for (role, task) in self.tasks.into_iter().enumerate() {
            task.await.unwrap();
            self.counts.joined[role].fetch_add(1, Ordering::SeqCst);
        }
        Settled::Joined
    }
}

fn task_counts(counts: &[AtomicUsize; 3]) -> [usize; 3] {
    std::array::from_fn(|role| counts[role].load(Ordering::SeqCst))
}

// The roles model watch, lease and writer tasks: only the reserved build can start them, and
// replacement cannot build its tasks until every old role has joined.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_starts_reserve_one_bundle_and_replacement_joins_every_task() {
    let supervisor = Arc::new(Supervisor::default());
    let begin = Arc::new(Barrier::new(3));
    let release_build = Arc::new(Barrier::new(2));
    let builds = Arc::new(AtomicUsize::new(0));
    let counts = Arc::new(TaskCounts::default());
    let (completed, mut outcomes) = mpsc::unbounded_channel();
    let mut callers = Vec::new();
    for _ in 0..2 {
        let (supervisor, begin, release_build) = (
            Arc::clone(&supervisor),
            Arc::clone(&begin),
            Arc::clone(&release_build),
        );
        let (builds, counts, completed) =
            (Arc::clone(&builds), Arc::clone(&counts), completed.clone());
        callers.push(tokio::spawn(async move {
            begin.wait().await;
            let result = supervisor
                .start(7, move |_| async move {
                    builds.fetch_add(1, Ordering::SeqCst);
                    release_build.wait().await;
                    Ok(TaskBundle::build(counts).await)
                })
                .await;
            completed.send(result).unwrap();
        }));
    }
    begin.wait().await;
    assert_eq!(
        outcomes.recv().await,
        Some(Err(Refused::Busy(Phase::Starting(1))))
    );
    assert_eq!(task_counts(&counts.active), [0; 3]);
    release_build.wait().await;
    assert_eq!(outcomes.recv().await, Some(Ok(1)));
    for caller in callers {
        caller.await.unwrap();
    }
    assert_eq!(builds.load(Ordering::SeqCst), 1);
    assert_eq!(task_counts(&counts.active), [1; 3]);
    assert_eq!(task_counts(&counts.started), [1; 3]);

    let replacement_counts = Arc::clone(&counts);
    let replaced = supervisor
        .start(7, move |_| async move {
            assert_eq!(task_counts(&replacement_counts.active), [0; 3]);
            assert_eq!(task_counts(&replacement_counts.joined), [1; 3]);
            Ok(TaskBundle::build(replacement_counts).await)
        })
        .await;
    assert_eq!(replaced, Ok(2));
    assert_eq!(task_counts(&counts.active), [1; 3]);
    assert_eq!(task_counts(&counts.started), [2; 3]);
    assert_eq!(supervisor.stop(7, Some(2), StopReason::Sigterm).await, None);
    assert_eq!(task_counts(&counts.active), [0; 3]);
    assert_eq!(task_counts(&counts.joined), [2; 3]);
}

#[tokio::test]
async fn stale_cleanup_and_stale_stop_leave_the_current_generation_running() {
    let supervisor = Arc::new(Supervisor::default());
    let stops = Arc::new(AtomicUsize::new(0));
    let first = bundle(&stops);
    assert_eq!(supervisor.start(7, |_| async { Ok(first) }).await, Ok(1));
    let second = bundle(&stops);
    assert_eq!(supervisor.start(7, |_| async { Ok(second) }).await, Ok(2));
    assert_eq!(stops.load(Ordering::SeqCst), 1);

    let (done, completed) = watch::channel(false);
    let mut stale = Finish {
        supervisor: Arc::clone(&supervisor),
        channel: 7,
        generation: 1,
        done: Some(done),
    };
    stale.apply(None);
    assert!(*completed.borrow());
    assert_eq!(supervisor.phase(7), Some(Phase::Running(2)));
    assert_eq!(
        supervisor.stop(7, Some(1), StopReason::RowGone).await,
        Some(Phase::Running(2))
    );
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert_eq!(supervisor.stop(7, Some(2), StopReason::Sigterm).await, None);
    assert_eq!(stops.load(Ordering::SeqCst), 2);
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
