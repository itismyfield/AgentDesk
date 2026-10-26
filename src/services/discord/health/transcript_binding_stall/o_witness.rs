//! O's side of the binding-stall verdict. When O posts a channel's TUI body, the Legacy inflight
//! row and capture offsets say nothing about delivery; O's own position after each poll does.

use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

use tokio::time::Instant;

use crate::services::tui_o::writer::progress::{self, ChannelProgress};

/// What health can see of the actor that posts an O-owned channel's body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum OWitness {
    /// O owns the channel, but no running actor published a position.
    Unwitnessed,
    /// The actor's latest position, and how long its ledger serial has stood still as this
    /// process saw it. A position first seen now has stood still for zero seconds.
    Seen {
        debt: usize,
        unsealed: bool,
        still_secs: u64,
    },
}

/// The serial each channel last showed and when this process first saw it there.
static STILL_SINCE: Mutex<Option<HashMap<u64, (u64, Instant)>>> = Mutex::new(None);

/// Whether this process's O writer owns the body of `channel`; reads ownership without claiming.
pub(in crate::services::discord) fn o_owns(channel: u64) -> bool {
    crate::services::tui_o::cutover::boot_ownership()
        .into_iter()
        .any(|(id, _, candidate)| id == channel && candidate.is_some_and(|c| c.peek().owned()))
}

/// O's witness for `channel`, or `None` when O does not own its body.
pub(in crate::services::discord) fn observe(channel: u64) -> Option<OWitness> {
    if !o_owns(channel) {
        return None;
    }
    Some(witness_at(channel, progress::read(channel), Instant::now()))
}

fn witness_at(channel: u64, position: Option<ChannelProgress>, now: Instant) -> OWitness {
    let Some(position) = position else {
        return OWitness::Unwitnessed;
    };
    let mut guard = STILL_SINCE.lock().unwrap_or_else(PoisonError::into_inner);
    let entry = guard
        .get_or_insert_with(HashMap::new)
        .entry(channel)
        .or_insert((position.serial, now));
    if entry.0 != position.serial {
        *entry = (position.serial, now);
    }
    OWitness::Seen {
        debt: position.debt,
        unsealed: position.unsealed,
        still_secs: now.saturating_duration_since(entry.1).as_secs(),
    }
}

/// Makes `channel`'s position at `serial` look first seen `secs` ago, as a long-running
/// process would have seen it.
#[cfg(test)]
pub(in crate::services::discord) fn backdate_for_tests(channel: u64, serial: u64, secs: u64) {
    let since = Instant::now()
        .checked_sub(std::time::Duration::from_secs(secs))
        .expect("process clock covers the backdate");
    let mut guard = STILL_SINCE.lock().unwrap_or_else(PoisonError::into_inner);
    guard
        .get_or_insert_with(HashMap::new)
        .insert(channel, (serial, since));
}

#[cfg(test)]
pub(super) fn witness_for_tests(
    channel: u64,
    position: Option<ChannelProgress>,
    now: Instant,
) -> OWitness {
    witness_at(channel, position, now)
}
