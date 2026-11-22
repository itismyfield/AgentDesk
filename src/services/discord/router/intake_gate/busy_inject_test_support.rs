//! Test seams for the Discord injection hook: a per-channel gate, and what each message met.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::Notify;

use super::InjectAttempt;

enum GateFixture {
    Platform,
    WithFlock,
}

static OPEN: Mutex<Option<HashMap<u64, GateFixture>>> = Mutex::new(None);
/// Per message: hook entries past the gate, promotion answers and attempt results.
static SEEN: Mutex<Option<HashMap<u64, Seen>>> = Mutex::new(None);
type Park = (Arc<Notify>, Arc<Notify>);
static PARKS: Mutex<Option<HashMap<u64, Park>>> = Mutex::new(None);

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
        open.get_or_insert_with(HashMap::new).remove(&self.0);
    }
}

pub(crate) fn open_gate(channel: u64) -> OpenGate {
    open_gate_with(channel, GateFixture::Platform)
}

/// Stubs flock support for a gate decision on this fixture channel.
pub(super) fn open_gate_with_flock(channel: u64) -> OpenGate {
    open_gate_with(channel, GateFixture::WithFlock)
}

fn open_gate_with(channel: u64, fixture: GateFixture) -> OpenGate {
    let mut open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    open.get_or_insert_with(HashMap::new)
        .insert(channel, fixture);
    OpenGate(channel)
}

pub(super) fn gate_open(channel: u64, platform_has_flock: bool) -> bool {
    let open = OPEN.lock().unwrap_or_else(|e| e.into_inner());
    open.as_ref()
        .and_then(|open| open.get(&channel))
        .is_some_and(|fixture| platform_has_flock || matches!(fixture, GateFixture::WithFlock))
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

#[cfg(unix)]
pub(crate) async fn self_heal_pending(
    ctx: &super::serenity::Context,
    data: &super::Data,
    channel: super::ChannelId,
    message: super::MessageId,
) {
    assert!(
        super::super::queue_effects::add_queue_pending_reaction_self_healing(
            ctx, data, channel, message, '📬',
        )
        .await
    );
}
