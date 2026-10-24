use super::*;
use crate::services::discord::commands::control::codex_clear_relaunch_tests::settled_owner_observation;
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::turn_presence::activity::{Reading, reading_now};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

fn isolated(test: &str) -> bool {
    const CHILD: &str = "ADK_A2_SUPERSEDED_MEASUREMENT_CHILD";
    let test = test
        .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
        .unwrap_or(test);
    if std::env::var(CHILD).ok().as_deref() == Some(test) {
        return true;
    }
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(CHILD, test)
        .env(
            crate::services::tui_o::cutover::test_override::CHILD_ENV,
            "1",
        )
        .env(
            crate::services::tui_o::cutover::test_override::CHANNELS_ENV,
            "[]",
        )
        .env_remove("TMUX")
        .env_remove("TMUX_PANE")
        .env_remove("AGENTDESK_CONFIG")
        .output()
        .unwrap();
    let logs = format!(
        "{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
    eprintln!("{logs}");
    assert!(result.status.success(), "{logs}");
    assert!(
        logs.contains("1 passed; 0 failed; 0 ignored;"),
        "measurement child did not execute exactly one test: {logs}"
    );
    false
}

#[derive(Clone, Default)]
struct LogSink(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for LogSink {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        lock(&self.0).extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl LogSink {
    fn events(&self) -> Vec<(String, u64)> {
        let text = String::from_utf8(lock(&self.0).clone()).unwrap();
        text.lines()
            .filter(|line| line.contains("unbound observation superseded"))
            .map(|line| {
                let reason = line
                    .split("reason=\"")
                    .nth(1)
                    .and_then(|value| value.split('"').next())
                    .expect("superseded event omitted its reason")
                    .to_string();
                let count = line
                    .split("superseded_count=")
                    .nth(1)
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse().ok())
                    .expect("superseded event omitted its existing counter");
                (reason, count)
            })
            .collect()
    }
}

async fn registered_rig(id: u64) -> (Rig, Arc<HealthRegistry>) {
    let mut rig = Rig::new(id, false, false).await;
    let registry = Arc::new(HealthRegistry::new());
    Arc::get_mut(&mut rig.shared).unwrap().health_registry = Arc::downgrade(&registry);
    registry.register("claude".into(), rig.shared.clone()).await;
    (rig, registry)
}

async fn idle_reading(rig: &Rig) -> Reading {
    let reading = settled_owner_observation(
        "A2_STABLE_OBSERVATION superseded_retries",
        || reading_now(&rig.shared, &ProviderKind::Claude, rig.channel),
        |sample| sample.observed.reason == "superseded",
    )
    .await;
    assert_eq!(reading.observed, observed(Activity::Idle, "no_session"));
    reading
}

async fn fresh_idle(rig: &Rig) {
    assert!(
        idle_reading(rig)
            .await
            .publish_if_current(|| ())
            .await
            .is_some()
    );
}

async fn rejected_without_callback(reading: &Reading) {
    let callbacks = AtomicUsize::new(0);
    assert!(
        reading
            .publish_if_current(|| callbacks.fetch_add(1, Ordering::SeqCst))
            .await
            .is_none(),
        "changed saved Reading unexpectedly published"
    );
    assert_eq!(callbacks.load(Ordering::SeqCst), 0);
}

struct ProvidersFence {
    release: std::sync::mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl ProvidersFence {
    async fn hold(registry: Arc<HealthRegistry>) -> Self {
        let generation = registry.unbound_runtime_snapshot().await.generation;
        let (entered_send, entered) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            registry
                .with_unbound_membership(generation, || {
                    entered_send.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(20)).unwrap();
                })
                .unwrap();
        });
        entered.recv_timeout(Duration::from_secs(20)).unwrap();
        Self {
            release,
            thread: Some(thread),
        }
    }

    fn hold_watch(watch: Arc<Mutex<Watch>>) -> Self {
        let (entered_send, entered) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _watch = lock(&watch);
            entered_send.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(20)).unwrap();
        });
        entered.recv_timeout(Duration::from_secs(20)).unwrap();
        Self {
            release,
            thread: Some(thread),
        }
    }
}
impl Drop for ProvidersFence {
    fn drop(&mut self) {
        let _ = self.release.send(());
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a2_s2_existing_superseded_counters_cover_saved_reading_rejections_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_s2_existing_superseded_counters_cover_saved_reading_rejections_pg"
    )) {
        return;
    }
    let (rig, registry) = registered_rig(69_185_302).await;
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(rig.channel.get());
    let sink = LogSink::default();
    let writer = sink.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let _capture = tracing::subscriber::set_default(subscriber);

    let reading = idle_reading(&rig).await;
    let unrelated = crate::services::discord::make_shared_data_for_tests_with_storage(None);
    assert!(
        unrelated
            .tmux_watchers
            .restore_owner_channel_for_tmux_session(
                "a2-counter-unrelated-owner",
                ChannelId::new(rig.channel.get() + 1),
            )
    );
    rejected_without_callback(&reading).await;
    fresh_idle(&rig).await;

    let reading = idle_reading(&rig).await;
    let mut added = crate::services::discord::make_shared_data_for_tests_with_storage(
        rig.shared.pg_pool.clone(),
    );
    let mutable = Arc::get_mut(&mut added).unwrap();
    mutable.provider = ProviderKind::Codex;
    mutable.token_hash = "a2-counter-added-runtime".into();
    mutable.health_registry = Arc::downgrade(&registry);
    registry.register("codex".into(), added).await;
    rejected_without_callback(&reading).await;
    fresh_idle(&rig).await;

    let reading = idle_reading(&rig).await;
    let providers = ProvidersFence::hold(registry.clone()).await;
    rejected_without_callback(&reading).await;
    drop(providers);
    fresh_idle(&rig).await;

    let reading = idle_reading(&rig).await;
    let watch = reading.watch.as_ref().expect("actual Reading has no watch");
    let held_watch = ProvidersFence::hold_watch(watch.clone());
    rejected_without_callback(&reading).await;
    drop(held_watch);
    fresh_idle(&rig).await;

    let events = sink.events();
    let mut summary = BTreeMap::new();
    for reason in ["owner_epoch", "membership", "providers_busy", "watch_busy"] {
        let matching: Vec<_> = events.iter().filter(|(found, _)| found == reason).collect();
        assert!(
            !matching.is_empty(),
            "missing real superseded event: {reason}"
        );
        assert!(matching.iter().all(|(_, count)| *count > 0));
        summary.insert(reason, (matching.len(), matching.last().unwrap().1));
    }
    eprintln!("A2_S2_EXISTING_SUPERSEDED_COUNTERS {summary:?} callbacks=0 fresh_idle=4");
}

#[derive(Default)]
struct Sample {
    attempts: usize,
    idle: usize,
    superseded: usize,
    other_hold: usize,
    elapsed_ms: u128,
    latencies_us: Vec<u128>,
    reasons: BTreeMap<String, usize>,
}
impl Sample {
    async fn collect(rig: &Rig) -> Self {
        let mut sample = Self::default();
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(2) {
            let attempt = Instant::now();
            let reading = reading_now(&rig.shared, &ProviderKind::Claude, rig.channel).await;
            let published = reading.publish_if_current(|| ()).await.is_some();
            sample.attempts += 1;
            *sample
                .reasons
                .entry(reading.observed.reason.into())
                .or_default() += 1;
            if !published || reading.observed.reason == "superseded" {
                sample.superseded += 1;
                if !published {
                    *sample
                        .reasons
                        .entry("publication_rejected".into())
                        .or_default() += 1;
                }
            } else if reading.observed.activity == Activity::Idle {
                sample.idle += 1;
            } else {
                sample.other_hold += 1;
            }
            sample.latencies_us.push(attempt.elapsed().as_micros());
            tokio::task::yield_now().await;
        }
        sample.elapsed_ms = start.elapsed().as_millis();
        sample
    }

    fn report(&mut self, phase: &str, mutations: usize, events: &[(String, u64)]) {
        self.latencies_us.sort_unstable();
        let mut event_counts = BTreeMap::<&str, usize>::new();
        let mut latest_counters = BTreeMap::<&str, u64>::new();
        for (reason, count) in events {
            *event_counts.entry(reason).or_default() += 1;
            latest_counters.insert(reason, *count);
        }
        assert!(self.attempts > 0);
        assert_eq!(self.attempts, self.idle + self.superseded + self.other_hold);
        eprintln!(
            "A2_S2_OWNER_CHURN phase={phase} requested_ms=2000 interval_ms=100 attempts={} idle={} superseded={} other_hold={} elapsed_ms={} mutations={mutations} latency_us_min={} latency_us_median={} latency_us_max={} reading_reasons={:?} superseded_event_counts={event_counts:?} superseded_counter_latest={latest_counters:?}",
            self.attempts,
            self.idle,
            self.superseded,
            self.other_hold,
            self.elapsed_ms,
            self.latencies_us[0],
            self.latencies_us[self.latencies_us.len() / 2],
            self.latencies_us.last().unwrap(),
            self.reasons,
        );
    }
}

struct OwnerChurn {
    stop: Arc<AtomicBool>,
    mutations: Arc<AtomicUsize>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl OwnerChurn {
    fn start(channel: u64) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let mutations = Arc::new(AtomicUsize::new(0));
        let (started_send, started) = std::sync::mpsc::channel();
        let (stop_thread, count_thread) = (stop.clone(), mutations.clone());
        let thread = std::thread::spawn(move || {
            let unrelated = crate::services::discord::make_shared_data_for_tests_with_storage(None);
            let mut sequence = 0;
            while !stop_thread.load(Ordering::SeqCst) {
                let owner = ChannelId::new(channel + 1 + sequence % 2);
                if unrelated
                    .tmux_watchers
                    .restore_owner_channel_for_tmux_session("a2-measure-unrelated-owner", owner)
                {
                    count_thread.fetch_add(1, Ordering::SeqCst);
                }
                if sequence == 0 {
                    started_send.send(()).unwrap();
                }
                sequence += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        started.recv_timeout(Duration::from_secs(20)).unwrap();
        Self {
            stop,
            mutations,
            thread: Some(thread),
        }
    }

    fn finish(mut self) -> usize {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
        self.mutations.load(Ordering::SeqCst)
    }
}
impl Drop for OwnerChurn {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn a2_s2_unrelated_owner_churn_measures_read_availability_pg() {
    if !isolated(concat!(
        module_path!(),
        "::a2_s2_unrelated_owner_churn_measures_read_availability_pg"
    )) {
        return;
    }
    let (rig, _registry) = registered_rig(69_185_303).await;
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(rig.channel.get());
    let sink = LogSink::default();
    let writer = sink.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .with_writer(move || writer.clone())
        .finish();
    crate::logging::test_capture::pin_callsite_interest();
    let _capture = tracing::subscriber::set_default(subscriber);
    fresh_idle(&rig).await;
    let baseline_at = sink.events().len();
    let mut baseline = Sample::collect(&rig).await;
    assert!(baseline.idle > 0, "baseline did not publish a real Idle");
    let events = sink.events();
    baseline.report("baseline", 0, &events[baseline_at..]);

    let churn_at = events.len();
    let churn = OwnerChurn::start(rig.channel.get());
    let mut sample = Sample::collect(&rig).await;
    let mutations = churn.finish();
    assert!(
        mutations > 0,
        "owner churn never changed an actual registry"
    );
    sample.report("churn", mutations, &sink.events()[churn_at..]);
    fresh_idle(&rig).await;
}
