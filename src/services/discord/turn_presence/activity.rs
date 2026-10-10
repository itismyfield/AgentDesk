//! Fresh read of a confirmed turn-mode channel's own transcript: idle, busy or unknown.
//! Effect points only poll within a chunk budget; binding folds and prefix hashes run on a worker.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use poise::serenity_prelude::ChannelId;

use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;
use crate::services::provider::session_probe::SessionLiveness;
#[cfg(all(test, unix))]
use crate::services::tui_o::shadow::SourceId;
use crate::services::tui_o::shadow::capture::MAX_PARTIAL_BYTES;
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding};
use crate::services::tui_o::writer::adoption::{Hold, logged};
use crate::services::tui_o::writer::binding::{BindingEvent, BindingEvents, ChannelBindingLog};
use crate::services::tui_o::writer::input_facts::{InputFacts, Resume, TurnState};

/// New bytes one effect-point poll may read before it answers `catching_up`.
const CHUNK_BUDGET: u64 = 4 * 1024 * 1024;
/// A larger transcript is never rebuilt; it reads Unknown until the session changes.
const MAX_TRANSCRIPT_BYTES: u64 = 256 * 1024 * 1024;
/// An open turn whose transcript has not grown for this long needs the pane to read busy.
const STALE_OPEN_AFTER: Duration = Duration::from_secs(600);
const UNREADABLE_RETRY_AFTER: Duration = Duration::from_secs(30);
/// A worker poll this large either completes a record or stops at a trailing partial line.
const WORKER_BUDGET: u64 = MAX_PARTIAL_BYTES as u64 + 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum Activity {
    Idle,
    Busy,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) struct Observed {
    pub activity: Activity,
    pub reason: &'static str,
}

const fn observed(activity: Activity, reason: &'static str) -> Observed {
    Observed { activity, reason }
}

/// Which tmux session the channel's watcher names, or the one a start would create.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Bound(String),
    Unbound(Option<String>),
}

/// A rebuild is valid only for the provider, session and binding seq it read.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    channel: u64,
    provider: ShadowProvider,
    session: String,
    seq: u64,
}

enum Outcome {
    Rebuilding,
    Source {
        facts: Box<InputFacts>,
        from_start: bool,
    },
    NoSource,
    Pending,
    Unreadable {
        retry_at: Instant,
    },
    Halted,
    TooLarge,
}

struct Watch {
    key: Option<Key>,
    generation: u64,
    /// Moves whenever an effect-point poll reads new records, halts or finds bytes left unread.
    revision: u64,
    outcome: Outcome,
    through: u64,
    grew_at: Instant,
}

impl Default for Watch {
    fn default() -> Self {
        Self {
            key: None,
            generation: 0,
            revision: 0,
            outcome: Outcome::Rebuilding,
            through: 0,
            grew_at: Instant::now(),
        }
    }
}

type Job = Box<dyn FnOnce() + Send>;

/// Everything the probe reads outside its own state; tests script it.
trait Ports: Send + Sync + 'static {
    fn binding_seq(&self, channel: u64) -> Result<u64, String>;
    fn binding_events(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> Result<Vec<BindingEvent>, String>;
    /// Present unless tmux confirms the session is missing.
    fn session_present(&self, session: &str) -> bool;
    fn final_ready(&self, session: &str) -> bool;
    fn pane_busy(&self, session: &str) -> bool;
    fn liveness(&self, provider: ShadowProvider, channel: u64, session: &str) -> SessionLiveness;
    /// False when the worker could not start.
    fn spawn(&self, job: Job) -> bool;
}

static WATCHES: LazyLock<Mutex<HashMap<u64, Arc<Mutex<Watch>>>>> = LazyLock::new(Default::default);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// An unstamped fresh result still supersedes earlier Busy effect approvals.
fn supersede_unstamped(watch: &Mutex<Watch>) {
    lock(watch).revision += 1;
}

/// Judged fresh for an effect point; callers ask only for confirmed turn-mode channels.
pub(in crate::services::discord) async fn activity_now(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Observed {
    reading_now(shared, provider, channel).await.observed
}

/// What a judgment was made on, so a later publisher can tell whether it still holds.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    key: Key,
    generation: u64,
    revision: u64,
    source: Option<String>,
    #[cfg(all(test, unix))]
    source_id: Option<SourceId>,
}

/// One fresh judgment and the watch state it read.
pub(in crate::services::discord) struct Reading {
    pub observed: Observed,
    stamp: Option<Stamp>,
    watch: Option<Arc<Mutex<Watch>>>,
    #[cfg(all(test, unix))]
    host_checked: bool,
}

impl Reading {
    /// Supplies the receiver's binding identity; the bot id comes from its verified HTTP client.
    #[cfg(all(test, unix))]
    pub(super) fn identity(&self, bot_id: u64) -> Option<super::admission::Identity> {
        if bot_id == 0 {
            return None;
        }
        let stamp = self.stamp.as_ref()?;
        let identity = super::admission::Identity {
            provider: stamp.key.provider,
            channel: stamp.key.channel,
            session: stamp.key.session.clone(),
            source: stamp.source_id.as_ref()?.clone(),
            binding_seq: stamp.key.seq,
            bot_id,
        };
        self.with_busy(&identity, || ())?;
        Some(identity)
    }

    /// Unlike observation publication, effect admission requires a live, stamped Busy source.
    #[cfg(all(test, unix))]
    pub(super) fn with_busy<R>(
        &self,
        identity: &super::admission::Identity,
        hand_off: impl FnOnce() -> R,
    ) -> Option<R> {
        let (Some(stamp), Some(watch)) = (&self.stamp, &self.watch) else {
            return None;
        };
        if !self.host_checked
            || self.observed.activity != Activity::Busy
            || stamp.key.channel != identity.channel
            || stamp.key.provider != identity.provider
            || stamp.key.seq != identity.binding_seq
            || stamp.key.session != identity.session
            || stamp.source_id.as_ref() != Some(&identity.source)
        {
            return None;
        }
        let guard = lock(watch);
        let current = guard.key.as_ref() == Some(&stamp.key)
            && (guard.generation, guard.revision) == (stamp.generation, stamp.revision);
        current.then(hand_off)
    }
    pub(in crate::services::discord) fn session(&self) -> Option<&str> {
        self.stamp.as_ref().map(|stamp| stamp.key.session.as_str())
    }

    pub(in crate::services::discord) fn source(&self) -> Option<&str> {
        self.stamp
            .as_ref()
            .and_then(|stamp| stamp.source.as_deref())
    }

    /// Runs `publish` inside the watch's lock only while its key, generation and revision are
    /// still the ones this judgment read; a reading made before any lock holds nothing to check.
    pub(in crate::services::discord) fn publish_if_current<R>(
        &self,
        publish: impl FnOnce() -> R,
    ) -> Option<R> {
        let (Some(stamp), Some(watch)) = (&self.stamp, &self.watch) else {
            return Some(publish());
        };
        let guard = lock(watch);
        let current = guard.key.as_ref() == Some(&stamp.key)
            && (guard.generation, guard.revision) == (stamp.generation, stamp.revision);
        current.then(publish)
    }
}

#[cfg(test)]
impl Reading {
    /// A judgment with no watch behind it, published unconditionally.
    pub(in crate::services::discord) fn unwatched_for_tests(
        observed: Observed,
        session: Option<&str>,
        source: Option<&str>,
    ) -> Self {
        let stamp = session.map(|session| Stamp {
            key: Key {
                channel: 0,
                provider: ShadowProvider::Claude,
                session: session.into(),
                seq: 0,
            },
            generation: 0,
            revision: 0,
            source: source.map(str::to_string),
            #[cfg(all(test, unix))]
            source_id: None,
        });
        Self {
            observed,
            stamp,
            watch: None,
            #[cfg(all(test, unix))]
            host_checked: false,
        }
    }

    /// A judgment whose watch has since moved one revision on under the same key and generation.
    pub(in crate::services::discord) fn overtaken_for_tests(observed: Observed) -> Self {
        let key = Key {
            channel: 0,
            provider: ShadowProvider::Claude,
            session: "overtaken".into(),
            seq: 1,
        };
        let watch = Watch {
            key: Some(key.clone()),
            generation: 1,
            revision: 1,
            ..Watch::default()
        };
        let stamp = Stamp {
            key,
            generation: 1,
            revision: 0,
            source: None,
            #[cfg(all(test, unix))]
            source_id: None,
        };
        Self {
            observed,
            stamp: Some(stamp),
            watch: Some(Arc::new(Mutex::new(watch))),
            #[cfg(all(test, unix))]
            host_checked: false,
        }
    }
}

type Answer = (Observed, Option<Stamp>);

/// The answer and the state it was decided on, read under the same lock.
fn at(guard: &Watch, answer: Observed) -> Answer {
    let source = match &guard.outcome {
        Outcome::Source { facts, .. } => Some(facts.binding().source.path.display().to_string()),
        _ => None,
    };
    let stamp = guard.key.clone().map(|key| Stamp {
        key,
        generation: guard.generation,
        revision: guard.revision,
        source,
        #[cfg(all(test, unix))]
        source_id: match &guard.outcome {
            Outcome::Source { facts, .. } => Some(facts.binding().source.clone()),
            _ => None,
        },
    });
    (answer, stamp)
}

/// A fresh judgment with the state it read, for publishers outside the effect points.
pub(in crate::services::discord) async fn reading_now(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Reading {
    read_now(shared, provider, channel, false).await
}

/// Dormant presence observer; existing input and supervisor observers retain their policy.
#[cfg(all(test, unix))]
pub(super) async fn presence_reading_now(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    ticket: &super::lifecycle::Ticket,
) -> Option<Reading> {
    if ticket.channel() != channel.get() || ticket.incarnation().is_none() {
        return None;
    }
    let reading = read_now(shared, provider, channel, true).await;
    ticket.with_current(|_| reading)
}

async fn read_now(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    host_checked: bool,
) -> Reading {
    let unstamped = |observed| {
        if host_checked {
            let watch = lock(&WATCHES).get(&channel.get()).cloned();
            if let Some(watch) = watch {
                supersede_unstamped(&watch);
            }
        }
        Reading {
            observed,
            stamp: None,
            watch: None,
            #[cfg(all(test, unix))]
            host_checked,
        }
    };
    let shadow = match provider {
        ProviderKind::Claude => ShadowProvider::Claude,
        ProviderKind::Codex => ShadowProvider::Codex,
        _ => return unstamped(observed(Activity::Unknown, "provider_unsupported")),
    };
    let target = match shared.tmux_watchers.channel_binding(&channel) {
        Some(watcher) if watcher.owner_channel_id == channel => {
            Target::Bound(watcher.tmux_session_name)
        }
        _ => {
            let core = shared.core.lock().await;
            let name = core
                .sessions
                .get(&channel)
                .and_then(|s| s.channel_name.clone());
            Target::Unbound(name.map(|name| provider.build_tmux_session_name(&name)))
        }
    };
    let watch = lock(&WATCHES).entry(channel.get()).or_default().clone();
    let ports: Arc<dyn Ports> = Arc::new(LivePorts {
        #[cfg(test)]
        root: crate::services::tui_prompt_dedupe::binding_events::test_root(),
    });
    let held = watch.clone();
    let judged = tokio::task::spawn_blocking(move || {
        if host_checked {
            judge_presence(&held, &ports, shadow, channel.get(), target)
        } else {
            judge(&held, &ports, shadow, channel.get(), target)
        }
    });
    match judged.await {
        Ok((observed, stamp)) => Reading {
            observed,
            stamp,
            watch: Some(watch),
            #[cfg(all(test, unix))]
            host_checked,
        },
        Err(_) => unstamped(observed(Activity::Unknown, "probe_failed")),
    }
}

#[cfg(test)]
fn observe(
    watch: &Arc<Mutex<Watch>>,
    ports: &Arc<dyn Ports>,
    provider: ShadowProvider,
    channel: u64,
    target: Target,
) -> Observed {
    judge(watch, ports, provider, channel, target).0
}

fn judge(
    watch: &Arc<Mutex<Watch>>,
    ports: &Arc<dyn Ports>,
    provider: ShadowProvider,
    channel: u64,
    target: Target,
) -> Answer {
    judge_with_policy(watch, ports, provider, channel, target, false)
}

fn judge_with_policy(
    watch: &Arc<Mutex<Watch>>,
    ports: &Arc<dyn Ports>,
    provider: ShadowProvider,
    channel: u64,
    target: Target,
    strict: bool,
) -> Answer {
    let session = match target {
        Target::Bound(session) => session,
        Target::Unbound(Some(session)) if ports.session_present(&session) => {
            return (observed(Activity::Unknown, "watcher_unbound"), None);
        }
        Target::Unbound(_) => return (observed(Activity::Idle, "no_session"), None),
    };
    let seq = match ports.binding_seq(channel) {
        Ok(seq) => seq,
        Err(_) => return (observed(Activity::Unknown, "binding_unreadable"), None),
    };
    let key = Key {
        channel,
        provider,
        session,
        seq,
    };
    let mut guard = lock(watch);
    let retry =
        matches!(guard.outcome, Outcome::Unreadable { retry_at } if Instant::now() >= retry_at);
    if guard.key.as_ref() != Some(&key) || retry {
        guard.key = Some(key.clone());
        return rebuild(watch, guard, ports, channel, key, None);
    }
    let stored = |reason| observed(Activity::Unknown, reason);
    let (facts, from_start) = match &mut guard.outcome {
        Outcome::Rebuilding => return at(&guard, stored("catching_up")),
        Outcome::NoSource => {
            return ask_pane(watch, guard, &key, strict, || {
                no_turn_evidence(ports.as_ref(), &key)
            });
        }
        Outcome::Pending => return at(&guard, stored("binding_pending")),
        Outcome::Unreadable { .. } => return at(&guard, stored("binding_unreadable")),
        Outcome::Halted => return at(&guard, stored("facts_halted")),
        Outcome::TooLarge => return at(&guard, stored("transcript_too_large")),
        Outcome::Source { facts, from_start } => (facts, *from_start),
    };
    let polled = facts.poll(CHUNK_BUDGET).map(|fact| {
        let caught_up = facts.caught_up().unwrap_or(false);
        let flags = (facts.awaiting_boundary(), facts.saw_turn_evidence());
        (fact, caught_up, flags)
    });
    let resume = polled.as_ref().err().and_then(|_| {
        let resume = facts.resume_point().cloned()?;
        Some((facts.binding().clone(), resume))
    });
    let (fact, caught_up, (awaiting, evidence)) = match (polled, resume) {
        (Ok(polled), _) => polled,
        (Err(_), Some(resume)) => {
            let (_, stamp) = rebuild(watch, guard, ports, channel, key, Some(resume));
            return (observed(Activity::Unknown, "facts_resumed"), stamp);
        }
        (Err(_), _) => {
            guard.outcome = Outcome::Halted;
            guard.revision += 1;
            return at(&guard, observed(Activity::Unknown, "facts_halted"));
        }
    };
    let advanced = fact.through > guard.through;
    if advanced {
        (guard.through, guard.grew_at) = (fact.through, Instant::now());
    }
    // New records, or bytes this poll saw but has not finished, both supersede an earlier pane read.
    if advanced || !caught_up {
        guard.revision += 1;
    }
    if !caught_up {
        return at(&guard, observed(Activity::Unknown, "catching_up"));
    }
    match fact.state {
        TurnState::Idle => at(&guard, observed(Activity::Idle, "closed")),
        TurnState::Open { .. } if guard.grew_at.elapsed() < STALE_OPEN_AFTER => {
            at(&guard, observed(Activity::Busy, "open"))
        }
        TurnState::Open { .. } if provider == ShadowProvider::Claude => {
            ask_pane(watch, guard, &key, strict, || {
                match ports.pane_busy(&key.session) {
                    true => observed(Activity::Busy, "open_pane_busy"),
                    false => observed(Activity::Unknown, "open_without_progress"),
                }
            })
        }
        TurnState::Open { .. } => at(&guard, observed(Activity::Unknown, "open_without_progress")),
        TurnState::Unknown if awaiting => at(&guard, observed(Activity::Unknown, "facts_resumed")),
        TurnState::Unknown if from_start && !evidence => {
            ask_pane(watch, guard, &key, strict, || {
                no_turn_evidence(ports.as_ref(), &key)
            })
        }
        TurnState::Unknown => at(&guard, observed(Activity::Unknown, "no_turn_boundary")),
    }
}

fn judge_presence(
    watch: &Arc<Mutex<Watch>>,
    ports: &Arc<dyn Ports>,
    provider: ShadowProvider,
    channel: u64,
    target: Target,
) -> Answer {
    let (answer, stamp) = judge_with_policy(watch, ports, provider, channel, target, true);
    let Some(stamp) = stamp else {
        supersede_unstamped(watch);
        return (answer, None);
    };
    if answer.activity == Activity::Unknown {
        let mut guard = lock(watch);
        if guard.key.as_ref() == Some(&stamp.key)
            && (guard.generation, guard.revision) == (stamp.generation, stamp.revision)
        {
            guard.revision += 1;
            return at(&guard, answer);
        }
        return (observed(Activity::Unknown, "superseded"), Some(stamp));
    }
    let liveness = ports.liveness(provider, channel, &stamp.key.session);
    let mut guard = lock(watch);
    if guard.key.as_ref() != Some(&stamp.key)
        || (guard.generation, guard.revision) != (stamp.generation, stamp.revision)
    {
        return (observed(Activity::Unknown, "superseded"), Some(stamp));
    }
    let answer = match liveness {
        SessionLiveness::Alive => answer,
        SessionLiveness::Missing => observed(Activity::Unknown, "host_dead"),
        SessionLiveness::ProbeFailed => observed(Activity::Unknown, "host_probe_failed"),
        SessionLiveness::Unknown => observed(Activity::Unknown, "host_unobservable"),
    };
    if liveness != SessionLiveness::Alive {
        guard.revision += 1;
        return at(&guard, answer);
    }
    (answer, Some(stamp))
}

/// Reads the pane outside the channel's lock; if the key, generation or read moved on meanwhile the
/// answer reads unknown, so a superseded read never grants a start.
fn ask_pane(
    watch: &Mutex<Watch>,
    guard: MutexGuard<'_, Watch>,
    key: &Key,
    strict: bool,
    ask: impl FnOnce() -> Observed,
) -> Answer {
    let read = (guard.generation, guard.revision);
    let original = strict.then(|| at(&guard, observed(Activity::Unknown, "superseded")));
    drop(guard);
    let answer = ask();
    let guard = lock(watch);
    match guard.key.as_ref() == Some(key) && (guard.generation, guard.revision) == read {
        true => at(&guard, answer),
        false => original.unwrap_or_else(|| at(&guard, observed(Activity::Unknown, "superseded"))),
    }
}

/// A session with no turn evidence is idle only for a Claude pane that passes the final send check.
fn no_turn_evidence(ports: &dyn Ports, key: &Key) -> Observed {
    match key.provider == ShadowProvider::Claude && ports.final_ready(&key.session) {
        true => observed(Activity::Idle, "no_turn_evidence_ready"),
        false => observed(Activity::Unknown, "no_turn_evidence"),
    }
}

/// Starts a new generation and its worker after releasing the channel's lock.
fn rebuild(
    watch: &Arc<Mutex<Watch>>,
    mut guard: MutexGuard<'_, Watch>,
    ports: &Arc<dyn Ports>,
    channel: u64,
    key: Key,
    resume: Option<(SourceBinding, Resume)>,
) -> Answer {
    guard.generation += 1;
    guard.outcome = Outcome::Rebuilding;
    let generation = guard.generation;
    let answer = at(&guard, observed(Activity::Unknown, "catching_up"));
    drop(guard);
    let (worker, worker_ports, failed) = (watch.clone(), ports.clone(), key.clone());
    let spawned = ports.spawn(Box::new(move || {
        let (outcome, grown) = match resume {
            Some((binding, resume)) => read_from(InputFacts::resume(binding, &resume), false),
            None => locate(worker_ports.as_ref(), channel, &key),
        };
        install(&worker, &key, generation, outcome, grown);
    }));
    if !spawned {
        let retry_at = Instant::now() + UNREADABLE_RETRY_AFTER;
        install(
            watch,
            &failed,
            generation,
            Outcome::Unreadable { retry_at },
            None,
        );
    }
    answer
}

/// Only the generation that asked may install, checked in the same critical section.
fn install(watch: &Mutex<Watch>, key: &Key, generation: u64, outcome: Outcome, grown: Grown) {
    let mut guard = lock(watch);
    if guard.key.as_ref() == Some(key) && guard.generation == generation {
        guard.outcome = outcome;
        if let Some((through, grew_at)) = grown {
            (guard.through, guard.grew_at) = (through, grew_at);
        }
    }
}

type Grown = Option<(u64, Instant)>;

fn locate(ports: &dyn Ports, channel: u64, key: &Key) -> (Outcome, Grown) {
    let unreadable = || Outcome::Unreadable {
        retry_at: Instant::now() + UNREADABLE_RETRY_AFTER,
    };
    let Ok(events) = ports.binding_events(channel, key.provider) else {
        return (unreadable(), None);
    };
    let events: Vec<_> = events
        .into_iter()
        .filter(|event| event.tmux_session == key.session)
        .collect();
    let source = match logged(&events) {
        Ok((sources, _)) => sources.last().map(|source| (*source).clone()),
        Err(refused) if refused.hold == Hold::Binding => return (Outcome::Pending, None),
        Err(refused) if refused.hold == Hold::Final => return (Outcome::NoSource, None),
        Err(_) => None,
    };
    // Only a log that binds nothing reads as no source; a bound file gone missing may come back.
    let Some(source) = source else {
        return (unreadable(), None);
    };
    match std::fs::metadata(&source.path) {
        Err(_) => (unreadable(), None),
        Ok(meta) if meta.len() > MAX_TRANSCRIPT_BYTES => (Outcome::TooLarge, None),
        Ok(_) => {
            let binding = SourceBinding {
                channel_id: channel,
                provider: key.provider,
                source,
            };
            match InputFacts::open(binding) {
                Ok(facts) => read_from(Ok(facts), true),
                Err(_) => (unreadable(), None),
            }
        }
    }
}

/// Reads to the end, resuming past each typed `Blocked` record; any other error halts.
fn read_from(facts: Result<InputFacts, String>, mut from_start: bool) -> (Outcome, Grown) {
    let Ok(mut facts) = facts else {
        return (Outcome::Halted, None);
    };
    let mut through = 0;
    loop {
        match facts.poll(WORKER_BUDGET) {
            Err(_) => {
                let Some(resume) = facts.resume_point().cloned() else {
                    return (Outcome::Halted, None);
                };
                match InputFacts::resume(facts.binding().clone(), &resume) {
                    Ok(next) => (facts, from_start, through) = (next, false, 0),
                    Err(_) => return (Outcome::Halted, None),
                }
            }
            Ok(fact) if fact.through > through => through = fact.through,
            // A trailing partial line stays for the effect point to finish.
            Ok(_) => break,
        }
        if facts.caught_up().unwrap_or(false) {
            break;
        }
    }
    let grown = modified_at(&facts).map(|at| (through, at));
    let facts = Box::new(facts);
    (Outcome::Source { facts, from_start }, grown)
}

/// The file's last write as a monotonic instant, so a restart does not read an old turn as fresh.
fn modified_at(facts: &InputFacts) -> Option<Instant> {
    let modified = std::fs::metadata(&facts.binding().source.path)
        .and_then(|meta| meta.modified())
        .ok()?;
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default();
    Instant::now().checked_sub(age)
}

struct LivePorts {
    #[cfg(test)]
    root: Option<std::path::PathBuf>,
}

impl LivePorts {
    #[cfg(not(test))]
    fn scoped<R>(&self, read: impl FnOnce() -> R) -> R {
        read()
    }

    /// Test binding logs live under a per-thread root, so each read re-enters it.
    #[cfg(test)]
    fn scoped<R>(&self, read: impl FnOnce() -> R) -> R {
        use crate::services::tui_prompt_dedupe::binding_events as p5;
        let saved = p5::test_root();
        p5::set_test_root(self.root.as_deref());
        let value = read();
        p5::set_test_root(saved.as_deref());
        value
    }
}

impl Ports for LivePorts {
    fn liveness(&self, provider: ShadowProvider, channel: u64, session: &str) -> SessionLiveness {
        let provider = match provider {
            ShadowProvider::Claude => ProviderKind::Claude,
            ShadowProvider::Codex => ProviderKind::Codex,
        };
        self.scoped(|| {
            let row = crate::services::discord::inflight::load_inflight_state_read_only_result(
                &provider, channel,
            );
            match row {
                Ok(row) => {
                    crate::services::discord::host_liveness::observe_liveness(session, row.as_ref())
                }
                Err(_) => SessionLiveness::ProbeFailed,
            }
        })
    }
    fn binding_seq(&self, channel: u64) -> Result<u64, String> {
        use crate::services::tui_prompt_dedupe::binding_events::subscribe_binding_events;
        self.scoped(|| subscribe_binding_events(channel))
            .map(|seq| *seq.borrow())
            .map_err(|e| e.to_string())
    }

    fn binding_events(
        &self,
        channel: u64,
        provider: ShadowProvider,
    ) -> Result<Vec<BindingEvent>, String> {
        let log = ChannelBindingLog::new(channel, provider);
        self.scoped(|| log.binding_events_since(channel, 0))
    }

    fn session_present(&self, session: &str) -> bool {
        use crate::services::platform::tmux::{SessionPresence, session_presence};
        session_presence(session) != SessionPresence::Missing
    }

    fn final_ready(&self, session: &str) -> bool {
        #[cfg(all(test, unix))]
        if tests::pane_ready_for_tests(session) {
            return true;
        }
        use crate::services::claude_tui::input::{final_prompt_ready, prompt_readiness_snapshot};
        final_prompt_ready(&prompt_readiness_snapshot(session))
    }

    fn pane_busy(&self, session: &str) -> bool {
        crate::services::platform::tmux::capture_pane(session, -40).is_some_and(|pane| {
            crate::services::tmux_common::tmux_capture_indicates_claude_tui_busy(&pane)
        })
    }

    fn spawn(&self, job: Job) -> bool {
        #[cfg(test)]
        let job: Job = {
            let ports = LivePorts {
                root: self.root.clone(),
            };
            Box::new(move || ports.scoped(job))
        };
        let spawned = std::thread::Builder::new()
            .name("turn-presence-rebuild".into())
            .spawn(job);
        if let Err(error) = &spawned {
            tracing::warn!(%error, "turn presence rebuild could not start");
        }
        spawned.is_ok()
    }
}

#[cfg(all(test, unix))]
#[path = "activity_tests.rs"]
pub(in crate::services::discord) mod tests;
