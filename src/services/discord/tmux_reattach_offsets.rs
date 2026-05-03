use std::collections::VecDeque;
use std::sync::{LazyLock, Mutex};

use poise::serenity_prelude::ChannelId;

const RECENT_WATCHER_REATTACH_OFFSET_CAPACITY: usize = 32;
const RECENT_WATCHER_REATTACH_OFFSET_TTL: std::time::Duration =
    std::time::Duration::from_secs(15 * 60);

#[derive(Debug, Clone)]
pub(super) struct RecentWatcherReattachOffset {
    channel_id: ChannelId,
    tmux_session_name: String,
    pub(super) offset: u64,
    recorded_at: std::time::Instant,
}

static RECENT_WATCHER_REATTACH_OFFSETS: LazyLock<Mutex<VecDeque<RecentWatcherReattachOffset>>> =
    LazyLock::new(|| {
        Mutex::new(VecDeque::with_capacity(
            RECENT_WATCHER_REATTACH_OFFSET_CAPACITY,
        ))
    });

fn recent_watcher_reattach_offsets()
-> std::sync::MutexGuard<'static, VecDeque<RecentWatcherReattachOffset>> {
    match RECENT_WATCHER_REATTACH_OFFSETS.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn prune_recent_watcher_reattach_offsets(
    offsets: &mut VecDeque<RecentWatcherReattachOffset>,
    now: std::time::Instant,
) {
    offsets.retain(|entry| {
        now.saturating_duration_since(entry.recorded_at) <= RECENT_WATCHER_REATTACH_OFFSET_TTL
    });
}

pub(super) fn record_recent_watcher_reattach_offset(
    channel_id: ChannelId,
    tmux_session_name: &str,
    offset: u64,
) {
    let now = std::time::Instant::now();
    let mut offsets = recent_watcher_reattach_offsets();
    prune_recent_watcher_reattach_offsets(&mut offsets, now);
    while offsets.len() >= RECENT_WATCHER_REATTACH_OFFSET_CAPACITY {
        offsets.pop_front();
    }
    offsets.push_back(RecentWatcherReattachOffset {
        channel_id,
        tmux_session_name: tmux_session_name.to_string(),
        offset,
        recorded_at: now,
    });
}

pub(super) fn matching_recent_watcher_reattach_offset(
    channel_id: ChannelId,
    tmux_session_name: &str,
    data_start_offset: u64,
) -> Option<RecentWatcherReattachOffset> {
    let now = std::time::Instant::now();
    let mut offsets = recent_watcher_reattach_offsets();
    prune_recent_watcher_reattach_offsets(&mut offsets, now);
    offsets
        .iter()
        .rev()
        .find(|entry| {
            entry.channel_id == channel_id
                && entry.tmux_session_name == tmux_session_name
                && entry.offset == data_start_offset
        })
        .cloned()
}

#[cfg(all(test, feature = "legacy-sqlite-tests"))]
pub(super) fn clear_recent_watcher_reattach_offsets_for_tests() {
    recent_watcher_reattach_offsets().clear();
}
