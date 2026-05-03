use poise::serenity_prelude::{ChannelId, MessageId};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeadlessTurnStartOutcome {
    pub turn_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HeadlessTurnReservation {
    pub(super) user_msg_id: MessageId,
    pub(super) placeholder_msg_id: MessageId,
}

impl HeadlessTurnReservation {
    pub(in crate::services::discord) fn turn_id(&self, channel_id: ChannelId) -> String {
        discord_turn_id(channel_id, self.user_msg_id)
    }
}

pub(super) fn discord_turn_id(channel_id: ChannelId, user_msg_id: MessageId) -> String {
    format!("discord:{}:{}", channel_id.get(), user_msg_id.get())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HeadlessTurnStartError {
    Conflict(String),
    Internal(String),
}

impl std::fmt::Display for HeadlessTurnStartError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conflict(message) | Self::Internal(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for HeadlessTurnStartError {}

#[cfg(test)]
pub(super) const HEADLESS_TURN_MESSAGE_ID_BASE: u64 = 9_100_000_000_000_000_000;
#[cfg(not(test))]
const HEADLESS_TURN_MESSAGE_ID_BASE: u64 = 9_100_000_000_000_000_000;
const HEADLESS_TURN_MESSAGE_ID_EPOCH_MILLIS: u64 = 1_700_000_000_000;
const HEADLESS_TURN_MESSAGE_IDS_PER_MILLI: u64 = 1_024;

fn next_headless_turn_message_id() -> MessageId {
    static HEADLESS_TURN_MESSAGE_ID_SEQ: AtomicU64 = AtomicU64::new(0);
    ensure_headless_turn_message_id_seeded(&HEADLESS_TURN_MESSAGE_ID_SEQ);
    MessageId::new(HEADLESS_TURN_MESSAGE_ID_SEQ.fetch_add(1, Ordering::Relaxed))
}

fn ensure_headless_turn_message_id_seeded(sequence: &AtomicU64) {
    if sequence.load(Ordering::Acquire) != 0 {
        return;
    }
    let _ = sequence.compare_exchange(
        0,
        headless_turn_message_id_seed(current_unix_millis(), std::process::id()),
        Ordering::AcqRel,
        Ordering::Acquire,
    );
}

fn current_unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().try_into().unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
pub(super) fn headless_turn_message_id_seed(now_millis: u64, process_id: u32) -> u64 {
    headless_turn_message_id_seed_impl(now_millis, process_id)
}

#[cfg(not(test))]
fn headless_turn_message_id_seed(now_millis: u64, process_id: u32) -> u64 {
    headless_turn_message_id_seed_impl(now_millis, process_id)
}

fn headless_turn_message_id_seed_impl(now_millis: u64, process_id: u32) -> u64 {
    let max_elapsed_millis =
        (u64::MAX - HEADLESS_TURN_MESSAGE_ID_BASE - (HEADLESS_TURN_MESSAGE_IDS_PER_MILLI - 1))
            / HEADLESS_TURN_MESSAGE_IDS_PER_MILLI;
    let elapsed_millis = now_millis
        .saturating_sub(HEADLESS_TURN_MESSAGE_ID_EPOCH_MILLIS)
        .min(max_elapsed_millis);
    HEADLESS_TURN_MESSAGE_ID_BASE
        + (elapsed_millis * HEADLESS_TURN_MESSAGE_IDS_PER_MILLI)
        + (u64::from(process_id) % HEADLESS_TURN_MESSAGE_IDS_PER_MILLI)
}

pub(in crate::services::discord) fn reserve_headless_turn() -> HeadlessTurnReservation {
    HeadlessTurnReservation {
        user_msg_id: next_headless_turn_message_id(),
        placeholder_msg_id: next_headless_turn_message_id(),
    }
}

pub(super) fn resolve_session_id_for_current_turn(
    session_id: Option<String>,
    reset_applied: bool,
) -> Option<String> {
    if reset_applied { None } else { session_id }
}
