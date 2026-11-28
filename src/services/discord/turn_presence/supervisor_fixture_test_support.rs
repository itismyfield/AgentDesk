use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

use crate::services::discord::turn_presence::activity::Observed;

pub(in crate::services::discord) const BUSY: Observed = Observed {
    activity: Activity::Busy,
    reason: "open",
};

pub(in crate::services::discord) const IDLE: Observed = Observed {
    activity: Activity::Idle,
    reason: "closed",
};

pub(in crate::services::discord) const UNKNOWN: Observed = Observed {
    activity: Activity::Unknown,
    reason: "facts_halted",
};

pub(in crate::services::discord) struct LiveSupervisor {
    inner: Supervisor<LiveEffects>,
    provider: ProviderKind,
    channel: u64,
    _started: Started,
}

impl LiveSupervisor {
    pub(in crate::services::discord) const SUPERVISOR_TICK: Duration = super::TICK;
    pub(in crate::services::discord) const SUPERVISOR_WAKE_EVERY: Duration = super::WAKE_EVERY;

    pub(in crate::services::discord) fn new(
        shared: Arc<SharedData>,
        provider: ProviderKind,
        channel: u64,
    ) -> Self {
        register(&provider, &[channel]);
        let started = Started::new(&provider);
        let mut inner = Supervisor::new(Arc::new(LiveEffects {
            shared,
            provider: provider.clone(),
        }));
        inner.presence(channel);
        Self {
            inner,
            provider,
            channel,
            _started: started,
        }
    }

    pub(in crate::services::discord) async fn tick(&self) {
        self.inner.tick(Instant::now()).await;
    }

    /// Ticks as of `at`, so an Unknown first seen then is already past the notice grace now.
    pub(in crate::services::discord) async fn tick_at(&self, at: Instant) {
        self.inner.tick(at).await;
    }

    pub(in crate::services::discord) fn state(&self) -> serde_json::Value {
        lock(&self.inner.channels[&self.channel]).json()
    }

    pub(in crate::services::discord) fn last_kick_at(&self) -> Option<Instant> {
        lock(&self.inner.channels[&self.channel]).last_kick_at
    }

    pub(in crate::services::discord) fn woken_head(&self) -> Option<u64> {
        lock(&self.inner.channels[&self.channel])
            .woken_head
            .map(|(head, _)| head)
    }
}

impl Drop for LiveSupervisor {
    fn drop(&mut self) {
        let mut registered = lock(&REGISTERED);
        if let Some(channels) = registered.get_mut(self.provider.as_str()) {
            channels.remove(&self.channel);
            if channels.is_empty() {
                registered.remove(self.provider.as_str());
            }
        }
        drop(registered);
        let mut presence = lock(&PRESENCE);
        if presence
            .get(&self.channel)
            .is_some_and(|current| Arc::ptr_eq(current, &self.inner.channels[&self.channel]))
        {
            presence.remove(&self.channel);
        }
    }
}

/// Scripted reads and recorded sends, timed from the test's start on tokio's paused clock.
type Reader = Arc<dyn Fn(u64) -> Reading + Send + Sync>;

pub(in crate::services::discord) struct Fake {
    root: tempfile::TempDir,
    reader: Mutex<Option<Reader>>,
    start: Instant,
    channels: Mutex<Vec<u64>>,
    readings: Mutex<HashMap<u64, Observed>>,
    pub(in crate::services::discord) overtaken: AtomicBool,
    pub(in crate::services::discord) token: AtomicBool,
    pub(in crate::services::discord) head: Mutex<Option<u64>>,
    read_delay: Mutex<HashMap<u64, Duration>>,
    mailbox_delay: Mutex<HashMap<u64, Duration>>,
    pub(in crate::services::discord) typing_delay: Mutex<HashMap<u64, Duration>>,
    typing_errors: Mutex<HashMap<u64, String>>,
    typed: Mutex<Vec<(u64, Duration)>>,
    succeeded: Mutex<Vec<(u64, Duration)>>,
    kicks: Mutex<Vec<(u64, Duration)>>,
    pub(in crate::services::discord) owner_wakes: Mutex<Vec<(u64, Duration)>>,
    pub(in crate::services::discord) notices: Mutex<Vec<(u64, Duration, String)>>,
    pub(in crate::services::discord) notice_delay: Mutex<Option<Duration>>,
    notice_error: AtomicBool,
    notice_ok: Mutex<Vec<u64>>,
    pub(in crate::services::discord) gate_blocked: AtomicBool,
    /// When set, the mailbox answer comes from this runtime's real mailbox and disk markers.
    pub(in crate::services::discord) live_mailbox: Mutex<Option<Arc<SharedData>>>,
}

impl Fake {
    pub(in crate::services::discord) fn new(channels: &[u64], observed: Observed) -> Arc<Self> {
        Arc::new(Self {
            root: tempfile::tempdir().unwrap(),
            reader: Mutex::new(None),
            start: Instant::now(),
            channels: Mutex::new(channels.to_vec()),
            readings: Mutex::new(channels.iter().map(|&c| (c, observed)).collect()),
            overtaken: AtomicBool::new(false),
            token: AtomicBool::new(false),
            head: Mutex::new(None),
            read_delay: Mutex::new(HashMap::new()),
            mailbox_delay: Mutex::new(HashMap::new()),
            typing_delay: Mutex::new(HashMap::new()),
            typing_errors: Mutex::new(HashMap::new()),
            typed: Mutex::new(Vec::new()),
            succeeded: Mutex::new(Vec::new()),
            kicks: Mutex::new(Vec::new()),
            owner_wakes: Mutex::new(Vec::new()),
            notices: Mutex::new(Vec::new()),
            notice_delay: Mutex::new(None),
            notice_error: AtomicBool::new(false),
            notice_ok: Mutex::new(Vec::new()),
            gate_blocked: AtomicBool::new(false),
            live_mailbox: Mutex::new(None),
        })
    }

    pub(in crate::services::discord) fn set(&self, channel: u64, observed: Observed) {
        lock(&self.readings).insert(channel, observed);
    }

    fn since(&self) -> Duration {
        Instant::now().duration_since(self.start)
    }

    pub(in crate::services::discord) fn notice_secs(&self) -> Vec<u64> {
        lock(&self.notices).iter().map(|n| n.1.as_secs()).collect()
    }

    pub(in crate::services::discord) fn typed_secs(&self, channel: u64) -> Vec<u64> {
        let typed = lock(&self.typed);
        typed
            .iter()
            .filter(|t| t.0 == channel)
            .map(|t| t.1.as_secs())
            .collect()
    }

    pub(in crate::services::discord) fn kick_secs(&self) -> Vec<u64> {
        lock(&self.kicks).iter().map(|k| k.1.as_secs()).collect()
    }

    pub(in crate::services::discord) fn notices(&self) -> Vec<(u64, String)> {
        let notices = lock(&self.notices);
        notices
            .iter()
            .map(|n| (n.1.as_secs(), n.2.clone()))
            .collect()
    }
}

#[async_trait]
impl Effects for Fake {
    fn notice_scope(&self, channel: u64) -> notice_store::Scope {
        notice_store::Scope {
            root: Some(self.root.path().to_owned()),
            provider: "claude".into(),
            token_hash: "test".into(),
            channel,
        }
    }

    fn channels(&self) -> Vec<u64> {
        lock(&self.channels).clone()
    }

    async fn read(&self, channel: u64) -> Reading {
        let delay = lock(&self.read_delay).get(&channel).copied();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let reader = lock(&self.reader).clone();
        if let Some(reader) = reader {
            return reader(channel);
        }
        let observed = lock(&self.readings)[&channel];
        if self.overtaken.load(Ordering::SeqCst) {
            return Reading::overtaken_for_tests(observed);
        }
        Reading::unwatched_for_tests(observed, Some("pane"), Some("/t.jsonl"))
    }

    async fn mailbox(&self, channel: u64) -> (bool, Option<u64>, bool) {
        let delay = lock(&self.mailbox_delay).remove(&channel);
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        let live = lock(&self.live_mailbox).clone();
        if let Some(shared) = live {
            return waiting_input(&shared, &ProviderKind::Claude, ChannelId::new(channel)).await;
        }
        (self.token.load(Ordering::SeqCst), *lock(&self.head), false)
    }

    async fn typing(&self, channel: u64) -> Result<(), String> {
        lock(&self.typed).push((channel, self.since()));
        let delay = lock(&self.typing_delay).get(&channel).copied();
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        if let Some(error) = lock(&self.typing_errors).get(&channel).cloned() {
            return Err(error);
        }
        lock(&self.succeeded).push((channel, self.since()));
        Ok(())
    }

    fn kickoff(&self, channel: u64) {
        lock(&self.kicks).push((channel, self.since()));
    }

    fn wake_owner(&self, channel: u64) {
        lock(&self.owner_wakes).push((channel, self.since()));
    }

    fn idle(&self, channel: u64) {
        if let Some(shared) = lock(&self.live_mailbox).clone() {
            LiveEffects {
                shared,
                provider: ProviderKind::Claude,
            }
            .idle(channel);
        }
    }

    async fn legacy_gate_blocked(&self, _: u64) -> bool {
        self.gate_blocked.load(Ordering::SeqCst)
    }

    async fn notice(&self, channel: u64, text: String) -> Result<(), String> {
        lock(&self.notices).push((channel, self.since(), text));
        let delay = *lock(&self.notice_delay);
        if let Some(delay) = delay {
            tokio::time::sleep(delay).await;
        }
        if self.notice_error.load(Ordering::SeqCst) {
            return Err("notice unavailable".into());
        }
        lock(&self.notice_ok).push(self.since().as_secs());
        Ok(())
    }

    async fn inject_target(&self, _: u64) -> Option<(String, String)> {
        None
    }
}

#[path = "supervisor_deadline_tests.rs"]
mod deadline_tests;

#[path = "supervisor_injected_tests.rs"]
mod injected_tests;

/// Publishes a presence for `channel` as a running supervisor would.
pub(crate) fn seed_presence_for_tests(channel: u64, busy: bool) {
    let mut presence = Presence::new();
    presence.activity = if busy {
        Activity::Busy
    } else {
        Activity::Unknown
    };
    lock(&PRESENCE).insert(channel, Arc::new(Mutex::new(presence)));
}

pub(crate) fn seed_long_unknown_for_tests(channel: u64, reason: &'static str, age_secs: u64) {
    lock(&PRESENCE).remove(&channel);
    let mut presence = Presence::new();
    presence.reason = reason;
    presence.observed_at = Some(Utc::now());
    let owner = notices::Owner::get(notice_store::Scope {
        root: crate::services::discord::runtime_store::runtime_root(),
        provider: "claude".into(),
        token_hash: "health-test".into(),
        channel,
    });
    let reading = Reading::unwatched_for_tests(
        Observed {
            activity: Activity::Unknown,
            reason,
        },
        None,
        None,
    );
    owner.observe(
        Some(reading.notice_observation()),
        false,
        None,
        Instant::now() - Duration::from_secs(age_secs),
    );
    presence.notice = Some(owner);
    presence.activity_since = Utc::now() - chrono::Duration::seconds(age_secs as i64);
    lock(&PRESENCE).insert(channel, Arc::new(Mutex::new(presence)));
}

#[path = "supervisor_worker_tests.rs"]
mod worker_tests;

#[path = "supervisor_notice_tests.rs"]
mod notice_tests;

// Uses the unix-only activity test probe (`activity::tests`).
#[cfg(unix)]
#[path = "supervisor_reader_notice_tests.rs"]
mod reader_notice_tests;

/// Runs the supervisor over `fake` for the life of the returned handle.
pub(in crate::services::discord) fn supervise(fake: &Arc<Fake>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(Supervisor::new(fake.clone()).run())
}

pub(in crate::services::discord) async fn wait(secs: u64) {
    tokio::time::sleep(Duration::from_secs(secs)).await;
}

pub(in crate::services::discord) fn typing_counts(channel: u64) -> (u64, u64) {
    let json = status(channel).expect("turn-mode channel");
    let typing = &json["typing"];
    let count = |name: &str| typing[name].as_u64().unwrap();
    (count("sent_ok_total"), count("late_ok_total"))
}

/// Ticks once and waits for the channel's notice drive, which loads and posts off the tick.
pub(in crate::services::discord) async fn tick_settled(
    supervisor: &Supervisor<Fake>,
    channel: u64,
) {
    supervisor.tick(Instant::now()).await;
    let owner = lock(&supervisor.channels[&channel]).notice.clone().unwrap();
    owner.settled_for_tests().await;
}

pub(in crate::services::discord) struct NoticeProbe {
    fake: Arc<Fake>,
    inner: Supervisor<Fake>,
    reading: Arc<Mutex<Option<Reading>>>,
    channel: u64,
}

impl NoticeProbe {
    pub(in crate::services::discord) fn new(channel: u64) -> Self {
        let fake = Fake::new(&[channel], BUSY);
        *lock(&fake.head) = Some(7);
        let reading = Arc::new(Mutex::new(None));
        let next = reading.clone();
        *lock(&fake.reader) = Some(Arc::new(move |_| lock(&next).take().expect("live reading")));
        let mut inner = Supervisor::new(fake.clone());
        inner.presence(channel);
        Self {
            fake,
            inner,
            reading,
            channel,
        }
    }
    pub(in crate::services::discord) async fn tick(&self, reading: Reading, now: Instant) {
        *lock(&self.reading) = Some(reading);
        self.inner.tick(now).await;
        let owner = lock(&self.inner.channels[&self.channel])
            .notice
            .clone()
            .unwrap();
        owner.settled_for_tests().await;
    }
    pub(in crate::services::discord) fn notice_count(&self) -> usize {
        self.fake.notices().len()
    }
}
