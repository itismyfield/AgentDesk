//! Why a restarted row's end leaves it, logged once per change with each reason's observed time,
//! and an end task's phase; timing here only observes, it is never evidence that the turn ended.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use super::*;

/// A change this soon after the channel's last line is counted into its next one.
const CHANGE_EVERY: Duration = Duration::from_secs(5);

/// Provider, channel, row nonce and born generation.
type RowKey = (String, u64, String, u64);

struct Held {
    reason: &'static str,
    previous: &'static str,
    /// The end task's phase when the reason is its own `ending_busy`.
    phase: &'static str,
    since: Instant,
    repeats: u64,
    logged_at: Option<Instant>,
    shown: bool,
    unshown: u64,
    /// When the last pass was seen; the time until the next one counts to that pass's reason.
    seen_at: Instant,
    /// Each reason's observed time, in first-seen order, so compressed changes keep their share.
    spent: Vec<((&'static str, &'static str), Duration)>,
}

impl Held {
    fn new(reason: &'static str, phase: &'static str, now: Instant) -> Self {
        Self {
            reason,
            previous: "-",
            phase,
            since: now,
            repeats: 0,
            logged_at: None,
            shown: false,
            unshown: 0,
            seen_at: now,
            spent: Vec::new(),
        }
    }

    /// Counts the time since the last pass to its reason, then takes this pass; true when it logs.
    fn observe(&mut self, reason: &'static str, phase: &'static str, now: Instant) -> bool {
        self.close(now);
        if (self.reason, self.phase) == (reason, phase) {
            self.repeats += 1;
        } else {
            self.unshown += u64::from(!self.shown);
            (self.previous, self.reason, self.phase) = (self.reason, reason, phase);
            (self.since, self.repeats, self.shown) = (now, 0, false);
        }
        if self.shown || self.logged_at.is_some_and(|at| now - at < CHANGE_EVERY) {
            return false;
        }
        (self.logged_at, self.shown) = (Some(now), true);
        true
    }

    /// Adds the open interval to the current reason's total.
    fn close(&mut self, now: Instant) {
        let took = now.saturating_duration_since(self.seen_at);
        self.seen_at = now;
        let key = (self.reason, self.phase);
        match self.spent.iter_mut().find(|(seen, _)| *seen == key) {
            Some((_, total)) => *total += took,
            None => self.spent.push((key, took)),
        }
    }

    /// `reason=ms` per observed reason, `ending_busy` with its phase.
    fn spent_text(&self) -> String {
        let name = |(reason, phase): (&str, &str)| match phase {
            "" => reason.to_owned(),
            phase => format!("{reason}@{phase}"),
        };
        (self.spent.iter())
            .map(|(key, took)| format!("{}={}ms", name(*key), took.as_millis()))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

static HELD: LazyLock<std::sync::Mutex<HashMap<RowKey, Held>>> = LazyLock::new(Default::default);

/// A running end task's phase, with when the task and the phase began and each phase's time.
pub(super) struct Phase {
    name: &'static str,
    task_at: Instant,
    phase_at: Instant,
    spent: Vec<(&'static str, Duration)>,
}

impl Phase {
    pub(super) fn start() -> Self {
        let now = Instant::now();
        Self {
            name: "queued",
            task_at: now,
            phase_at: now,
            spent: Vec::new(),
        }
    }
}

fn row_key(provider: &ProviderKind, row: &inflight::InflightTurnState) -> RowKey {
    let nonce = row.turn_nonce.clone().unwrap_or_default();
    (
        provider.as_str().to_owned(),
        row.channel_id,
        nonce,
        row.born_generation,
    )
}

/// Moves the channel's end task into phase `name`.
pub(super) fn phase(provider: &ProviderKind, channel: u64, name: &'static str) {
    let mut ending = ENDING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(phase) = ending.get_mut(&(provider.as_str().to_owned(), channel)) {
        let now = Instant::now();
        phase.spent.push((phase.name, now - phase.phase_at));
        (phase.name, phase.phase_at) = (name, now);
    }
}

/// The pass found the row's end task still running: the phase it waits in is the reason's detail.
pub(super) fn ending_busy(provider: &ProviderKind, row: &inflight::InflightTurnState) {
    let key = (provider.as_str().to_owned(), row.channel_id);
    let running = ENDING.lock().unwrap_or_else(|e| e.into_inner());
    let phase = running.get(&key).map(|phase| {
        let (task, current) = (phase.task_at.elapsed(), phase.phase_at.elapsed());
        (phase.name, task, current)
    });
    drop(running);
    let (name, task, current) = phase.unwrap_or(("-", Duration::ZERO, Duration::ZERO));
    if record(provider, row, "ending_busy", name) {
        tracing::info!(
            provider = provider.as_str(),
            channel_id = row.channel_id,
            phase = name,
            task_ms = task.as_millis() as u64,
            phase_ms = current.as_millis() as u64,
            "herdr row end task still running"
        );
    }
}

/// Records why this pass left the row: a new reason logs, an unchanged one only counts.
pub(super) fn held(
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    reason: &'static str,
) {
    record(provider, row, reason, "");
}

fn record(
    provider: &ProviderKind,
    row: &inflight::InflightTurnState,
    reason: &'static str,
    phase: &'static str,
) -> bool {
    let (key, now) = (row_key(provider, row), Instant::now());
    let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    // A replaced row's state goes with it.
    held.retain(|other, _| (&other.0, other.1) != (&key.0, key.1) || *other == key);
    let entry = held
        .entry(key)
        .or_insert_with(|| Held::new(reason, phase, now));
    if !entry.observe(reason, phase, now) {
        return false;
    }
    tracing::info!(
        provider = provider.as_str(),
        channel_id = row.channel_id,
        turn_nonce = row.turn_nonce.as_deref().unwrap_or_default(),
        born_generation = row.born_generation,
        save_generation = row.save_generation,
        reason,
        previous = entry.previous,
        phase,
        held_ms = (now - entry.since).as_millis() as u64,
        repeats = entry.repeats,
        unshown_changes = entry.unshown,
        spent = entry.spent_text(),
        "herdr row end held a restarted row"
    );
    entry.unshown = 0;
    true
}

/// The row ended: one line with how long it was held, then its state goes.
pub(super) fn settled(provider: &ProviderKind, row: &inflight::InflightTurnState) {
    let key = (provider.as_str().to_owned(), row.channel_id);
    let phases = ENDING
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&key)
        .map(|phase| {
            let mut spent: Vec<String> = (phase.spent.iter())
                .map(|(name, took)| format!("{name}={}ms", took.as_millis()))
                .collect();
            spent.push(format!(
                "{}={}ms",
                phase.name,
                phase.phase_at.elapsed().as_millis()
            ));
            (spent.join(" "), phase.task_at.elapsed().as_millis() as u64)
        });
    let mut held = HELD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&row_key(provider, row));
    if let Some(held) = held.as_mut() {
        held.close(Instant::now());
    }
    let (phases, task_ms) = phases.unwrap_or_default();
    tracing::info!(
        provider = provider.as_str(),
        channel_id = row.channel_id,
        last_reason = held.as_ref().map_or("-", |held| held.reason),
        last_held_ms = held
            .as_ref()
            .map_or(0, |held| held.since.elapsed().as_millis() as u64),
        spent = held.as_ref().map(Held::spent_text).unwrap_or_default(),
        task_ms,
        phases,
        "herdr row end settled a restarted turn"
    );
}

/// The reason the channel's row was last held, for a scenario's assertions.
#[cfg(test)]
pub(super) fn last_reason(provider: &ProviderKind, channel: u64) -> Option<&'static str> {
    let held = HELD.lock().unwrap_or_else(|e| e.into_inner());
    (held.iter())
        .find(|(key, _)| key.0 == provider.as_str() && key.1 == channel)
        .map(|(_, held)| held.reason)
}

#[cfg(test)]
#[path = "diagnostics_tests.rs"]
mod tests;
