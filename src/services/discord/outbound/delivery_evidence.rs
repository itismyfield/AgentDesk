//! Process-lifetime evidence that Discord accepted an outbound write.
//!
//! Relay offsets can be absent or reset independently of successful Discord
//! transport calls. This module keeps a separate monotonic sequence per channel
//! and snapshots that sequence when a mailbox turn starts.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

use dashmap::DashMap;
use poise::serenity_prelude::ChannelId;

#[derive(Default)]
struct DeliveryEvidence {
    next_sequence: AtomicU64,
    delivered_sequence: DashMap<ChannelId, u64>,
    turn_start_sequence: DashMap<ChannelId, u64>,
}

fn evidence() -> &'static DeliveryEvidence {
    static EVIDENCE: OnceLock<DeliveryEvidence> = OnceLock::new();
    EVIDENCE.get_or_init(DeliveryEvidence::default)
}

pub(in crate::services::discord) fn begin_turn(channel_id: ChannelId) {
    let state = evidence();
    let delivered_sequence = state
        .delivered_sequence
        .get(&channel_id)
        .map(|sequence| *sequence)
        .unwrap_or(0);
    state
        .turn_start_sequence
        .insert(channel_id, delivered_sequence);
}

pub(super) fn record_confirmed_delivery(channel_id: ChannelId) {
    let state = evidence();
    let previous = state
        .next_sequence
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |sequence| {
            Some(sequence.saturating_add(1))
        })
        .unwrap_or(u64::MAX);
    let sequence = previous.saturating_add(1);
    state
        .delivered_sequence
        .entry(channel_id)
        .and_modify(|current| *current = (*current).max(sequence))
        .or_insert(sequence);
}

/// `None` means this process did not observe the mailbox turn start. Callers
/// that authorize destructive recovery must treat that state as unknown.
pub(in crate::services::discord) fn confirmed_delivery_since_turn_start(
    channel_id: ChannelId,
) -> Option<bool> {
    let state = evidence();
    let turn_start_sequence = *state.turn_start_sequence.get(&channel_id)?;
    let delivered_sequence = state
        .delivered_sequence
        .get(&channel_id)
        .map(|sequence| *sequence)
        .unwrap_or(0);
    Some(delivered_sequence > turn_start_sequence)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delivery_evidence_is_unknown_until_turn_start_then_monotonic() {
        let channel = ChannelId::new(50_220_001);
        assert_eq!(confirmed_delivery_since_turn_start(channel), None);

        begin_turn(channel);
        assert_eq!(confirmed_delivery_since_turn_start(channel), Some(false));

        record_confirmed_delivery(channel);
        assert_eq!(confirmed_delivery_since_turn_start(channel), Some(true));
        record_confirmed_delivery(channel);
        assert_eq!(confirmed_delivery_since_turn_start(channel), Some(true));
    }

    #[test]
    fn a_new_turn_uses_the_existing_monotonic_delivery_sequence_as_its_baseline() {
        let channel = ChannelId::new(50_220_002);
        record_confirmed_delivery(channel);
        begin_turn(channel);
        assert_eq!(confirmed_delivery_since_turn_start(channel), Some(false));

        record_confirmed_delivery(channel);
        assert_eq!(confirmed_delivery_since_turn_start(channel), Some(true));
    }
}
