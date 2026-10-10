//! One supervisor per provider runtime over its confirmed turn-mode channels: typing while a direct
//! turn reads busy, one notice when it stays unknown, and queue wakeups while it reads idle.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use poise::serenity_prelude::ChannelId;
use tokio::task::AbortHandle;
use tokio::time::Instant;

use super::activity::{Activity, Reading, reading_now};
use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;

const TICK: Duration = Duration::from_secs(3);
const TYPING_EVERY: Duration = Duration::from_secs(8);
const TYPING_RETRY: Duration = Duration::from_secs(30);
const UNKNOWN_NOTICE_AFTER: Duration = Duration::from_secs(60);
const NOTICE_EVERY: Duration = Duration::from_secs(30 * 60);
const WAKE_EVERY: Duration = Duration::from_secs(30);
/// Wakes on the same queue head before the supervisor asks why it does not move.
const HELD_AFTER_WAKES: u32 = 2;
const HELD_NOTICE_AFTER: Duration = Duration::from_secs(120);

/// Channels each provider runtime confirmed into turn mode.
static REGISTERED: LazyLock<Mutex<HashMap<String, BTreeSet<u64>>>> =
    LazyLock::new(Default::default);
/// What each supervised channel last published, for the turn endpoint.
static PRESENCE: LazyLock<Mutex<HashMap<u64, Arc<Mutex<Presence>>>>> =
    LazyLock::new(Default::default);
static PROCESS_STARTED: LazyLock<DateTime<Utc>> = LazyLock::new(Utc::now);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Adds channels a confirmation point returned; only their names are kept.
pub(in crate::services::discord) fn register(provider: &ProviderKind, channels: &[u64]) {
    if channels.is_empty() {
        return;
    }
    let mut registered = lock(&REGISTERED);
    let set = registered.entry(provider.as_str().to_string()).or_default();
    set.extend(channels.iter().copied().filter(|&channel| channel != 0));
}

fn registered(provider: &ProviderKind) -> Vec<u64> {
    let registered = lock(&REGISTERED);
    let set = registered.get(provider.as_str());
    let turns = crate::services::tui_o::turn_mode::transcript_turns;
    set.map(|set| set.iter().copied().filter(|&c| turns(c)).collect())
        .unwrap_or_default()
}

/// Spawns the provider's supervisor; call once its bot token is set so no send precedes HTTP.
pub(in crate::services::discord) fn spawn(shared: Arc<SharedData>, provider: ProviderKind) {
    LazyLock::force(&PROCESS_STARTED);
    let effects = Arc::new(LiveEffects { shared, provider });
    tokio::spawn(Supervisor::new(effects).run());
}

/// The turn endpoint's `turn_presence` block, or `None` outside turn mode.
pub(crate) fn status(channel: u64) -> Option<serde_json::Value> {
    if !crate::services::tui_o::turn_mode::transcript_turns(channel) {
        return None;
    }
    let presence = lock(&PRESENCE).get(&channel).cloned()?;
    let mut status = { lock(&presence).json() };
    status["n1_observation"] =
        serde_json::to_value(crate::services::tui_o::n1_observation::snapshot(channel))
            .unwrap_or_else(|_| serde_json::json!({"status": "unavailable"}));
    Some(status)
}

#[derive(Debug)]
struct Presence {
    activity: Activity,
    reason: &'static str,
    observed_at: Option<DateTime<Utc>>,
    session: Option<String>,
    source: Option<String>,
    activity_since: DateTime<Utc>,
    /// Moves on every activity transition; a send from an older generation counts nothing.
    generation: u64,
    next_typing_at: Option<Instant>,
    sending: Option<AbortHandle>,
    sent_ok_total: u64,
    late_ok_total: u64,
    last_ok_at: Option<DateTime<Utc>>,
    last_error: Option<String>,
    unknown_since: Option<Instant>,
    last_notice_at: Option<Instant>,
    notice_posted_at: Option<DateTime<Utc>>,
    kickoffs_total: u64,
    last_kick_at: Option<Instant>,
    /// The queue head the latest wakes were for and how many there were.
    woken_head: Option<(u64, u32)>,
    queue_held: Option<(&'static str, DateTime<Utc>, Instant)>,
    held_noticed: bool,
    mismatch: Option<(String, String)>,
}

impl Presence {
    fn new() -> Self {
        Self {
            activity: Activity::Unknown,
            reason: "not_observed",
            observed_at: None,
            session: None,
            source: None,
            activity_since: Utc::now(),
            generation: 0,
            next_typing_at: None,
            sending: None,
            sent_ok_total: 0,
            late_ok_total: 0,
            last_ok_at: None,
            last_error: None,
            unknown_since: None,
            last_notice_at: None,
            notice_posted_at: None,
            kickoffs_total: 0,
            last_kick_at: None,
            woken_head: None,
            queue_held: None,
            held_noticed: false,
            mismatch: None,
        }
    }

    fn json(&self) -> serde_json::Value {
        let held = self.queue_held.as_ref();
        serde_json::json!({
            "activity": name(self.activity),
            "reason": self.reason,
            "observed_at": self.observed_at,
            "session": self.session,
            "source_id": self.source,
            "activity_since": self.activity_since,
            "generation": self.generation,
            "typing": {
                "sent_ok_total": self.sent_ok_total,
                "late_ok_total": self.late_ok_total,
                "last_ok_at": self.last_ok_at,
                "last_error": self.last_error,
            },
            "unknown_notice": {"posted_at": self.notice_posted_at},
            "kickoffs_total": self.kickoffs_total,
            "queue_held": {
                "reason": held.map(|held| held.0),
                "since": held.map(|held| held.1),
            },
            "process_started_at": *PROCESS_STARTED,
        })
    }

    fn may_notice(&self, now: Instant) -> bool {
        self.last_notice_at
            .is_none_or(|at| now.duration_since(at) >= NOTICE_EVERY)
    }
}

/// What a published observation asks the supervisor to do after releasing its locks.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Typing(u64),
    Kickoff,
    CheckHeld(u64),
    Notice(String),
}

/// Everything the supervisor reads or sends outside its own state; tests script it.
#[async_trait]
trait Effects: Send + Sync + 'static {
    fn channels(&self) -> Vec<u64>;
    async fn read(&self, channel: u64) -> Reading;
    /// Whether a mailbox token holds the channel, and its oldest waiting input.
    async fn mailbox(&self, channel: u64) -> (bool, Option<u64>);
    async fn typing(&self, channel: u64) -> Result<(), String>;
    fn kickoff(&self, channel: u64);
    async fn legacy_gate_blocked(&self, channel: u64) -> bool;
    async fn notice(&self, channel: u64, text: String) -> Result<(), String>;
    /// The tmux session and transcript a busy inject would pick, by the same candidate order.
    async fn inject_target(&self, channel: u64) -> Option<(String, String)>;
}

struct Supervisor<E: Effects> {
    effects: Arc<E>,
    channels: HashMap<u64, Arc<Mutex<Presence>>>,
}

impl<E: Effects> Supervisor<E> {
    fn new(effects: Arc<E>) -> Self {
        Self {
            effects,
            channels: HashMap::new(),
        }
    }

    async fn run(mut self) {
        let mut next_tick = Instant::now();
        loop {
            let now = Instant::now();
            if now >= next_tick {
                self.tick(now).await;
                next_tick = now + TICK;
            }
            self.send_due(Instant::now()).await;
            let deadline = self
                .earliest_deadline()
                .map_or(next_tick, |d| d.min(next_tick));
            tokio::time::sleep_until(deadline).await;
        }
    }

    fn presence(&mut self, channel: u64) -> Arc<Mutex<Presence>> {
        let presence = self.channels.entry(channel).or_insert_with(|| {
            let presence = Arc::new(Mutex::new(Presence::new()));
            lock(&PRESENCE).insert(channel, presence.clone());
            presence
        });
        presence.clone()
    }

    fn earliest_deadline(&self) -> Option<Instant> {
        let due = |presence: &Arc<Mutex<Presence>>| {
            let presence = lock(presence);
            presence
                .next_typing_at
                .filter(|_| presence.sending.is_none())
        };
        self.channels.values().filter_map(due).min()
    }

    async fn tick(&mut self, now: Instant) {
        for channel in self.effects.channels() {
            let presence = self.presence(channel);
            let reading = self.effects.read(channel).await;
            let (token, head) = self.effects.mailbox(channel).await;
            // A judgment a later read or rebuild already moved past is never published.
            let published = reading.publish_if_current(|| {
                let mut presence = lock(&presence);
                apply(&mut presence, channel, now, &reading, token, head)
            });
            let Some(actions) = published else {
                tracing::debug!(channel, "[turn_presence] superseded reading not published");
                continue;
            };
            self.watch_target(channel, &presence, &reading).await;
            for action in actions {
                self.act(channel, &presence, action).await;
            }
        }
    }

    /// Re-sends typing at each due deadline; a mailbox token's own turn types for itself.
    async fn send_due(&mut self, now: Instant) {
        let channels: Vec<_> = self.channels.iter().map(|(c, p)| (*c, p.clone())).collect();
        for (channel, presence) in channels {
            let due = {
                let presence = lock(&presence);
                presence.activity == Activity::Busy
                    && presence.sending.is_none()
                    && presence.next_typing_at.is_some_and(|at| at <= now)
            };
            if !due {
                continue;
            }
            let (token, _) = self.effects.mailbox(channel).await;
            let mut guard = lock(&presence);
            if token {
                guard.next_typing_at = Some(now + TYPING_EVERY);
                continue;
            }
            let generation = guard.generation;
            drop(guard);
            self.start_typing(channel, &presence, generation, now);
        }
    }

    fn start_typing(
        &self,
        channel: u64,
        presence: &Arc<Mutex<Presence>>,
        generation: u64,
        at: Instant,
    ) {
        let (effects, state) = (self.effects.clone(), presence.clone());
        let mut guard = lock(presence);
        guard.next_typing_at = None;
        let task = tokio::spawn(async move {
            let sent = effects.typing(channel).await;
            let mut presence = lock(&state);
            typed(&mut presence, channel, generation, at, sent);
        });
        guard.sending = Some(task.abort_handle());
    }

    async fn act(&self, channel: u64, presence: &Arc<Mutex<Presence>>, action: Action) {
        match action {
            Action::Typing(generation) => {
                self.start_typing(channel, presence, generation, Instant::now());
            }
            Action::Kickoff => {
                tracing::info!(channel, "[turn_presence] kickoff channel={channel}");
                self.effects.kickoff(channel);
            }
            Action::CheckHeld(head) => {
                let reason = match self.effects.legacy_gate_blocked(channel).await {
                    true => "legacy_tui_gate",
                    false => "unknown",
                };
                let mut presence = lock(presence);
                let still = presence.activity == Activity::Idle
                    && presence.woken_head.is_some_and(|(h, _)| h == head);
                if still && presence.queue_held.is_none() {
                    tracing::warn!(
                        channel,
                        "[turn_presence] queue_held channel={channel} reason={reason}"
                    );
                    presence.queue_held = Some((reason, Utc::now(), Instant::now()));
                }
            }
            // Posted off the tick like typing, so a slow notice holds no other channel.
            Action::Notice(text) => {
                tracing::warn!(channel, "[turn_presence] unknown_notice channel={channel}");
                let effects = self.effects.clone();
                tokio::spawn(async move {
                    if let Err(error) = effects.notice(channel, text).await {
                        tracing::warn!(channel, %error, "[turn_presence] notice failed");
                    }
                });
            }
        }
    }

    /// Logs once per change when the busy inject would pick another pane or transcript.
    async fn watch_target(&self, channel: u64, presence: &Arc<Mutex<Presence>>, reading: &Reading) {
        let (Some(session), Some(source)) = (reading.session(), reading.source()) else {
            return;
        };
        let Some(inject) = self.effects.inject_target(channel).await else {
            return;
        };
        let ours = (session.to_string(), source.to_string());
        let mut presence = lock(presence);
        let differs = inject != ours;
        if differs && presence.mismatch.as_ref() != Some(&inject) {
            tracing::warn!(
                channel,
                "[turn_presence] target_mismatch channel={channel} pr1={}:{} c1={}:{}",
                inject.0,
                inject.1,
                ours.0,
                ours.1
            );
        }
        presence.mismatch = differs.then_some(inject);
    }
}

/// The lowercase activity name the endpoint and log markers share.
fn name(activity: Activity) -> String {
    format!("{activity:?}").to_lowercase()
}

/// Applies one published observation and returns what to do outside the locks.
fn apply(
    presence: &mut Presence,
    channel: u64,
    now: Instant,
    reading: &Reading,
    token: bool,
    head: Option<u64>,
) -> Vec<Action> {
    let observed = reading.observed;
    let mut actions = Vec::new();
    let first = presence.observed_at.is_none();
    let moved = presence.activity != observed.activity;
    presence.observed_at = Some(Utc::now());
    presence.reason = observed.reason;
    presence.session = reading.session().map(str::to_string);
    presence.source = reading.source().map(str::to_string);
    if moved || first {
        tracing::info!(
            channel,
            "[turn_presence] activity channel={channel} from={} to={} reason={} session={}",
            name(presence.activity),
            name(observed.activity),
            observed.reason,
            presence.session.as_deref().unwrap_or("-")
        );
        presence.generation += 1;
        if let Some(sending) = presence.sending.take() {
            sending.abort();
        }
        presence.next_typing_at = None;
        presence.activity = observed.activity;
        presence.activity_since = Utc::now();
        presence.unknown_since = (observed.activity == Activity::Unknown).then_some(now);
        presence.woken_head = None;
        presence.queue_held = None;
        presence.held_noticed = false;
        match observed.activity {
            Activity::Busy if token => presence.next_typing_at = Some(now + TYPING_EVERY),
            Activity::Busy => actions.push(Action::Typing(presence.generation)),
            Activity::Idle | Activity::Unknown => {}
        }
    }
    match observed.activity {
        Activity::Busy => {}
        Activity::Unknown => {
            let since = presence.unknown_since.unwrap_or(now);
            if now.duration_since(since) >= UNKNOWN_NOTICE_AFTER && presence.may_notice(now) {
                presence.last_notice_at = Some(now);
                presence.notice_posted_at = Some(Utc::now());
                actions.push(Action::Notice(unknown_notice(observed.reason)));
            }
        }
        Activity::Idle => wake(presence, now, moved || first, token, head, &mut actions),
    }
    actions
}

/// Level-triggered: an idle channel with a queue and no token is woken at once on the transition
/// and every 30 seconds after; a head that two wakes did not move is looked into once.
fn wake(
    presence: &mut Presence,
    now: Instant,
    turned_idle: bool,
    token: bool,
    head: Option<u64>,
    actions: &mut Vec<Action>,
) {
    let Some(head) = head else {
        (presence.woken_head, presence.queue_held) = (None, None);
        presence.held_noticed = false;
        return;
    };
    if presence.woken_head.is_some_and(|(h, _)| h != head) {
        (presence.woken_head, presence.queue_held) = (None, None);
        presence.held_noticed = false;
    }
    // Counted before this tick's wake, so the check runs on a tick after the second wake.
    let wakes = presence.woken_head.map_or(0, |(_, n)| n);
    if wakes >= HELD_AFTER_WAKES && presence.queue_held.is_none() {
        actions.push(Action::CheckHeld(head));
    }
    let due = presence
        .last_kick_at
        .is_none_or(|at| now.duration_since(at) >= WAKE_EVERY);
    if !token && (turned_idle || due) {
        presence.last_kick_at = Some(now);
        presence.kickoffs_total += 1;
        presence.woken_head = Some((head, wakes + 1));
        actions.push(Action::Kickoff);
    }
    if let Some((reason, _, since)) = presence.queue_held
        && now.duration_since(since) >= HELD_NOTICE_AFTER
        && !presence.held_noticed
        && presence.may_notice(now)
    {
        presence.held_noticed = true;
        presence.last_notice_at = Some(now);
        presence.notice_posted_at = Some(Utc::now());
        actions.push(Action::Notice(held_notice(reason)));
    }
}

/// Counts a send only for the generation that started it while the channel still reads busy.
fn typed(
    presence: &mut Presence,
    channel: u64,
    generation: u64,
    at: Instant,
    sent: Result<(), String>,
) {
    let current = presence.generation == generation && presence.activity == Activity::Busy;
    if !current {
        presence.late_ok_total += u64::from(sent.is_ok());
        return;
    }
    presence.sending = None;
    match sent {
        Ok(()) => {
            tracing::info!(
                channel,
                "[turn_presence] typing_sent channel={channel} status=2xx gen={generation}"
            );
            presence.sent_ok_total += 1;
            presence.last_ok_at = Some(Utc::now());
            presence.next_typing_at = Some(at + TYPING_EVERY);
        }
        Err(error) => {
            tracing::warn!(
                channel,
                "[turn_presence] typing_sent channel={channel} status={error} gen={generation}"
            );
            presence.last_error = Some(error);
            presence.next_typing_at = Some(Instant::now() + TYPING_RETRY);
        }
    }
}

fn clock() -> String {
    chrono::Local::now().format("%H:%M").to_string()
}

fn unknown_notice(reason: &str) -> String {
    format!(
        "⚠ {} 터미널 턴 상태를 확인하지 못해 진행 표시를 멈췄습니다 (사유: {reason}). 그 동안 들어온 외부 입력은 대기열에 보관됩니다.",
        clock()
    )
}

fn held_notice(reason: &str) -> String {
    match reason {
        "legacy_tui_gate" => format!(
            "⚠ {} 진행 표시는 끝났지만 대기열이 Legacy TUI 판정에 막혀 있습니다.",
            clock()
        ),
        _ => format!("⚠ {} 대기열이 진행되지 않아 확인이 필요합니다.", clock()),
    }
}

/// The mailbox token and the oldest waiting input: queue head, in-memory reservation, then the
/// on-disk pending marker a failed requeue can leave behind. Read only; kickoff decides the start.
async fn waiting_input(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: ChannelId,
) -> (bool, Option<u64>) {
    let snapshot = super::super::mailbox_snapshot(shared, channel).await;
    let head = snapshot.intervention_queue.first().map(|i| i.message_id);
    let waiting = head.or(snapshot.pending_user_dispatch).or_else(|| {
        let load = crate::services::turn_orchestrator::load_channel_pending_dispatch_marker;
        load(provider, &shared.token_hash, channel).map(|(marker, _)| marker.message_id)
    });
    (snapshot.cancel_token.is_some(), waiting.map(|id| id.get()))
}

struct LiveEffects {
    shared: Arc<SharedData>,
    provider: ProviderKind,
}

#[async_trait]
impl Effects for LiveEffects {
    fn channels(&self) -> Vec<u64> {
        registered(&self.provider)
    }

    async fn read(&self, channel: u64) -> Reading {
        reading_now(&self.shared, &self.provider, ChannelId::new(channel)).await
    }

    async fn mailbox(&self, channel: u64) -> (bool, Option<u64>) {
        waiting_input(&self.shared, &self.provider, ChannelId::new(channel)).await
    }

    async fn typing(&self, channel: u64) -> Result<(), String> {
        let http = self.shared.serenity_http_or_token_fallback();
        let http = http.ok_or_else(|| "no http".to_string())?;
        let broadcast = ChannelId::new(channel).broadcast_typing(&http);
        broadcast.await.map_err(|error| error.to_string())
    }

    fn kickoff(&self, channel: u64) {
        crate::services::discord::schedule_deferred_idle_queue_kickoff_immediate(
            self.shared.clone(),
            self.provider.clone(),
            ChannelId::new(channel),
            "turn_presence_idle",
        );
    }

    async fn legacy_gate_blocked(&self, channel: u64) -> bool {
        let blocked = crate::services::discord::router::hosted_tui_promote_readiness_blocked;
        blocked(&self.shared, &self.provider, ChannelId::new(channel)).await
    }

    async fn notice(&self, channel: u64, text: String) -> Result<(), String> {
        let http = self.shared.serenity_http_or_token_fallback();
        let http = http.ok_or_else(|| "no http".to_string())?;
        let said = ChannelId::new(channel).say(&*http, text).await;
        said.map(|_| ()).map_err(|error| error.to_string())
    }

    async fn inject_target(&self, channel: u64) -> Option<(String, String)> {
        if self.provider != ProviderKind::Claude {
            return None;
        }
        let channel_id = ChannelId::new(channel);
        let watcher = self.shared.tmux_watchers.channel_binding(&channel_id);
        let read = crate::services::discord::inflight::load_inflight_state_read_only;
        let row = read(&self.provider, channel);
        let named = {
            let core = self.shared.core.lock().await;
            let session = core.sessions.get(&channel_id);
            session.and_then(|session| session.channel_name.clone())
        };
        let candidates = [
            watcher.map(|binding| binding.tmux_session_name),
            row.and_then(|row| row.tmux_session_name),
            named.map(|name| self.provider.build_tmux_session_name(&name)),
        ];
        let binding = crate::services::tui_prompt_dedupe::runtime_binding_for_tmux_session;
        candidates.into_iter().flatten().find_map(|session| {
            let found = binding(&session)?;
            Some((session, found.relay_output_path().to_string()))
        })
    }
}

#[cfg(test)]
#[path = "supervisor_tests.rs"]
pub(in crate::services::discord) mod tests;
