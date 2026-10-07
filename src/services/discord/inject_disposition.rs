//! Discord inputs a busy-turn injection already handed to a pane, keyed by (provider, message).
//! A bounded in-process table answers the mailbox actor; a bounded provider file answers scans.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
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

#[derive(Default)]
struct SourceTable {
    terminal: HashMap<u64, (ChannelId, InjectionOutcome, Instant)>,
    /// Insertion order for cap and age pruning; a re-noted id leaves a stale older slot.
    order: VecDeque<(u64, Instant)>,
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

/// Records `message`'s terminal for the mailbox actor; an injection with no message records nothing.
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
    table
        .terminal
        .insert(message.get(), (channel, outcome, now));
    table.order.push_back((message.get(), now));
    table.prune(now);
}

/// `message`'s live terminal. With no provider (an actor that has not seen its persistence yet)
/// any provider's terminal counts: Discord ids are unique, so this only blocks more.
pub(crate) fn terminal(
    provider: Option<&ProviderKind>,
    message: MessageId,
    now: Instant,
) -> Option<InjectionOutcome> {
    let mut tables = lock_table(TABLE.get()?);
    let live = |table: &mut SourceTable| {
        table.prune(now);
        let entry = table.terminal.get(&message.get());
        entry
            .filter(|entry| fresh(entry.2, now))
            .map(|entry| entry.1)
    };
    let found = match provider {
        Some(provider) => tables.get_mut(provider.as_str()).and_then(live),
        None => tables.values_mut().find_map(live),
    };
    tables.retain(|_, table| !table.terminal.is_empty());
    found
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
