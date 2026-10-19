//! Actual foreign-watcher release during a kick's Activity read, followed by live supervisor drain.
use super::*;
use crate::services::discord::turn_presence::supervisor::tests::LiveSupervisor;

struct ForeignWatcher {
    task: Option<tokio::task::JoinHandle<()>>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
    observation: inflight::o_seed_observation::Guard,
    _confirmation: TestConfirmation,
    _namespace: crate::services::platform::tmux::session_inventory::tests::ChannelNamespace,
}

impl ForeignWatcher {
    async fn attach(fixture: &Fixture) -> Self {
        let shared = &fixture.foreign[0];
        let channel = fixture.foreign_channel;
        let name = &fixture.foreign_name;
        let confirmation = TestConfirmation::new(channel.get());
        let namespace =
            crate::services::platform::tmux::session_inventory::tests::ChannelNamespace::new(
                channel.get(),
                fixture.host.socket.clone(),
            );
        let observation =
            inflight::o_seed_observation::Guard::new(&ProviderKind::Codex, channel.get());
        let key = adk_session::build_namespaced_session_key(
            &shared.token_hash,
            &ProviderKind::Codex,
            name,
        );
        inflight::seed_session_row_hashed(
            shared.pg_pool.as_ref().unwrap(),
            &key,
            channel.get(),
            &shared.token_hash,
            None,
        )
        .await;
        let transcript = fixture.rig.root.path().join("foreign-watcher.jsonl");
        std::fs::write(&transcript, "{}\n").unwrap();
        crate::services::discord::turn_presence::activity::tests::log_turn_source(
            &ProviderKind::Codex,
            channel.get(),
            name,
            "foreign-watcher-execution",
            &transcript,
        );
        crate::services::discord::turn_presence::activity::tests::companion_attach_log_watcher(
            shared,
            channel.get(),
            name,
            &transcript,
        );
        let handle = shared.tmux_watchers.get(&channel).unwrap();
        let flags = (
            handle.cancel.clone(),
            handle.paused.clone(),
            handle.resume_offset.clone(),
            handle.pause_epoch.clone(),
            handle.turn_delivered.clone(),
            handle.last_heartbeat_ts_ms.clone(),
        );
        drop(handle);
        let task = tokio::spawn(
            crate::services::discord::tmux::tmux_output_watcher_with_restore(
                channel,
                fixture.ctx.http.clone(),
                shared.clone(),
                transcript.display().to_string(),
                name.clone(),
                0,
                flags.0.clone(),
                flags.1,
                flags.2,
                flags.3,
                flags.4,
                flags.5,
                None,
            ),
        );
        until(async || observation.snapshot().watcher_initialization_observed).await;
        assert!(
            !task.is_finished(),
            "foreign watcher never entered its real loop"
        );
        Self {
            task: Some(task),
            cancel: flags.0,
            observation,
            _confirmation: confirmation,
            _namespace: namespace,
        }
    }

    async fn finish(&mut self) {
        tokio::time::timeout(DRAIN_LIMIT, self.task.take().unwrap())
            .await
            .expect("foreign watcher did not finish after its real registry release")
            .unwrap();
        assert_eq!(
            self.observation.snapshot().event_count("post_stream_exit"),
            1
        );
    }
}

impl Drop for ForeignWatcher {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

fn capture_logs() -> LogSink {
    let logs = LogSink(Arc::new(std::sync::Mutex::new(Vec::new())));
    tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(logs.clone())
        .try_init()
        .unwrap();
    logs
}

fn log_has(logs: &LogSink, message: &str, reason: &str) -> bool {
    String::from_utf8(logs.0.lock().unwrap().clone())
        .unwrap()
        .lines()
        .any(|line| line.contains(message) && line.contains(reason))
}

struct FailureLogs(LogSink);

impl Drop for FailureLogs {
    fn drop(&mut self) {
        if !std::thread::panicking() {
            return;
        }
        let bytes = self.0.0.lock().unwrap_or_else(|error| error.into_inner());
        let markers = [
            "[turn_presence] dequeue_held",
            "[turn_presence] unbound hold leaf",
            "Deferred drain: one-shot kick for channel",
            "Deferred drain: kickoff already active for channel; coalescing duplicate request",
        ];
        for line in String::from_utf8_lossy(&bytes).lines() {
            if let Some(start) = markers.iter().find_map(|marker| line.rfind(*marker)) {
                // Spans before the event message may contain a full namespaced key.
                eprintln!("A2_SAFE_FAILURE_LOG {}", &line[start..]);
            }
        }
    }
}

fn trace_supervisor_tick(supervisor: &LiveSupervisor, recovery_tick: usize) {
    let state = supervisor.state();
    let safe = serde_json::json!({
        "activity": state["activity"],
        "reason": state["reason"],
        "generation": state["generation"],
        "kickoffs_total": state["kickoffs_total"],
        "queue_held": state["queue_held"]["reason"],
    });
    eprintln!("A2_SUPERVISOR_TICK recovery_tick={recovery_tick} state={safe}");
}

fn recovery_kick_observed(logs: &LogSink, channel: ChannelId) -> bool {
    let fresh = format!("one-shot kick for channel {channel} (turn_presence_idle)");
    let channel_field = format!("channel_id={channel}");
    String::from_utf8(logs.0.lock().unwrap().clone())
        .unwrap()
        .lines()
        .any(|line| {
            line.contains(&fresh)
                || (line
                    .contains("kickoff already active for channel; coalescing duplicate request")
                    && line.split_whitespace().any(|field| field == channel_field)
                    && line.split_whitespace().any(|field| {
                        matches!(
                            field,
                            "reason=\"turn_presence_idle\"" | "reason=turn_presence_idle"
                        )
                    })
                    && line
                        .split_whitespace()
                        .any(|field| field == "immediate=true")
                    && line
                        .split_whitespace()
                        .any(|field| field == "wake_existing=true"))
        })
}

async fn assert_pair_untouched(fixture: &Fixture, a: u64, b: u64) {
    let state = fixture
        .rig
        .shared
        .mailbox(fixture.rig.channel)
        .snapshot()
        .await;
    assert!(state.cancel_token.is_none());
    assert!(state.pending_user_dispatch.is_none());
    assert_eq!(
        state
            .intervention_queue
            .iter()
            .map(|item| item.message_id.get())
            .collect::<Vec<_>>(),
        [a, b]
    );
    assert_eq!(fixture.disk_ids(), [a, b]);
    assert!(inflight::load_inflight_state(&fixture.provider, fixture.rig.channel.get()).is_none());
}

async fn baseline_idle_supervisor(fixture: &Fixture) -> Arc<LiveSupervisor> {
    let supervisor = Arc::new(LiveSupervisor::new(
        fixture.rig.shared.clone(),
        fixture.provider.clone(),
        fixture.rig.channel.get(),
    ));
    supervisor.tick().await;
    let state = supervisor.state();
    assert_eq!(state["activity"], "idle");
    assert_eq!(state["reason"], "no_session");
    assert_eq!(state["generation"], 1);
    assert_eq!(state["kickoffs_total"], 0);
    supervisor
}

async fn start_completion_listener(fixture: &Fixture) {
    let mailbox = fixture.rig.shared.mailbox(fixture.rig.channel);
    let empty = mailbox.snapshot().await;
    assert!(empty.intervention_queue.is_empty());
    assert!(empty.cancel_token.is_none());
    assert!(empty.pending_user_dispatch.is_none());
    let ready_before = crate::services::discord::queue_io::cancel_backstop_test_support::listener_completed_reconciles(fixture.rig.channel);
    let receivers_before = fixture.rig.shared.turn_completion_events.receiver_count();
    // The exact isolated child's Tokio runtime owns this void-returning production listener.
    crate::services::discord::queue_io::spawn_turn_completion_idle_queue_listener(
        fixture.rig.shared.clone(),
        fixture.provider.clone(),
    );
    assert!(fixture.rig.shared.turn_completion_events.receiver_count() > receivers_before);
    until(async || {
        crate::services::discord::queue_io::cancel_backstop_test_support::listener_completed_reconciles(
            fixture.rig.channel,
        ) > ready_before
    })
    .await;
}

struct DrainCase {
    ids: [u64; 2],
    began: tokio::time::Instant,
    label: &'static str,
}

struct RecoveryDriver {
    task: Option<tokio::task::JoinHandle<()>>,
    ticks: Arc<AtomicUsize>,
}

impl RecoveryDriver {
    fn start(supervisor: Arc<LiveSupervisor>) -> Self {
        // The first recovery tick has already completed with the same Idle generation.
        let ticks = Arc::new(AtomicUsize::new(1));
        let counted = ticks.clone();
        let task = tokio::spawn(async move {
            let period = Duration::from_secs(3);
            let mut interval =
                tokio::time::interval_at(tokio::time::Instant::now() + period, period);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                let tick = counted.fetch_add(1, Ordering::SeqCst) + 1;
                supervisor.tick().await;
                trace_supervisor_tick(&supervisor, tick);
            }
        });
        Self {
            task: Some(task),
            ticks,
        }
    }

    async fn stop(mut self) -> usize {
        let task = self.task.take().unwrap();
        task.abort();
        if let Err(error) = task.await {
            assert!(
                error.is_cancelled(),
                "live supervisor driver failed: {error}"
            );
        }
        self.ticks.load(Ordering::SeqCst)
    }
}

impl Drop for RecoveryDriver {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

async fn supervisor_drains_pair(
    fixture: &Fixture,
    supervisor: &Arc<LiveSupervisor>,
    logs: &LogSink,
    mut first: ProviderTurn,
    guard: &ProviderGuard,
    case: DrainCase,
) {
    let DrainCase {
        ids: [a, b],
        began,
        label,
    } = case;
    let _failure_logs = FailureLogs(logs.clone());
    let baseline = supervisor.state();
    assert_eq!(baseline["activity"], "idle");
    assert_eq!(baseline["generation"], 1);
    assert_eq!(baseline["kickoffs_total"], 0);
    supervisor.tick().await;
    let state = supervisor.state();
    assert_eq!(state["activity"], "idle");
    assert_eq!(state["reason"], "no_session");
    assert_eq!(state["generation"], baseline["generation"]);
    assert_eq!(state["kickoffs_total"], 1);
    trace_supervisor_tick(supervisor, 1);
    let driver = RecoveryDriver::start(supervisor.clone());
    let mut ledger = vec![first.wait(fixture, a).await];
    assert!(
        recovery_kick_observed(logs, fixture.rig.channel),
        "the real LiveEffects recovery kick origin was not observed"
    );
    let first_entry = began.elapsed();
    assert_eq!(fixture.disk_ids(), [b]);
    let state = fixture
        .rig
        .shared
        .mailbox(fixture.rig.channel)
        .snapshot()
        .await;
    assert_eq!(state.active_user_message_id, Some(MessageId::new(a)));
    assert_eq!(
        state
            .intervention_queue
            .iter()
            .map(|item| item.message_id.get())
            .collect::<Vec<_>>(),
        [b]
    );
    let mut second = ProviderTurn::arm(fixture.rig.channel.get());
    first.finish();
    ledger.push(second.wait(fixture, b).await);
    second.finish();
    fixture.assert_finished().await;
    let recovery_ticks = driver.stop().await;
    let drained = began.elapsed();
    assert!(
        drained <= DRAIN_LIMIT,
        "{label}: final drain exceeded six ticks: {drained:?}"
    );
    assert!(
        recovery_ticks <= 6,
        "{label}: recovery used {recovery_ticks} real ticks"
    );
    assert_eq!(ledger, [a, b]);
    assert_eq!(ledger.iter().filter(|id| **id == a).count(), 1);
    assert_eq!(ledger.iter().filter(|id| **id == b).count(), 1);
    assert_eq!(guard.0.load(Ordering::SeqCst), 0);
    eprintln!(
        "A2_SUPERVISOR_DRAIN case={label} entries={ledger:?} a_entries=1 b_entries=1 duplicates={} supervisor_ticks={} recovery_ticks={recovery_ticks} tick_ms=3000 supervisor_kickoffs={} first_entry_ms={} final_drain_ms={} limit_ms={}",
        guard.0.load(Ordering::SeqCst),
        1 + recovery_ticks,
        supervisor.state()["kickoffs_total"],
        first_entry.as_millis(),
        drained.as_millis(),
        DRAIN_LIMIT.as_millis(),
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a2_actual_foreign_watcher_release_mid_kick_read_is_superseded_then_supervisor_drains_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_actual_foreign_watcher_release_mid_kick_read_is_superseded_then_supervisor_drains_pg"
    )) {
        return;
    }
    let logs = capture_logs();
    let id = 69_184_131;
    let fixture = Fixture::connect_and_migrate_fixture(id, ProviderKind::Claude).await;
    fixture.produce_owners();
    let (target_before_release, release_target) =
        install_owner_race_pause(fixture.rig.channel, "watcher_before_release");
    let (target_after_release, finish_target) =
        install_owner_race_pause(fixture.rig.channel, "watcher_after_release");
    let target_watcher = fixture.attach_old_watcher().await;
    fixture.clear().await;
    reached(&target_before_release).await;
    assert!(
        fixture
            .rig
            .shared
            .tmux_watchers
            .channel_binding(&fixture.rig.channel)
            .is_some(),
        "actual clear precondition must retain the target watcher until its real release"
    );
    release_target.notify_one();
    reached(&target_after_release).await;
    assert!(
        fixture
            .rig
            .shared
            .tmux_watchers
            .channel_binding(&fixture.rig.channel)
            .is_none()
    );
    finish_target.notify_one();
    tokio::time::timeout(DRAIN_LIMIT, target_watcher)
        .await
        .expect("the actual target watcher did not finish after clear and registry release")
        .unwrap();
    {
        let core = fixture.rig.shared.core.lock().await;
        let session = core.sessions.get(&fixture.rig.channel).unwrap();
        assert!(session.cleared && session.channel_name.is_some());
    }
    assert!(fixture.disk_ids().is_empty());
    eprintln!(
        "A2_P5_ACTUAL_CLEAR core_basis_retained=true target_watcher_released=true prior_queue_empty=true"
    );
    let (before_release, release_watcher) =
        install_owner_race_pause(fixture.foreign_channel, "watcher_before_release");
    let (after_release, finish_watcher) =
        install_owner_race_pause(fixture.foreign_channel, "watcher_after_release");
    let mut watcher = ForeignWatcher::attach(&fixture).await;
    watcher.cancel.store(true, Ordering::SeqCst);
    reached(&before_release).await;
    assert!(
        fixture.foreign[0]
            .tmux_watchers
            .channel_binding(&fixture.foreign_channel)
            .is_some()
    );
    assert!(
        fixture
            .rig
            .shared
            .tmux_watchers
            .channel_binding(&fixture.rig.channel)
            .is_none()
    );
    assert_eq!(
        fixture.observed().await,
        observed(Activity::Idle, "no_session")
    );
    let supervisor = baseline_idle_supervisor(&fixture).await;
    start_completion_listener(&fixture).await;
    let (a, b) = (id + 201, id + 202);
    fixture.enqueue_pair(a, b).await;
    let guard = ProviderGuard::new(id);
    let mut first = ProviderTurn::arm(id);
    let (kick_entered, release_kick) =
        install_owner_race_pause(fixture.rig.channel, "kickoff_before_take");
    let began = tokio::time::Instant::now();
    crate::services::discord::queue_io::schedule_post_enqueue_idle_queue_kick(
        fixture.rig.shared.clone(),
        fixture.provider.clone(),
        fixture.rig.channel,
    );
    reached(&kick_entered).await;
    let (reading_entered, release_reading) =
        install_owner_race_pause(fixture.rig.channel, "activity_before_final_publication");
    release_kick.notify_one();
    tokio::time::timeout(DRAIN_LIMIT, reading_entered.notified())
        .await
        .expect("actual kick Activity read did not reach the final-publication phase");
    let epoch = crate::services::discord::tmux_watcher_registry::owner_observation_epoch();
    release_watcher.notify_one();
    reached(&after_release).await;
    assert!(
        fixture.foreign[0]
            .tmux_watchers
            .channel_binding(&fixture.foreign_channel)
            .is_none()
    );
    assert_ne!(
        crate::services::discord::tmux_watcher_registry::owner_observation_epoch(),
        epoch
    );
    finish_watcher.notify_one();
    watcher.finish().await;
    release_reading.notify_one();
    until(async || log_has(&logs, "dequeue_held", "superseded")).await;
    assert_pair_untouched(&fixture, a, b).await;
    assert!(matches!(
        first.entered.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    assert!(INPUT_EFFECT_PROBE.lock().unwrap().is_some());
    assert_eq!(guard.0.load(Ordering::SeqCst), 0);
    eprintln!(
        "A2_MIDREAD_DISCARD phase=actual_kick_final_publication initial=Unknown/superseded queue=[{a},{b}] provider_entries=0 actual_foreign_release=1"
    );
    supervisor_drains_pair(
        &fixture,
        &supervisor,
        &logs,
        first,
        &guard,
        DrainCase {
            ids: [a, b],
            began,
            label: "midread_foreign_release",
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a2_s1_no_core_stale_canonical_private_fixture_supervisor_drains_fifo_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_s1_no_core_stale_canonical_private_fixture_supervisor_drains_fifo_pg"
    )) {
        return;
    }
    let logs = capture_logs();
    let id = 69_184_132;
    let fixture = Fixture::connect_and_migrate_fixture(id, ProviderKind::Claude).await;
    fixture.produce_owners();
    let prior = fixture
        .rig
        .shared
        .core
        .lock()
        .await
        .sessions
        .remove(&fixture.rig.channel)
        .unwrap();
    let mut config = (*crate::config_live_reload::current().unwrap()).clone();
    let mut channel = config.agents[0].channels.get("claude").unwrap().clone();
    let crate::config::AgentChannel::Detailed(binding) = &mut channel else {
        panic!("private target must have a detailed TUI registration")
    };
    binding.name = None;
    binding.aliases.clear();
    binding.workspace = prior.current_path;
    config.agents[0].channels.insert("claude", channel);
    std::fs::write(
        crate::runtime_layout::config_file_path(fixture.rig.root.path()),
        serde_yaml::to_string(&config).unwrap(),
    )
    .unwrap();
    crate::config_live_reload::install(config);
    let old = ProviderKind::Claude.build_tmux_session_name(&format!("s1-restored-old-{id}"));
    let canonical =
        ProviderKind::Claude.build_tmux_session_name(&format!("s1-restored-canonical-{id}"));
    let transcript = fixture.rig.root.path().join("s1-restored-history.jsonl");
    std::fs::write(&transcript, "{}\n").unwrap();
    crate::services::discord::turn_presence::activity::tests::log_turn_source(
        &fixture.provider,
        id,
        &old,
        "restored-historical-execution",
        &transcript,
    );
    let key = adk_session::build_namespaced_session_key(
        &fixture.rig.shared.token_hash,
        &fixture.provider,
        &canonical,
    );
    let pool = fixture.rig.shared.pg_pool.as_ref().unwrap();
    inflight::seed_session_row_hashed(pool, &key, id, &fixture.rig.shared.token_hash, None).await;
    sqlx::query("UPDATE sessions SET status='idle', active_turn_nonce='stable-terminal-nonce' WHERE session_key=$1")
        .bind(&key).execute(pool).await.unwrap();
    let reading = reading_now(&fixture.rig.shared, &fixture.provider, fixture.rig.channel).await;
    assert_eq!(reading.observed, observed(Activity::Idle, "no_session"));
    let snapshot = reading.unbound.as_ref().unwrap().snapshot.as_ref().unwrap();
    assert!(snapshot.basis.is_none());
    assert_eq!(snapshot.files.recovered.as_ref(), Some(&canonical));
    let candidates = snapshot.files.candidates.as_ref().unwrap();
    assert_eq!(candidates.current.as_ref(), Some(&canonical));
    for name in [&old, &canonical] {
        assert!(
            candidates
                .names
                .iter()
                .any(|candidate| &candidate.name == name)
        );
    }
    assert!(reading.publish_if_current(|| ()).await.is_some());
    let retained: Option<String> =
        sqlx::query_scalar("SELECT active_turn_nonce FROM sessions WHERE session_key=$1")
            .bind(&key)
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(retained.as_deref(), Some("stable-terminal-nonce"));
    assert!(
        !fixture
            .rig
            .shared
            .core
            .lock()
            .await
            .sessions
            .contains_key(&fixture.rig.channel)
    );
    assert!(
        fixture
            .rig
            .shared
            .tmux_watchers
            .channel_binding(&fixture.rig.channel)
            .is_none()
    );
    let supervisor = baseline_idle_supervisor(&fixture).await;
    start_completion_listener(&fixture).await;
    let (a, b) = (id + 201, id + 202);
    fixture.enqueue_pair(a, b).await;
    assert_pair_untouched(&fixture, a, b).await;
    let guard = ProviderGuard::new(id);
    let first = ProviderTurn::arm(id);
    eprintln!(
        "A2_S1_PRIVATE_ABSENCE core=none historical={old} canonical={canonical} retained_terminal_nonce=1 inventory=actual_private_tmux"
    );
    supervisor_drains_pair(
        &fixture,
        &supervisor,
        &logs,
        first,
        &guard,
        DrainCase {
            ids: [a, b],
            began: tokio::time::Instant::now(),
            label: "s1_no_core_stale_canonical",
        },
    )
    .await;
}
