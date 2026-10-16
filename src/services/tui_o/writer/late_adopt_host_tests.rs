//! Late binding, transient refusal, rotation and live OFF use the actual managed host.

use super::late_delegated::{Legacy, closed, seed};
use super::*;
use crate::services::tui_o::cutover::{BodyClaim, BodySend, claim_then_send, intake_route};
use crate::services::tui_o::writer::host::{ManagedWriterHandle, start_managed};
use std::sync::atomic::AtomicU64;
use std::time::Duration;

struct Late {
    harness: Harness,
    path: PathBuf,
    io: Arc<TestIo>,
    legacy: Arc<Legacy>,
    ready: Arc<Readiness>,
    _boot: test_override::ChannelsGuard,
}

#[tokio::test(start_paused = true)]
async fn late_adopt_preflight_does_not_act_on_facts_from_a_lost_generation() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_preflight_does_not_act_on_facts_from_a_lost_generation"
    )) {
        return;
    }
    let f = Late::new();
    f.io.facts.lock().unwrap().as_mut().unwrap().runner_sessions = 1;
    let gate = f.harness.gate.clone();
    *f.io.on_facts.lock().unwrap() = Some(Box::new(move || gate.lost()));
    let handle = f.start();
    polls(3).await;
    assert!(f.io.calls().contains(&("facts", CHANNEL)));
    assert_eq!(adoption(CHANNEL), Adoption::Deferred);
    assert!(!init_path(&f.harness, CHANNEL).exists());
    *f.io.facts.lock().unwrap() = Ok(ActivationFacts::default());
    f.harness.gate.acquired();
    let end = f.bind_delivered();
    f.assert_new_once(end).await;
    handle.stop_and_join().await.unwrap();
}

impl Late {
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

    fn start(&self) -> ManagedWriterHandle {
        p5::set_test_root(Some(self.harness._runtime.path()));
        let mut handles = start_managed(
            ShadowProvider::Claude,
            true,
            cutover::boot_ownership(),
            || HostParts {
                io: self.io.clone(),
                runtime_root: Some(root(&self.harness)),
                gate: self.harness.gate.clone(),
                readiness: self.ready.clone(),
            },
        );
        assert_eq!(handles.len(), 1);
        handles.pop().unwrap()
    }

    fn bind_delivered(&self) -> u64 {
        append(&self.path, &row("late-old", "old delivered payload"));
        append(&self.path, &closed());
        let end = std::fs::metadata(&self.path).unwrap().len();
        self.legacy.frontier.store(end, Ordering::SeqCst);
        let source = source_id_for("s1", &self.path).unwrap();
        p5_log(self.harness._runtime.path(), CHANNEL, &startup(source));
        end
    }

    async fn adopted(&self) {
        for _ in 0..16 {
            tokio::time::sleep(Duration::from_secs(5)).await;
            tokio::task::yield_now().await;
            if self.ready.accepts(CHANNEL) {
                return;
            }
        }
        panic!(
            "same host did not become ready: {:?}, {:?}",
            adoption(CHANNEL),
            self.io.alarms.0.lock().unwrap()
        );
    }

    async fn assert_new_once(&self, end: u64) {
        self.adopted().await;
        assert_eq!(adoption(CHANNEL), Adoption::Committed);
        let init = self.harness.store.read_init(CHANNEL).unwrap().unwrap();
        assert_eq!(init.sources[0].delivery_start, end);
        assert!(
            self.harness.port.posts().is_empty(),
            "delivered history was reposted"
        );
        append(&self.path, &row("late-new", "unique new payload"));
        polls(3).await;
        assert_eq!(self.harness.port.posts(), ["unique new payload"]);
    }
}

#[tokio::test(start_paused = true)]
async fn late_adopt_first_placement_before_spawn_then_late_binding_posts_only_new_payload() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_first_placement_before_spawn_then_late_binding_posts_only_new_payload"
    )) {
        return;
    }
    let f = Late::new();
    assert_eq!(
        intake_route::route_for_placement("claude", CHANNEL),
        intake_route::IntakeRoute::Unselected
    );
    let old = Arc::new(Mutex::new(Vec::<String>::new()));
    let sent = old.clone();
    assert_eq!(
        claim_then_send(
            Some(BodyClaim::new(CHANNEL, Some(ClaudeTui))),
            || async move {
                sent.lock().unwrap().push("old delivered payload".into());
            }
        )
        .await
        .unwrap(),
        BodySend::Sent(())
    );
    let handle = f.start();
    let generation = handle.generation();
    polls(3).await;
    assert!(!f.ready.accepts(CHANNEL) && !init_path(&f.harness, CHANNEL).exists());
    let end = f.bind_delivered();
    f.assert_new_once(end).await;
    assert_eq!(*old.lock().unwrap(), ["old delivered payload"]);
    assert_eq!(handle.generation(), generation);
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn late_adopt_open_intake_and_custody_clear_without_restarting_host() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_open_intake_and_custody_clear_without_restarting_host"
    )) {
        return;
    }
    let f = Late::new();
    f.io.facts.lock().unwrap().as_mut().unwrap().open_intake = 1;
    *f.io.custody.lock().unwrap() = Ok(Custody::Active);
    let handle = f.start();
    polls(3).await;
    let end = f.bind_delivered();
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert!(!init_path(&f.harness, CHANNEL).exists());
    assert!(!matches!(
        adoption(CHANNEL),
        Adoption::Released | Adoption::Held
    ));
    f.io.facts.lock().unwrap().as_mut().unwrap().open_intake = 0;
    *f.io.custody.lock().unwrap() = Ok(Custody::Row);
    f.assert_new_once(end).await;
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn late_adopt_final_blocker_wins_over_transient_and_corruption_does_not_wait() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_final_blocker_wins_over_transient_and_corruption_does_not_wait"
    )) {
        return;
    }
    for override_node in [false, true] {
        let f = Late::new();
        *f.io.facts.lock().unwrap() = Ok(ActivationFacts {
            open_intake: 1,
            runner_sessions: i64::from(!override_node),
            node_override: override_node.then(|| "other node".into()),
        });
        let handle = f.start();
        polls(3).await;
        assert_eq!(adoption(CHANNEL), Adoption::Released);
        assert!(!init_path(&f.harness, CHANNEL).exists());
        handle.stop_and_join().await.unwrap();
    }
    let f = Late::new();
    p5_log(f.harness._runtime.path(), CHANNEL, b"not-json\n");
    let handle = f.start();
    polls(3).await;
    assert_eq!(
        adoption(CHANNEL),
        Adoption::Released,
        "corrupt log is not no-source"
    );
    assert!(!init_path(&f.harness, CHANNEL).exists());
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn late_adopt_live_off_parks_same_waiter_then_on_resumes() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_live_off_parks_same_waiter_then_on_resumes"
    )) {
        return;
    }
    let f = Late::new();
    let handle = f.start();
    polls(3).await;
    f.io.late_enabled.store(false, Ordering::SeqCst);
    let end = f.bind_delivered();
    tokio::time::sleep(Duration::from_secs(90)).await;
    assert!(!init_path(&f.harness, CHANNEL).exists());
    assert!(f.ready.is_hosted(CHANNEL));
    assert!(
        !f.io.calls().contains(&("fence", CHANNEL)),
        "OFF permits no new commit attempt"
    );
    f.io.late_enabled.store(true, Ordering::SeqCst);
    f.assert_new_once(end).await;
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn late_adopt_live_off_after_admission_finishes_one_commit() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_live_off_after_admission_finishes_one_commit"
    )) {
        return;
    }
    let f = Late::new();
    let end = f.bind_delivered();
    let io = f.io.clone();
    crate::services::tui_o::writer::activation::test_hook::set(
        CHANNEL,
        crate::services::tui_o::writer::activation::test_hook::Step::AfterFence,
        move || {
            io.late_enabled.store(false, Ordering::SeqCst);
            Ok(())
        },
    );
    let handle = f.start();
    f.assert_new_once(end).await;
    assert!(!f.io.late_enabled.load(Ordering::SeqCst));
    assert_eq!(
        f.io.calls()
            .iter()
            .filter(|call| **call == ("fence", CHANNEL))
            .count(),
        1
    );
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn late_adopt_coalesced_bound_rotation_rejects_before_pin() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_coalesced_bound_rotation_rejects_before_pin"
    )) {
        return;
    }
    let f = Late::new();
    let handle = f.start();
    polls(3).await;
    let a = source_id_for("s1", &f.path).unwrap();
    let second = f.harness._runtime.path().join("second.jsonl");
    std::fs::write(&second, b"").unwrap();
    let b = source_id_for("s2", &second).unwrap();
    let mut records = Vec::new();
    for (seq, source) in [(3, a), (5, b)] {
        let mut value: serde_json::Value = serde_json::from_slice(&startup(source)).unwrap();
        value["seq"] = seq.into();
        records.extend(format!("{value}\n").bytes());
    }
    // Complete seq history includes audit-only events between the two source binds.
    for seq in [1, 2, 4, 6] {
        let mut value: serde_json::Value =
            serde_json::from_slice(&startup(source_id_for("s1", &f.path).unwrap())).unwrap();
        value["seq"] = seq.into();
        value["new"] = serde_json::to_value(p5::BindingTarget::Rejected {
            payload_session_id: "audit".into(),
            payload_transcript_path: None,
            reason: "audit only".into(),
        })
        .unwrap();
        records.extend(format!("{value}\n").bytes());
    }
    let mut sorted: Vec<serde_json::Value> = records
        .split(|b| *b == b'\n')
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_slice(l).unwrap())
        .collect();
    sorted.sort_by_key(|v| v["seq"].as_u64().unwrap());
    let log: String = sorted.iter().map(|v| format!("{v}\n")).collect();
    p5_log(f.harness._runtime.path(), CHANNEL, log.as_bytes());
    let events = crate::services::tui_o::writer::binding::BindingEvents::binding_events_since(
        &ChannelBindingLog::new(CHANNEL, ShadowProvider::Claude),
        CHANNEL,
        0,
    )
    .expect("valid coalesced binding history");
    assert_eq!(events.len(), 6);
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(adoption(CHANNEL), Adoption::Released);
    assert!(!init_path(&f.harness, CHANNEL).exists());
    assert!(f.harness.port.posts().is_empty());
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn late_adopt_stalled_frontier_never_skips_to_eof() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_stalled_frontier_never_skips_to_eof"
    )) {
        return;
    }
    let f = Late::new();
    let end = f.bind_delivered();
    f.legacy.frontier.store(0, Ordering::SeqCst);
    let handle = f.start();
    tokio::time::sleep(Duration::from_secs(90 * 60)).await;
    assert!(!init_path(&f.harness, CHANNEL).exists());
    assert_eq!(adoption(CHANNEL), Adoption::Deferred);
    assert!(
        !f.io
            .alarms
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(_, a)| matches!(a, WriterAlarm::Abandoned { .. }))
    );
    f.legacy.frontier.store(end, Ordering::SeqCst);
    f.assert_new_once(end).await;
    handle.stop_and_join().await.unwrap();
}
