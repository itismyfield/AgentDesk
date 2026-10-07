//! Fresh read of a confirmed turn-mode channel's own transcript: idle, busy or unknown.
//! Effect points only poll within a chunk budget; binding folds and prefix hashes run on a worker.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use poise::serenity_prelude::ChannelId;

use crate::services::discord::SharedData;
use crate::services::provider::ProviderKind;
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
    outcome: Outcome,
    through: u64,
    grew_at: Instant,
}

impl Default for Watch {
    fn default() -> Self {
        Self {
            key: None,
            generation: 0,
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
    /// False when the worker could not start.
    fn spawn(&self, job: Job) -> bool;
}

static WATCHES: LazyLock<Mutex<HashMap<u64, Arc<Mutex<Watch>>>>> = LazyLock::new(Default::default);

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Judged fresh for an effect point; callers ask only for confirmed turn-mode channels.
pub(in crate::services::discord) async fn activity_now(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
) -> Observed {
    let shadow = match provider {
        ProviderKind::Claude => ShadowProvider::Claude,
        ProviderKind::Codex => ShadowProvider::Codex,
        _ => return observed(Activity::Unknown, "provider_unsupported"),
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
    let judged =
        tokio::task::spawn_blocking(move || observe(&watch, &ports, shadow, channel.get(), target));
    judged
        .await
        .unwrap_or(observed(Activity::Unknown, "probe_failed"))
}

fn observe(
    watch: &Arc<Mutex<Watch>>,
    ports: &Arc<dyn Ports>,
    provider: ShadowProvider,
    channel: u64,
    target: Target,
) -> Observed {
    let session = match target {
        Target::Bound(session) => session,
        Target::Unbound(Some(session)) if ports.session_present(&session) => {
            return observed(Activity::Unknown, "watcher_unbound");
        }
        Target::Unbound(_) => return observed(Activity::Idle, "no_session"),
    };
    let seq = match ports.binding_seq(channel) {
        Ok(seq) => seq,
        Err(_) => return observed(Activity::Unknown, "binding_unreadable"),
    };
    let key = Key {
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
    let (facts, from_start) = match &mut guard.outcome {
        Outcome::Rebuilding => return observed(Activity::Unknown, "catching_up"),
        Outcome::NoSource => return no_turn_evidence(ports.as_ref(), &key),
        Outcome::Pending => return observed(Activity::Unknown, "binding_pending"),
        Outcome::Unreadable { .. } => return observed(Activity::Unknown, "binding_unreadable"),
        Outcome::Halted => return observed(Activity::Unknown, "facts_halted"),
        Outcome::TooLarge => return observed(Activity::Unknown, "transcript_too_large"),
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
            rebuild(watch, guard, ports, channel, key, Some(resume));
            return observed(Activity::Unknown, "facts_resumed");
        }
        (Err(_), _) => {
            guard.outcome = Outcome::Halted;
            return observed(Activity::Unknown, "facts_halted");
        }
    };
    if fact.through > guard.through {
        (guard.through, guard.grew_at) = (fact.through, Instant::now());
    }
    if !caught_up {
        return observed(Activity::Unknown, "catching_up");
    }
    match fact.state {
        TurnState::Idle => observed(Activity::Idle, "closed"),
        TurnState::Open { .. } if guard.grew_at.elapsed() < STALE_OPEN_AFTER => {
            observed(Activity::Busy, "open")
        }
        TurnState::Open { .. } if provider == ShadowProvider::Claude => {
            drop(guard);
            match ports.pane_busy(&key.session) {
                true => observed(Activity::Busy, "open_pane_busy"),
                false => observed(Activity::Unknown, "open_without_progress"),
            }
        }
        TurnState::Open { .. } => observed(Activity::Unknown, "open_without_progress"),
        TurnState::Unknown if awaiting => observed(Activity::Unknown, "facts_resumed"),
        TurnState::Unknown if from_start && !evidence => {
            drop(guard);
            no_turn_evidence(ports.as_ref(), &key)
        }
        TurnState::Unknown => observed(Activity::Unknown, "no_turn_boundary"),
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
) -> Observed {
    guard.generation += 1;
    guard.outcome = Outcome::Rebuilding;
    let generation = guard.generation;
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
    observed(Activity::Unknown, "catching_up")
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
        Err(_) => None,
    };
    let Some(source) = source else {
        return (Outcome::NoSource, None);
    };
    match std::fs::metadata(&source.path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => (Outcome::NoSource, None),
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
