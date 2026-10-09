//! Test seams for the Discord injection hook: a per-channel gate, and what each message met.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use super::InjectAttempt;

static OPEN: Mutex<Option<HashSet<u64>>> = Mutex::new(None);
/// Per message: hook entries past the gate, promotion answers and attempt results.
static SEEN: Mutex<Option<HashMap<u64, Seen>>> = Mutex::new(None);
type Park = (Arc<Notify>, Arc<Notify>);
static PARKS: Mutex<Option<HashMap<u64, Park>>> = Mutex::new(None);
/// Seconds added to the clock catch-up's live yield reads, so a test can age a message.
static CLOCK_SKEW_SECS: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Seen {
    pub(crate) offers: usize,
    pub(crate) promotions: Vec<bool>,
    pub(crate) outcomes: Vec<String>,
}

/// Opens the gate for `channel` until the guard drops.
pub(crate) struct OpenGate(u64);

impl Drop for OpenGate {
    fn drop(&mut self) {
        let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
        open.get_or_insert_with(HashSet::new).remove(&self.0);
    }
}

pub(crate) fn open_gate(channel: u64) -> OpenGate {
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    open.get_or_insert_with(HashSet::new).insert(channel);
    OpenGate(channel)
}

pub(super) fn gate_open(channel: u64) -> bool {
    let open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    open.as_ref().is_some_and(|open| open.contains(&channel))
}

fn record(message: u64, note: impl FnOnce(&mut Seen)) {
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    note(
        seen.get_or_insert_with(HashMap::new)
            .entry(message)
            .or_default(),
    );
}

/// What the hook met for `message` so far.
pub(crate) fn seen(message: u64) -> Seen {
    let seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    seen.as_ref()
        .and_then(|seen| seen.get(&message))
        .cloned()
        .unwrap_or_default()
}

/// Parks the hook for `message` past the gate, before it claims the message: `(reached, resume)`.
pub(crate) fn park_offer(message: u64) -> Park {
    let park: Park = Default::default();
    let mut parks = PARKS.lock().unwrap_or_else(|e| e.into_inner());
    parks
        .get_or_insert_with(HashMap::new)
        .insert(message, park.clone());
    park
}

pub(super) async fn note_offer(message: u64) {
    record(message, |seen| seen.offers += 1);
    let park = {
        let mut parks = PARKS.lock().unwrap_or_else(|e| e.into_inner());
        parks.as_mut().and_then(|parks| parks.remove(&message))
    };
    if let Some((reached, resume)) = park {
        reached.notify_one();
        resume.notified().await;
    }
}

pub(super) fn note_promotion(message: u64, allowed: bool) {
    record(message, |seen| seen.promotions.push(allowed));
}

pub(super) fn note_outcome(message: u64, attempt: &InjectAttempt) {
    let outcome = format!("{attempt:?}");
    record(message, |seen| seen.outcomes.push(outcome));
}

/// Moves the clock catch-up's live yield reads by `secs` until the guard drops.
pub(crate) struct ClockSkew;

impl Drop for ClockSkew {
    fn drop(&mut self) {
        CLOCK_SKEW_SECS.store(0, std::sync::atomic::Ordering::SeqCst);
    }
}

pub(crate) fn skew_clock(secs: i64) -> ClockSkew {
    CLOCK_SKEW_SECS.store(secs, std::sync::atomic::Ordering::SeqCst);
    ClockSkew
}

pub(super) fn clock_skew() -> chrono::Duration {
    chrono::Duration::seconds(CLOCK_SKEW_SECS.load(std::sync::atomic::Ordering::SeqCst))
}
