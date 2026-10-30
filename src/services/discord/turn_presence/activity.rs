//! Fresh read of a confirmed turn-mode channel's own transcript: idle, busy or unknown.
//! Effect points only poll within a chunk budget; binding folds and prefix hashes run on a worker.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use poise::serenity_prelude::ChannelId;

use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;
use crate::services::provider::session_probe::SessionLiveness;
use crate::services::tui_o::shadow::capture::MAX_PARTIAL_BYTES;
use crate::services::tui_o::shadow::{ShadowProvider, SourceBinding};
use crate::services::tui_o::writer::adoption::{Hold, logged};
use crate::services::tui_o::writer::binding::{BindingEvent, BindingEvents, ChannelBindingLog};
use crate::services::tui_o::writer::input_facts::{InputFacts, Resume, TurnState};

pub(in crate::services::discord) mod notice_identity;
use notice_identity::{NoticeObservation, progress};

/// New bytes one effect-point poll may read before it answers `catching_up`.
const CHUNK_BUDGET: u64 = 4 * 1024 * 1024;
/// Cold sources above this boundary use bounded observation when enabled.
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
    #[cfg(all(test, unix))]
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
    Tail(Box<tail::TailReader>),
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
    notice: NoticeObservation,
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
            notice: NoticeObservation::legacy("not_observed", ""),
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
    #[cfg(all(test, unix))]
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
    let _effect = effect_reads::begin(channel.get());
    reading_now(shared, provider, channel).await.observed
}

/// What a judgment was made on, so a later publisher can tell whether it still holds.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    key: Key,
    generation: u64,
    revision: u64,
    source: Option<String>,
    notice: NoticeObservation,
    closed: Option<ClosedBoundary>,
}

mod closed_boundary;
pub(in crate::services::discord) use closed_boundary::ClosedBoundary;

/// One fresh judgment and the watch state it read.
pub(in crate::services::discord) struct Reading {
    pub observed: Observed,
    notice: NoticeObservation,
    stamp: Option<Stamp>,
    watch: Option<Arc<Mutex<Watch>>>,
    #[cfg(unix)]
    unbound: Option<unbound::live::Stamp>,
    #[cfg(unix)]
    admission: Option<unbound::live::inflight_bound::Admission>,
}

impl Reading {
    pub(in crate::services::discord) fn notice_observation(&self) -> NoticeObservation {
        self.notice.clone()
    }

    fn bound(
        observed: Observed,
        stamp: Option<Stamp>,
        watch: Option<Arc<Mutex<Watch>>>,
        label: Option<&str>,
    ) -> Self {
        let notice = stamp
            .as_ref()
            .map(|s| s.notice.clone())
            .unwrap_or_else(|| NoticeObservation::bound(observed.reason, label));
        Self {
            observed,
            notice,
            stamp,
            watch,
            #[cfg(unix)]
            unbound: None,
            #[cfg(unix)]
            admission: None,
        }
    }

    pub(in crate::services::discord) fn session(&self) -> Option<&str> {
        self.stamp.as_ref().map(|stamp| stamp.key.session.as_str())
    }

    /// The native end a `closed` judgment read, only when the reader read that turn from its
    /// opener and no queued input's receipt has arrived since without a turn opening for it.
    pub(in crate::services::discord) fn closed_boundary(&self) -> Option<&ClosedBoundary> {
        self.stamp.as_ref()?.closed.as_ref()
    }

    pub(in crate::services::discord) fn source(&self) -> Option<&str> {
        self.stamp
            .as_ref()
            .and_then(|stamp| stamp.source.as_deref())
    }

    /// Runs `publish` inside the watch's lock only while its key, generation and revision are
    /// still the ones this judgment read; a reading made before any lock holds nothing to check.
    pub(in crate::services::discord) async fn publish_if_current<R>(
        &self,
        publish: impl FnOnce() -> R,
    ) -> Option<R> {
        #[cfg(unix)]
        if let (Some(stamp), Some(watch)) = (&self.unbound, &self.watch) {
            return stamp.publish(watch, publish).await;
        }
        #[cfg(unix)]
        if let Some(admission) = &self.admission
            && !admission.current().await
        {
            return None;
        }
        let (Some(stamp), Some(watch)) = (&self.stamp, &self.watch) else {
            return Some(publish());
        };
        if !stamp.notice.current() {
            return None;
        }
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
            notice: NoticeObservation::legacy(observed.reason, session),
            closed: None,
        });
        let mut reading = Self::bound(observed, stamp, None, session);
        reading.notice = NoticeObservation::legacy(observed.reason, session.unwrap_or_default());
        reading
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
            notice: NoticeObservation::legacy(observed.reason, "overtaken"),
            closed: None,
        };
        Self::bound(
            observed,
            Some(stamp),
            Some(Arc::new(Mutex::new(watch))),
            None,
        )
    }
}

type Answer = (Observed, Option<Stamp>);

/// The answer and the state it was decided on, read under the same lock.
fn at(guard: &Watch, answer: Observed) -> Answer {
    let source = match &guard.outcome {
        Outcome::Source { facts, .. } => Some(facts.binding().source.path.display().to_string()),
        Outcome::Tail(reader) => Some(reader.facts.binding().source.path.display().to_string()),
        _ => None,
    };
    let stamp = guard.key.clone().map(|key| Stamp {
        key,
        generation: guard.generation,
        revision: guard.revision,
        source,
        notice: guard.notice.clone().observed(answer),
        closed: closed_boundary::closed_boundary(guard, answer),
    });
    let answer = (answer, stamp);
    if matches!(guard.outcome, Outcome::Rebuilding) {
        progress(answer)
    } else {
        answer
    }
}

/// A fresh judgment with the state it read, for publishers outside the effect points.
pub(in crate::services::discord) async fn reading_now(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Reading {
    // Bound callers' future sizes as the observation's database paths grow.
    Box::pin(read_now(shared, provider, channel, false)).await
}

async fn read_now(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    host_checked: bool,
) -> Reading {
    initialize_tail();
    let unstamped = |observed| {
        if host_checked {
            let watch = lock(&WATCHES).get(&channel.get()).cloned();
            if let Some(watch) = watch {
                supersede_unstamped(&watch);
            }
        }
        Reading::bound(observed, None, None, None)
    };
    let shadow = match provider {
        ProviderKind::Claude => ShadowProvider::Claude,
        ProviderKind::Codex => ShadowProvider::Codex,
        _ => return unstamped(observed(Activity::Unknown, "provider_unsupported")),
    };
    let bound = (shared.tmux_watchers.channel_binding(&channel))
        .filter(|watcher| watcher.owner_channel_id == channel)
        .map(|watcher| watcher.tmux_session_name);
    // Decided before the Unbound read, which resets the watch a Bound judgment builds on.
    #[cfg(unix)]
    let mut admission = None;
    #[cfg(unix)]
    let bound = match bound {
        None => match unbound::herdr_bound::admitted(shared, provider, channel).await {
            None => {
                admission =
                    unbound::live::inflight_bound::admitted(shared, provider, channel).await;
                admission.as_ref().map(|admitted| admitted.name.clone())
            }
            herdr => herdr,
        },
        bound => bound,
    };
    #[cfg(unix)]
    if bound.is_some() {
        unbound::hold_leaf::rearm(channel.get());
    }
    let target = match bound {
        Some(session) => Target::Bound(session),
        None => {
            let watch = lock(&WATCHES).entry(channel.get()).or_default().clone();
            #[cfg(unix)]
            {
                let (observed, stamp) =
                    unbound::live::read(shared, provider, channel, watch.clone()).await;
                return Reading {
                    observed,
                    notice: stamp.notice.clone(),
                    stamp: None,
                    watch: Some(watch),
                    unbound: Some(stamp),
                    admission: None,
                };
            }
            #[cfg(not(unix))]
            {
                let mut guard = lock(&watch);
                guard.key = None;
                guard.outcome = Outcome::Rebuilding;
                guard.generation += 1;
                guard.revision += 1;
                drop(guard);
                return unstamped(observed(Activity::Unknown, "session_probe_failed"));
            }
        }
    };
    let label = match &target {
        Target::Bound(label) => label.clone(),
        #[cfg(all(test, unix))]
        Target::Unbound(label) => label.clone().unwrap_or_default(),
    };
    let watch = lock(&WATCHES).entry(channel.get()).or_default().clone();
    let ports: Arc<dyn Ports> = Arc::new(LivePorts {
        #[cfg(test)]
        root: crate::services::tui_prompt_dedupe::binding_events::test_root(),
    });
    let held = watch.clone();
    let judged = tokio::task::spawn_blocking(move || {
        #[cfg(all(test, unix))]
        crate::services::tui_o::shadow::capture::io_trace::take();
        let answer = if host_checked {
            judge_presence(&held, &ports, shadow, channel.get(), target)
        } else {
            judge(&held, &ports, shadow, channel.get(), target)
        };
        #[cfg(all(test, unix))]
        tests::record_io(
            channel.get(),
            "effect",
            crate::services::tui_o::shadow::capture::io_trace::take(),
        );
        answer
    });
    match judged.await {
        Ok((observed, stamp)) => Reading {
            #[cfg(unix)]
            admission,
            ..Reading::bound(observed, stamp, Some(watch), Some(&label))
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
    #[cfg(not(all(test, unix)))]
    let Target::Bound(session) = target;
    #[cfg(all(test, unix))]
    let session = match target {
        Target::Bound(session) => session,
        #[cfg(test)]
        Target::Unbound(Some(session)) if ports.session_present(&session) => {
            return (observed(Activity::Unknown, "watcher_unbound"), None);
        }
        #[cfg(test)]
        Target::Unbound(_) => {
            return (
                observed(Activity::Unknown, "session_identity_unresolved"),
                None,
            );
        }
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
    let notice = NoticeObservation::bound("not_observed", Some(&key.session));
    let mut guard = lock(watch);
    guard.notice = notice;
    let retry =
        matches!(guard.outcome, Outcome::Unreadable { retry_at } if Instant::now() >= retry_at);
    if guard.key.as_ref() != Some(&key) || retry {
        guard.key = Some(key.clone());
        return rebuild(watch, guard, ports, channel, key, None);
    }
    let stored = |reason| observed(Activity::Unknown, reason);
    let (polled, from_start, allows_idle) = match &mut guard.outcome {
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
        Outcome::Source { facts, from_start } => {
            let result = facts.poll(CHUNK_BUDGET).map(|fact| {
                (
                    fact,
                    facts.caught_up().unwrap_or(false),
                    (facts.awaiting_boundary(), facts.saw_turn_evidence()),
                )
            });
            (result, *from_start, true)
        }
        Outcome::Tail(reader) => {
            let result = reader.poll_installed().map(|fact| {
                (
                    fact,
                    reader.facts.caught_up().unwrap_or(false),
                    (
                        reader.facts.awaiting_boundary(),
                        reader.facts.saw_turn_evidence(),
                    ),
                )
            });
            let proof = result
                .as_ref()
                .is_ok_and(|(fact, caught, _)| reader.allows_idle(fact, *caught));
            (result, false, proof)
        }
    };
    let (fact, caught_up, (awaiting, evidence)) = match polled {
        Ok(polled) => polled,
        Err(_) => {
            let resume = match std::mem::replace(&mut guard.outcome, Outcome::Halted) {
                Outcome::Source { facts, .. } => facts
                    .resume_point()
                    .cloned()
                    .map(|resume| Reload::Full(facts.binding().clone(), resume)),
                Outcome::Tail(reader) if reader.facts.resume_point().is_some() => {
                    Some(Reload::Tail(reader))
                }
                _ => None,
            };
            if let Some(resume) = resume {
                let (_, mut stamp) = rebuild(watch, guard, ports, channel, key, Some(resume));
                if let Some(stamp) = &mut stamp {
                    stamp.notice.reason = "facts_resumed";
                }
                return (observed(Activity::Unknown, "facts_resumed"), stamp);
            }
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
        return progress(at(&guard, observed(Activity::Unknown, "catching_up")));
    }
    match fact.state {
        TurnState::Idle if allows_idle => at(&guard, observed(Activity::Idle, "closed")),
        TurnState::Idle => at(&guard, observed(Activity::Unknown, "no_turn_boundary")),
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
            let mut fresh = at(&guard, answer);
            if let Some(fresh) = &mut fresh.1 {
                fresh.notice = stamp.notice;
            }
            return fresh;
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

enum Reload {
    Full(SourceBinding, Resume),
    Tail(Box<tail::TailReader>),
}

/// Starts a new generation and its worker after releasing the channel's lock.
fn rebuild(
    watch: &Arc<Mutex<Watch>>,
    mut guard: MutexGuard<'_, Watch>,
    ports: &Arc<dyn Ports>,
    channel: u64,
    key: Key,
    resume: Option<Reload>,
) -> Answer {
    guard.generation += 1;
    guard.outcome = Outcome::Rebuilding;
    let generation = guard.generation;
    let answer = at(&guard, observed(Activity::Unknown, "catching_up"));
    drop(guard);
    let (worker, worker_ports, failed) = (watch.clone(), ports.clone(), key.clone());
    let spawned = ports.spawn(Box::new(move || {
        #[cfg(all(test, unix))]
        let stage = if resume.is_some() {
            "resume"
        } else {
            "initial"
        };
        #[cfg(all(test, unix))]
        crate::services::tui_o::shadow::capture::io_trace::take();
        let (outcome, grown) = match resume {
            Some(Reload::Full(binding, resume)) => {
                read_from(InputFacts::resume(binding, &resume), false)
            }
            Some(Reload::Tail(mut reader)) => match reader.resume() {
                Ok(()) => read_tail(*reader, &key),
                Err(_) => (Outcome::Halted, None),
            },
            None => locate(worker_ports.as_ref(), channel, &key),
        };
        if let Outcome::Source { facts, .. } = &outcome {
            tracing::debug!(source = %facts.binding().source.path.display(), channel_id = channel, seq = key.seq,
                mode = "full", start = 0, through = facts.observation_progress().0,
                size = ?facts.source_len(), "activity rebuild complete");
        }
        #[cfg(all(test, unix))]
        tests::record_io(
            channel,
            stage,
            crate::services::tui_o::shadow::capture::io_trace::take(),
        );
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
        Ok(meta) if tail::limits(channel).tail_start(meta.len()).is_some() => {
            if !tail::enabled() {
                return (Outcome::TooLarge, None);
            }
            let binding = SourceBinding {
                channel_id: channel,
                provider: key.provider,
                source,
            };
            match tail::TailReader::open(binding, tail::limits(channel)) {
                Ok(reader) => read_tail(reader, key),
                Err(_) => (unreadable(), None),
            }
        }
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

fn read_tail(mut reader: tail::TailReader, key: &Key) -> (Outcome, Grown) {
    let result = reader.bootstrap();
    let (start, end, remaining, bytes) = reader.diagnostics();
    tracing::debug!(source = %reader.facts.binding().source.path.display(), seq = key.seq, mode = "tail", start, end, remaining, forward_bytes = bytes, result = ?result, "activity rebuild");
    if result.is_err() {
        return (Outcome::Halted, None);
    }
    let through = reader.facts.observation_progress().0;
    let grown = modified_at(&reader.facts).map(|at| (through, at));
    (Outcome::Tail(Box::new(reader)), grown)
}

pub(super) fn initialize_tail() {
    let _ = tail::enabled();
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

    #[cfg(all(test, unix))]
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

#[cfg(all(test, unix))]
#[path = "activity_notice_tests.rs"]
mod notice_tests;

pub(in crate::services::discord) mod effect_reads;
mod tail;
#[cfg(unix)]
mod unbound;
