//! Effect reads in flight per channel. The periodic observer skips a channel while one runs,
//! because its own read would supersede the judgment the effect point is about to act on.
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// How long one effect read defers the observer; reads that keep overlapping each defer it anew.
const DEFER_AT_MOST: Duration = Duration::from_secs(30);

static NEXT: AtomicU64 = AtomicU64::new(1);
static IN_FLIGHT: LazyLock<Mutex<HashMap<u64, HashMap<u64, Instant>>>> =
    LazyLock::new(Default::default);

/// Registered while the effect read runs; dropping it, also on cancel or unwind, unregisters it.
#[must_use]
pub(in crate::services::discord) struct EffectRead {
    channel: u64,
    id: u64,
}

pub(in crate::services::discord) fn begin(channel: u64) -> EffectRead {
    begin_at(channel, Instant::now())
}

fn begin_at(channel: u64, at: Instant) -> EffectRead {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut map = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(channel).or_default().insert(id, at);
    EffectRead { channel, id }
}

impl Drop for EffectRead {
    fn drop(&mut self) {
        let mut map = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(reads) = map.get_mut(&self.channel) {
            reads.remove(&self.id);
            if reads.is_empty() {
                map.remove(&self.channel);
            }
        }
    }
}

/// Whether an effect read on `channel` that started less than `DEFER_AT_MOST` ago is still running.
pub(in crate::services::discord::turn_presence) fn pending(channel: u64) -> bool {
    pending_at(channel, Instant::now())
}

fn pending_at(channel: u64, now: Instant) -> bool {
    let map = IN_FLIGHT.lock().unwrap_or_else(|e| e.into_inner());
    map.get(&channel).is_some_and(|reads| {
        reads
            .values()
            .any(|at| now.saturating_duration_since(*at) < DEFER_AT_MOST)
    })
}

#[cfg(test)]
#[path = "effect_reads_tests.rs"]
mod tests;
