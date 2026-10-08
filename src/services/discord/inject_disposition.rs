//! Discord inputs a busy-turn injection owns or already handed to a pane, keyed by (provider,
//! message). A bounded in-process table answers intake and the mailbox; a provider file, scans.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use poise::serenity_prelude::{ChannelId, MessageId};
use serde::{Deserialize, Serialize};

use super::recovery_known_ids::RecoveryKnownIdArm;
use super::runtime_store;
use crate::services::provider::ProviderKind;

/// Terminals the process keeps per provider, and for how long.
pub(crate) const MEMORY_CAP: usize = 512;
pub(crate) const MEMORY_TTL: Duration = Duration::from_secs(15 * 60);
/// Terminals the provider file keeps, and for how long.
pub(crate) const DISK_CAP: usize = 2048;
pub(crate) const DISK_TTL_MS: i64 = 24 * 60 * 60 * 1000;
/// A thread's claim on a promoted message lasts as long as intake's message-id dedup.
pub(crate) const THREAD_INTAKE_TTL: Duration = Duration::from_secs(60);

/// How an injection ended once the pane took the input or may have.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InjectionOutcome {
    Observed,
    /// Delivery unknown; the same message is never sent again on its own.
    Unconfirmed,
}

impl InjectionOutcome {
    fn arm(self) -> RecoveryKnownIdArm {
        match self {
            Self::Observed => RecoveryKnownIdArm::InjectedObserved,
            Self::Unconfirmed => RecoveryKnownIdArm::InjectedUnconfirmed,
        }
    }
}

/// Who is taking a message that has no terminal yet.
enum Source {
    /// A parent's injection, live while its guard holds the lease.
    InProgress { lease: Weak<SourceLease> },
    /// A thread's own intake of a message it promoted from its parent.
    ThreadIntake { at: Instant },
}

impl Source {
    fn live(&self, now: Instant) -> bool {
        match self {
            Self::InProgress { lease } => lease.strong_count() > 0,
            Self::ThreadIntake { at } => now.saturating_duration_since(*at) < THREAD_INTAKE_TTL,
        }
    }

    fn in_progress(&self, now: Instant) -> bool {
        matches!(self, Self::InProgress { .. }) && self.live(now)
    }
}

#[derive(Default)]
struct SourceTable {
    terminal: HashMap<u64, (ChannelId, InjectionOutcome, Instant)>,
    /// Insertion order for cap and age pruning, one slot per terminal.
    order: VecDeque<(u64, Instant)>,
    sources: HashMap<u64, Source>,
}

fn fresh(at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(at) < MEMORY_TTL
}

impl SourceTable {
    fn prune(&mut self, now: Instant) {
        while let Some(&(id, at)) = self.order.front() {
            if fresh(at, now) && self.terminal.len() <= MEMORY_CAP {
                break;
            }
            self.order.pop_front();
            if self.terminal.get(&id).is_some_and(|entry| entry.2 == at) {
                self.terminal.remove(&id);
            }
        }
        self.sources.retain(|_, source| source.live(now));
    }

    fn fresh_terminal(&self, message: u64, now: Instant) -> Option<InjectionOutcome> {
        let entry = self.terminal.get(&message);
        entry
            .filter(|entry| fresh(entry.2, now))
            .map(|entry| entry.1)
    }

    fn empty(&self) -> bool {
        self.terminal.is_empty() && self.sources.is_empty()
    }
}

/// Initialized by the first terminal noted, so a process that never injects never builds it.
static TABLE: OnceLock<Mutex<HashMap<String, SourceTable>>> = OnceLock::new();

fn lock_table(
    table: &Mutex<HashMap<String, SourceTable>>,
) -> std::sync::MutexGuard<'_, HashMap<String, SourceTable>> {
    table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Records `message`'s terminal, ending any claim on it; an injection with no message records
/// nothing.
pub(crate) fn note_terminal(
    provider: &ProviderKind,
    channel: ChannelId,
    message: Option<MessageId>,
    outcome: InjectionOutcome,
    now: Instant,
) {
    let Some(message) = message else {
        return;
    };
    let mut tables = lock_table(TABLE.get_or_init(Default::default));
    let table = tables.entry(provider.as_str().to_string()).or_default();
    let id = message.get();
    if table.terminal.insert(id, (channel, outcome, now)).is_some() {
        table.order.retain(|slot| slot.0 != id);
    }
    table.order.push_back((id, now));
    table.sources.remove(&id);
    table.prune(now);
}

/// `message`'s live terminal. With no provider (an actor that has not seen its persistence yet)
/// any provider's terminal counts: Discord ids are unique, so this only blocks more.
pub(crate) fn terminal(
    provider: Option<&ProviderKind>,
    message: MessageId,
    now: Instant,
) -> Option<InjectionOutcome> {
    let live = |table: &mut SourceTable| {
        table.prune(now);
        table.fresh_terminal(message.get(), now)
    };
    find(provider, live)
}

/// `pick` over `provider`'s table, or over every provider's when it is `None`; reads nothing
/// when no claim or terminal was ever recorded.
fn find<T>(
    provider: Option<&ProviderKind>,
    mut pick: impl FnMut(&mut SourceTable) -> Option<T>,
) -> Option<T> {
    let mut tables = lock_table(TABLE.get()?);
    let found = match provider {
        Some(provider) => tables.get_mut(provider.as_str()).and_then(&mut pick),
        None => tables.values_mut().find_map(&mut pick),
    };
    tables.retain(|_, table| !table.empty());
    found
}

/// Whether a parent's injection is taking `message` now. A thread's own intake does not count:
/// the thread's mailbox takes the message it promoted.
pub(crate) fn in_progress(
    provider: Option<&ProviderKind>,
    message: MessageId,
    now: Instant,
) -> bool {
    let held = |table: &mut SourceTable| {
        table.prune(now);
        let source = table.sources.get(&message.get());
        source
            .is_some_and(|source| source.in_progress(now))
            .then_some(())
    };
    find(provider, held).is_some()
}

/// The lease a parent's injection holds on its message.
pub(crate) struct SourceLease;

/// One injection's claim on `(provider, message)`, released when dropped.
pub(crate) struct SourceGuard {
    provider: String,
    message: u64,
    lease: Arc<SourceLease>,
}

impl Drop for SourceGuard {
    /// Removes only this guard's own claim: a later claim or a terminal stays.
    fn drop(&mut self) {
        let Some(table) = TABLE.get() else {
            return;
        };
        let mut tables = lock_table(table);
        let Some(table) = tables.get_mut(&self.provider) else {
            return;
        };
        let own = |source: &Source| match source {
            Source::InProgress { lease } => std::ptr::eq(lease.as_ptr(), Arc::as_ptr(&self.lease)),
            Source::ThreadIntake { .. } => false,
        };
        if table.sources.get(&self.message).is_some_and(own) {
            table.sources.remove(&self.message);
        }
        if table.empty() {
            tables.remove(&self.provider);
        }
    }
}

/// Claims `message` for a parent's injection unless a terminal, another injection or a thread's
/// intake already owns it. Called only on a channel the injection gate opens.
pub(crate) fn claim_source(
    provider: &ProviderKind,
    message: MessageId,
    now: Instant,
) -> Option<SourceGuard> {
    #[cfg(test)]
    test_support::note_call("claim_source", message.get());
    let mut tables = lock_table(TABLE.get_or_init(Default::default));
    let table = tables.entry(provider.as_str().to_string()).or_default();
    table.prune(now);
    let id = message.get();
    if table.fresh_terminal(id, now).is_some() || table.sources.contains_key(&id) {
        return None;
    }
    let lease = Arc::new(SourceLease);
    let source = Source::InProgress {
        lease: Arc::downgrade(&lease),
    };
    table.sources.insert(id, source);
    let provider = provider.as_str().to_string();
    Some(SourceGuard {
        provider,
        message: id,
        lease,
    })
}

/// Lets a thread take a message its parent passed on, recording the thread's intake, unless the
/// parent's injection owns it or already ended it. Called only on a parent the gate opens.
pub(crate) fn promote_thread(provider: &ProviderKind, message: MessageId, now: Instant) -> bool {
    #[cfg(test)]
    test_support::note_call("promote_thread", message.get());
    let mut tables = lock_table(TABLE.get_or_init(Default::default));
    let table = tables.entry(provider.as_str().to_string()).or_default();
    table.prune(now);
    let id = message.get();
    let parent_owns = (table.sources.get(&id)).is_some_and(|source| source.in_progress(now));
    if parent_owns || table.fresh_terminal(id, now).is_some() {
        return false;
    }
    let source = Source::ThreadIntake { at: now };
    table.sources.insert(id, source);
    true
}

/// Whether a live arrival of `message` was already taken by an injection: in progress, or ended
/// in the process table or the provider file. Called only on a channel the gate opens.
pub(crate) fn taken(
    provider: &ProviderKind,
    message: MessageId,
    now_ms: i64,
    now: Instant,
) -> bool {
    #[cfg(test)]
    test_support::note_call("taken", message.get());
    let ended = terminal(Some(provider), message, now).is_some();
    if ended || in_progress(Some(provider), message, now) {
        return true;
    }
    let Some(path) = ring_path(provider) else {
        return false;
    };
    let live = |entry: &DiskEntry| now_ms - entry.at_epoch_ms < DISK_TTL_MS;
    let entries = load(&path).0;
    entries
        .iter()
        .any(|entry| entry.message_id == message.get() && live(entry))
}

fn memory_terminals(provider: &ProviderKind, now: Instant) -> Vec<(u64, InjectionOutcome)> {
    let Some(table) = TABLE.get() else {
        return Vec::new();
    };
    let mut tables = lock_table(table);
    let Some(table) = tables.get_mut(provider.as_str()) else {
        return Vec::new();
    };
    table.prune(now);
    let live = table
        .terminal
        .iter()
        .filter(|(_, entry)| fresh(entry.2, now));
    live.map(|(id, entry)| (*id, entry.1)).collect()
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct DiskRing {
    entries: Vec<DiskEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct DiskEntry {
    message_id: u64,
    channel_id: u64,
    outcome: InjectionOutcome,
    at_epoch_ms: i64,
}

/// Beside the provider's checkpoints; the checkpoint scan and stale prune skip this name.
fn ring_path(provider: &ProviderKind) -> Option<PathBuf> {
    let root = runtime_store::last_message_root()?;
    Some(root.join(provider.as_str()).join("injected_inputs.json"))
}

/// The ring's entries; missing reads as empty, unreadable or malformed as empty with a warning.
fn load(path: &Path) -> (Vec<DiskEntry>, bool) {
    #[cfg(test)]
    test_support::note_read(path);
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), true),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "injected-input ring unreadable");
            return (Vec::new(), false);
        }
    };
    match serde_json::from_str::<DiskRing>(&raw) {
        Ok(ring) => (ring.entries, true),
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "injected-input ring malformed");
            (Vec::new(), false)
        }
    }
}

/// Records `message`'s terminal in the provider file under its flock. Blocking.
pub(crate) fn record_terminal(
    provider: &ProviderKind,
    channel: ChannelId,
    message: MessageId,
    outcome: InjectionOutcome,
    now_ms: i64,
) -> Result<(), String> {
    let path = ring_path(provider).ok_or("runtime root unavailable")?;
    #[cfg(test)]
    test_support::before_lock(&path);
    let _lock = super::outbound::delivery_record::lock_record_path(&path)?;
    let (mut entries, readable) = load(&path);
    if !readable {
        // An unreadable ring is replaced; the terminals it held stop blocking replays.
        tracing::warn!(path = %path.display(), "injected_inputs_corrupt_overwritten");
    }
    #[cfg(test)]
    test_support::pause_before_write(&path);
    entries.retain(|entry| {
        entry.message_id != message.get() && now_ms - entry.at_epoch_ms < DISK_TTL_MS
    });
    entries.push(DiskEntry {
        message_id: message.get(),
        channel_id: channel.get(),
        outcome,
        at_epoch_ms: now_ms,
    });
    let overflow = entries.len().saturating_sub(DISK_CAP);
    entries.drain(..overflow);
    let raw = serde_json::to_string(&DiskRing { entries }).map_err(|error| error.to_string())?;
    runtime_store::atomic_write(&path, &raw)?;
    runtime_store::fsync_parent_dir(&path).map_err(|error| error.to_string())?;
    Ok(())
}

/// One scan's view of the provider's terminals: the process table, then the provider file.
pub(in crate::services::discord) struct InjectedView {
    terminal: HashMap<u64, InjectionOutcome>,
}

pub(in crate::services::discord) fn scan_view(provider: &ProviderKind) -> InjectedView {
    let now_ms = chrono::Utc::now().timestamp_millis();
    scan_view_at(provider, now_ms, Instant::now())
}

pub(in crate::services::discord) fn scan_view_at(
    provider: &ProviderKind,
    now_ms: i64,
    now: Instant,
) -> InjectedView {
    let mut terminal: HashMap<u64, InjectionOutcome> = HashMap::new();
    if let Some(path) = ring_path(provider) {
        let live = load(&path).0.into_iter();
        let live = live.filter(|entry| now_ms - entry.at_epoch_ms < DISK_TTL_MS);
        terminal.extend(live.map(|entry| (entry.message_id, entry.outcome)));
    }
    terminal.extend(memory_terminals(provider, now));
    InjectedView { terminal }
}

impl InjectedView {
    /// Adds each terminal to the scan's known ids; an arm the mailbox already gave an id stays.
    pub(in crate::services::discord) fn merge(
        &self,
        arms: &mut HashMap<u64, RecoveryKnownIdArm>,
        known: &mut HashSet<u64>,
    ) {
        for (id, outcome) in &self.terminal {
            arms.entry(*id).or_insert(outcome.arm());
            known.insert(*id);
        }
    }

    /// Logs how many fetched ids this view holds. An observation only: a fetched id the scan
    /// never reached, or one a mailbox arm already covered, counts too.
    pub(in crate::services::discord) fn log_hits(
        &self,
        phase: &'static str,
        channel: ChannelId,
        fetched: impl IntoIterator<Item = u64>,
    ) {
        let (mut observed, mut unconfirmed) = (0usize, 0usize);
        for id in fetched {
            match self.terminal.get(&id) {
                Some(InjectionOutcome::Observed) => observed += 1,
                Some(InjectionOutcome::Unconfirmed) => unconfirmed += 1,
                None => {}
            }
        }
        if observed + unconfirmed == 0 {
            return;
        }
        #[cfg(test)]
        test_support::note_hits(channel, observed, unconfirmed);
        tracing::info!(
            phase,
            channel_id = channel.get(),
            observed,
            unconfirmed,
            "catch-up terminal_view_hits"
        );
    }
}

#[cfg(test)]
#[path = "inject_disposition_test_support.rs"]
pub(crate) mod test_support;

#[cfg(test)]
#[path = "inject_disposition_tests.rs"]
mod tests;
