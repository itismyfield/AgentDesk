//! Lifecycle cleanup through the production managed-host entry, including an unpolled future.

use super::late_delegated::{Legacy, closed, seed};
use super::*;
use crate::services::tui_o::writer::activation::test_hook::{self, Step};
use crate::services::tui_o::writer::host::{ManagedWriterHandle, start_managed};
use std::sync::atomic::AtomicU64;
use std::time::Duration;

struct Fixture {
    harness: Harness,
    path: PathBuf,
    io: Arc<TestIo>,
    legacy: Arc<Legacy>,
    ready: Arc<Readiness>,
    _boot: test_override::ChannelsGuard,
}

impl Fixture {
    fn new() -> Self {
        let (harness, path) = fresh(|_| Vec::new());
        harness.gate.acquired();
        let boot = seed(&harness, false);
        let io = TestIo::over(&harness);
        io.late_enabled.store(true, Ordering::SeqCst);
        let legacy = Arc::new(Legacy {
            path: path.clone(),
            frontier: AtomicU64::new(0),
        });
        *io.legacy.lock().unwrap() = Some(legacy.clone());
        Self {
            harness,
            path,
            io,
            legacy,
            ready: Arc::default(),
            _boot: boot,
        }
    }

    fn start_with(&self, runtime_root: Option<PathBuf>) -> Vec<ManagedWriterHandle> {
        p5::set_test_root(Some(self.harness._runtime.path()));
        start_managed(
            ShadowProvider::Claude,
            true,
            cutover::boot_ownership(),
            || HostParts {
                io: self.io.clone(),
                runtime_root,
                gate: self.harness.gate.clone(),
                readiness: self.ready.clone(),
            },
        )
    }

    fn start(&self) -> ManagedWriterHandle {
        let mut handles = self.start_with(Some(root(&self.harness)));
        assert_eq!(handles.len(), 1);
        handles.pop().unwrap()
    }

    fn bind_delivered(&self) -> u64 {
        append(
            &self.path,
            &row("cleanup-old", "cleanup old delivered payload"),
        );
        append(&self.path, &closed());
        let end = std::fs::metadata(&self.path).unwrap().len();
        self.legacy.frontier.store(end, Ordering::SeqCst);
        let source = source_id_for("s1", &self.path).unwrap();
        p5_log(self.harness._runtime.path(), CHANNEL, &startup(source));
        end
    }

    async fn first_facts(&self) {
        for _ in 0..32 {
            tokio::task::yield_now().await;
            if self.io.calls().contains(&("facts", CHANNEL)) {
                return;
            }
        }
        panic!("the actual host never polled activation facts");
    }

    async fn adopted_and_posts_new_once(&self, end: u64) {
        for _ in 0..18 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            tokio::task::yield_now().await;
            if self.ready.accepts(CHANNEL) {
                break;
            }
        }
        assert!(
            self.ready.accepts(CHANNEL),
            "actual host did not become ready"
        );
        assert_eq!(adoption(CHANNEL), Adoption::Committed);
        assert_eq!(
            self.harness
                .store
                .read_init(CHANNEL)
                .unwrap()
                .unwrap()
                .sources[0]
                .delivery_start,
            end
        );
        assert!(
            self.harness.port.posts().is_empty(),
            "old payload was reposted"
        );
        append(
            &self.path,
            &row("cleanup-new", "cleanup unique new payload"),
        );
        polls(3).await;
        assert_eq!(self.harness.port.posts(), ["cleanup unique new payload"]);
    }

    fn assert_no_publication(&self) {
        assert_eq!(self.harness.store.read_init(CHANNEL).unwrap(), None);
        assert!(self.harness.port.posts().is_empty());
        assert!(!self.ready.accepts(CHANNEL));
    }
}

#[tokio::test(start_paused = true)]
async fn cleanup_abort_before_first_poll_releases_reserved_candidate() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::cleanup_abort_before_first_poll_releases_reserved_candidate"
    )) {
        return;
    }
    let f = Fixture::new();
    let handle = f.start();
    assert!(
        f.io.calls().is_empty(),
        "single-thread runtime must not have polled the spawned future"
    );
    let task = handle.into_detached();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(
        adoption(CHANNEL),
        Adoption::Released,
        "cleanup must be owned before spawn"
    );
    f.assert_no_publication();
}

#[tokio::test(start_paused = true)]
async fn cleanup_root_and_home_failures_leave_no_reserved_host() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::cleanup_root_and_home_failures_leave_no_reserved_host"
    )) {
        return;
    }
    let f = Fixture::new();
    assert!(f.start_with(None).is_empty());
    assert_eq!(adoption(CHANNEL), Adoption::Released);
    assert!(!f.ready.is_hosted(CHANNEL));
    assert!(
        f.io.alarms
            .halted()
            .iter()
            .any(|(_, detail)| detail.contains("runtime root unresolved"))
    );
    f.assert_no_publication();

    let f = Fixture::new();
    use crate::services::cluster::home_availability::{self, Unavailable};
    let _registration =
        home_availability::install("claude", Err(Unavailable::MissingPool), || [CHANNEL].into());
    assert!(f.start_with(Some(root(&f.harness))).is_empty());
    assert_eq!(adoption(CHANNEL), Adoption::Released);
    assert!(!f.ready.is_hosted(CHANNEL));
    assert!(
        f.io.alarms
            .halted()
            .iter()
            .any(|(_, detail)| detail.contains("missing_pool"))
    );
    f.assert_no_publication();
}

#[tokio::test(start_paused = true)]
async fn cleanup_duplicate_claim_preserves_original_then_same_generation_posts_once() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::cleanup_duplicate_claim_preserves_original_then_same_generation_posts_once"
    )) {
        return;
    }
    let f = Fixture::new();
    let handle = f.start();
    let generation = handle.generation();
    assert!(f.start_with(Some(root(&f.harness))).is_empty());
    assert_eq!(adoption(CHANNEL), Adoption::Deferred);
    let end = f.bind_delivered();
    f.adopted_and_posts_new_once(end).await;
    assert_eq!(handle.generation(), generation);
    handle.stop_and_join().await.unwrap();
    assert!(!f.ready.is_hosted(CHANNEL));
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
}

#[tokio::test(start_paused = true)]
async fn cleanup_stop_after_first_poll_finishes_before_host_claim_is_released() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::cleanup_stop_after_first_poll_finishes_before_host_claim_is_released"
    )) {
        return;
    }
    let f = Fixture::new();
    let handle = f.start();
    f.first_facts().await;
    handle.stop_and_join().await.unwrap();
    assert_eq!(adoption(CHANNEL), Adoption::Released);
    assert!(!f.ready.is_hosted(CHANNEL));
    f.assert_no_publication();
}

#[tokio::test(start_paused = true)]
async fn cleanup_facts_panic_releases_unpublished_candidate() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::cleanup_facts_panic_releases_unpublished_candidate"
    )) {
        return;
    }
    let f = Fixture::new();
    *f.io.on_facts.lock().unwrap() = Some(Box::new(|| panic!("late preflight facts panic")));
    let task = f.start().into_detached();
    assert!(task.await.unwrap_err().is_panic());
    assert!(f.io.calls().contains(&("facts", CHANNEL)));
    assert_eq!(adoption(CHANNEL), Adoption::Released);
    f.assert_no_publication();
}

#[tokio::test(start_paused = true)]
async fn cleanup_store_corrupted_after_seed_settles_held_without_posts() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::cleanup_store_corrupted_after_seed_settles_held_without_posts"
    )) {
        return;
    }
    let f = Fixture::new();
    let init = init_path(&f.harness, CHANNEL);
    std::fs::create_dir_all(init.parent().unwrap()).unwrap();
    std::fs::write(&init, b"corrupted init").unwrap();
    f.start().into_detached().await.unwrap();
    assert_eq!(
        adoption(CHANNEL),
        Adoption::Held,
        "uncertain store must keep output ownership"
    );
    assert!(f.harness.store.read_init(CHANNEL).is_err());
    assert!(!f.io.alarms.halted().is_empty());
    assert!(!f.ready.accepts(CHANNEL));
    assert!(f.harness.port.posts().is_empty());
}

#[tokio::test(start_paused = true)]
async fn cleanup_after_published_write_error_keeps_committed_host_and_posts_new_once() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::cleanup_after_published_write_error_keeps_committed_host_and_posts_new_once"
    )) {
        return;
    }
    let f = Fixture::new();
    let end = f.bind_delivered();
    test_hook::set(CHANNEL, Step::AfterWrite, || {
        Err("published write error".into())
    });
    let handle = f.start();
    f.adopted_and_posts_new_once(end).await;
    handle.stop_and_join().await.unwrap();
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    assert_eq!(f.harness.port.posts(), ["cleanup unique new payload"]);
}

#[tokio::test(start_paused = true)]
async fn cleanup_after_publication_panic_recovers_committed_state_before_legacy_claim() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::cleanup_after_publication_panic_recovers_committed_state_before_legacy_claim"
    )) {
        return;
    }
    let f = Fixture::new();
    let end = f.bind_delivered();
    test_hook::set(CHANNEL, Step::AfterWrite, || {
        panic!("panic after durable init")
    });
    let task = f.start().into_detached();
    for _ in 0..18 {
        tokio::time::sleep(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        if task.is_finished() {
            break;
        }
    }
    assert!(task.is_finished(), "publication hook was not reached");
    assert!(task.await.unwrap_err().is_panic());
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    assert_eq!(
        f.harness.store.read_init(CHANNEL).unwrap().unwrap().sources[0].delivery_start,
        end
    );
    use crate::services::tui_o::cutover::{BodyClaim, BodySend, claim_then_send};
    let legacy_posts = Arc::new(AtomicUsize::new(0));
    let count = legacy_posts.clone();
    assert_eq!(
        claim_then_send(
            Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))),
            || async move {
                count.fetch_add(1, Ordering::SeqCst);
            }
        )
        .await
        .unwrap(),
        BodySend::OwnedByO
    );
    assert_eq!(legacy_posts.load(Ordering::SeqCst), 0);
    assert!(f.harness.port.posts().is_empty());
}
