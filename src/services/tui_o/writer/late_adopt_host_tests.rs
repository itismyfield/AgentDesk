//! Late binding, transient refusal, rotation and live OFF use the actual managed host.

use super::*;
use crate::config::Config;
use crate::services::tui_o::channel_policy;
use crate::services::tui_o::cutover::{BodyClaim, BodySend, claim_then_send, intake_route};
use crate::services::tui_o::writer::host::{ManagedWriterHandle, start_managed};
use std::sync::atomic::AtomicU64;
use std::time::Duration;

pub(super) const GATEWAY: &str = "late-adopt-gateway";
pub(super) const WORKER: &str = "late-adopt-worker";

pub(super) fn config(foreign: bool) -> Config {
    serde_json::from_value(serde_json::json!({
        "server": {},
        "cluster": {
            "enabled": true,
            "instance_id": if foreign { WORKER } else { GATEWAY },
            "gateway_preferred_instance_id": GATEWAY
        },
        "tui_o": {"writer": {"channels": [CHANNEL], "late_adopt": true}},
        "agents": [{"id": "late", "name": "Late", "channels": {
            "claude": {"id": CHANNEL.to_string(), "runtime": "tui"}
        }}]
    }))
    .unwrap()
}

/// Never installs a state by hand: seed the actual temporary store through production code.
pub(super) fn seed(harness: &Harness, foreign: bool) -> test_override::ChannelsGuard {
    let config = config(foreign);
    let snapshot = BootChannels::validate(&config)
        .unwrap()
        .seeded(&config, |channels, _| {
            Ok(channel_policy::stored(
                Some(harness._runtime.path()),
                channels,
            ))
        })
        .unwrap();
    crate::config_live_reload::install(config);
    test_override::force_boot(snapshot)
}

pub(super) struct Legacy {
    pub(super) path: PathBuf,
    pub(super) frontier: AtomicU64,
}

impl LegacyView for Legacy {
    fn started(&self) -> bool {
        true
    }

    fn cursor(&self, _: &str) -> LegacyCursor {
        LegacyCursor::Bound {
            path: self.path.clone(),
            offset: std::fs::metadata(&self.path).unwrap().len(),
        }
    }

    fn late_cursor(&self, tmux: &str) -> Result<LegacyCursor, String> {
        Ok(self.cursor(tmux))
    }

    fn frontier(&self, _: u64, _: &str, _: u64) -> Option<u64> {
        Some(self.frontier.load(Ordering::SeqCst))
    }

    fn tail_running(&self, _: &str) -> bool {
        false
    }
}

pub(super) fn closed() -> Vec<u8> {
    let row = serde_json::json!({"type":"system", "subtype":"turn_duration", "durationMs":5});
    format!("{row}\n").into_bytes()
}

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

/// Startup binds of `sources` in order, each `old` naming the one before, as restarts log them.
fn startup_binds(f: &Late, sources: &[SourceId]) {
    let mut log = Vec::new();
    for (index, source) in sources.iter().enumerate() {
        let event = p5::BindingEvent {
            seq: index as u64 + 1,
            channel_id: CHANNEL,
            provider: "claude".into(),
            tmux_session: "tmux".into(),
            execution_nonce: None,
            old: index.checked_sub(1).map(|before| sources[before].clone()),
            new: p5::BindingTarget::Source(source.clone()),
            cause: p5::BindingCause::Startup,
            parent_hint: None,
            evidence: p5::BindingEvidence {
                hook_event: Some("SessionStart".into()),
                received_at: chrono::Utc::now(),
            },
            committed_at: chrono::Utc::now(),
        };
        log.extend(serde_json::to_vec(&event).unwrap());
        log.push(b'\n');
    }
    p5_log(f.harness._runtime.path(), CHANNEL, &log);
}

/// A delivered transcript of its own, as one restart left it.
fn delivered_elsewhere(f: &Late, name: &str, session: &str) -> SourceId {
    let path = f.harness._runtime.path().join(format!("{name}.jsonl"));
    std::fs::write(
        &path,
        [row(name, "delivered before a restart"), closed()].concat(),
    )
    .unwrap();
    source_id_for(session, &path).unwrap()
}

/// A log of `rows` restarts logged before this process: one new transcript each, the second with
/// an empty session id, the current one last with Legacy's frontier at its end.
fn restarted(f: &Late, rows: usize) -> (Vec<SourceId>, u64) {
    let mut sources: Vec<SourceId> = (1..rows)
        .map(|index| {
            let session = if index == 2 {
                String::new()
            } else {
                format!("s{index}")
            };
            delivered_elsewhere(f, &format!("restart-{index}"), &session)
        })
        .collect();
    append(&f.path, &row("late-old", "old delivered payload"));
    append(&f.path, &closed());
    let end = std::fs::metadata(&f.path).unwrap().len();
    f.legacy.frontier.store(end, Ordering::SeqCst);
    sources.push(source_id_for(&format!("s{rows}"), &f.path).unwrap());
    startup_binds(f, &sources);
    (sources, end)
}

/// The one rotation the channel was released for.
fn released_detail(f: &Late) -> String {
    let released = f.io.alarms.released();
    let rotations: Vec<_> = (released.iter())
        .filter(|(channel, detail)| *channel == CHANNEL && detail.contains("bound while"))
        .collect();
    assert_eq!(rotations.len(), 1, "{released:?}");
    rotations[0].1.clone()
}

#[tokio::test(start_paused = true)]
async fn late_adopt_history_logged_before_the_reservation_commits_and_posts_new_once() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_history_logged_before_the_reservation_commits_and_posts_new_once"
    )) {
        return;
    }
    let f = Late::new();
    let (sources, end) = restarted(&f, 24);
    let handle = f.start();
    f.assert_new_once(end).await;
    let init = f.harness.store.read_init(CHANNEL).unwrap().unwrap();
    let starts: Vec<_> = (init.sources.iter())
        .map(|source| (source.source_id.clone(), source.delivery_start))
        .collect();
    let len = |source: &SourceId| std::fs::metadata(&source.path).unwrap().len();
    let past = sources[..23]
        .iter()
        .rev()
        .map(|source| (source.clone(), len(source)));
    let expected: Vec<_> = [(sources[23].clone(), end)]
        .into_iter()
        .chain(past)
        .collect();
    assert_eq!(
        starts, expected,
        "every earlier transcript starts at its end"
    );
    handle.stop_and_join().await.unwrap();
    // A restart over the committed store posts neither history nor the delivered body again.
    let _restarted = seed(&f.harness, false);
    assert_eq!(adoption(CHANNEL), Adoption::Committed);
    let handle = f.start();
    append(&f.path, &row("late-after", "after restart payload"));
    polls(3).await;
    assert_eq!(
        f.harness.port.posts(),
        ["unique new payload", "after restart payload"]
    );
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn late_adopt_bind_after_the_reservation_boundary_releases_naming_its_seq() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_bind_after_the_reservation_boundary_releases_naming_its_seq"
    )) {
        return;
    }
    // A new transcript, then an earlier one bound again (A→B→A), after the boundary.
    for again in [false, true] {
        let f = Late::new();
        let (mut sources, end) = restarted(&f, 4);
        f.legacy.frontier.store(0, Ordering::SeqCst);
        let handle = f.start();
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(adoption(CHANNEL), Adoption::Deferred, "again {again}");
        let next = match again {
            true => sources[1].clone(),
            false => delivered_elsewhere(&f, "after-boundary", "s5"),
        };
        sources.push(next.clone());
        startup_binds(&f, &sources);
        f.legacy.frontier.store(end, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(adoption(CHANNEL), Adoption::Released, "again {again}");
        let detail = released_detail(&f);
        let (rotated, current) = (next.path.display(), f.path.display());
        let expected = format!(
            "source {rotated} was bound while the adoption waited (seq 5; baseline {current}@4; boundary 4;"
        );
        assert!(detail.contains(&expected), "again {again}: {detail}");
        assert!(!init_path(&f.harness, CHANNEL).exists());
        assert!(f.harness.port.posts().is_empty());
        handle.stop_and_join().await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn late_adopt_inflight_row_at_the_boundary_keeps_the_whole_history_as_baseline() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_inflight_row_at_the_boundary_keeps_the_whole_history_as_baseline"
    )) {
        return;
    }
    let f = Late::new();
    let (sources, _) = restarted(&f, 4);
    // A previous process's turn row survives the restart; its source is not known to be done.
    *f.io.custody.lock().unwrap() = Ok(Custody::Row);
    let handle = f.start();
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(adoption(CHANNEL), Adoption::Released);
    let (rotated, first) = (sources[1].path.display(), sources[0].path.display());
    let expected = format!(
        "source {rotated} was bound while the adoption waited (seq 2; baseline {first}@1; boundary 0;"
    );
    let detail = released_detail(&f);
    assert!(detail.contains(&expected), "{detail}");
    assert!(!init_path(&f.harness, CHANNEL).exists());
    assert!(f.harness.port.posts().is_empty());
    handle.stop_and_join().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn late_adopt_successor_host_keeps_the_reservation_boundary() {
    if !test_override::isolated_binding_case(concat!(
        module_path!(),
        "::late_adopt_successor_host_keeps_the_reservation_boundary"
    )) {
        return;
    }
    let f = Late::new();
    let (mut sources, _) = restarted(&f, 4);
    f.legacy.frontier.store(0, Ordering::SeqCst);
    let handle = f.start();
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(adoption(CHANNEL), Adoption::Deferred);
    handle.keep_reservation();
    handle.stop_and_join().await.unwrap();
    assert_eq!(
        adoption(CHANNEL),
        Adoption::Deferred,
        "the reservation is kept"
    );
    let next = delivered_elsewhere(&f, "between-hosts", "s5");
    sources.push(next.clone());
    startup_binds(&f, &sources);
    // Legacy delivered the new transcript, so only the kept boundary refuses it.
    *f.io.legacy.lock().unwrap() = Some(Arc::new(Legacy {
        path: next.path.clone(),
        frontier: AtomicU64::new(std::fs::metadata(&next.path).unwrap().len()),
    }));
    let handle = f.start();
    tokio::time::sleep(Duration::from_secs(30)).await;
    assert_eq!(adoption(CHANNEL), Adoption::Released);
    let (rotated, current) = (next.path.display(), f.path.display());
    let expected = format!(
        "source {rotated} was bound while the adoption waited (seq 5; baseline {current}@4; boundary 4;"
    );
    let detail = released_detail(&f);
    assert!(detail.contains(&expected), "{detail}");
    assert!(!init_path(&f.harness, CHANNEL).exists());
    assert!(f.harness.port.posts().is_empty());
    handle.stop_and_join().await.unwrap();
}
