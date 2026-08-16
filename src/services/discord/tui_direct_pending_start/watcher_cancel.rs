use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use poise::serenity_prelude::ChannelId;

use super::super::TmuxWatcherRegistry;
use super::super::tmux_watcher_registry::lock_tmux_watcher_registry;

/// The live registry row the stale-FOREIGN cancel pins before it commits.
///
/// #5071 T3-A1: `remove_tmux_session_if_current`, the helper this pin feeds,
/// compares the session key and the cancel pointer only. The owner channel and
/// the output path are the rest of T3-R2's tuple, so they are pinned here and
/// handed to the identity fence for re-comparison inside the registry lock.
///
/// Every registry read below happens under `lock_tmux_watcher_registry`, so the
/// pinned pointer, owner channel and output path describe one registry state
/// rather than a torn sequence of reads.
pub(super) struct PinnedWatcher {
    pub(super) tmux_session_name: String,
    pub(super) owner_channel_id: ChannelId,
    pub(super) output_path: String,
    pub(super) cancel: Arc<AtomicBool>,
}

pub(super) fn pin_watcher_for_tmux_session(
    registry: &TmuxWatcherRegistry,
    tmux_session_name: &str,
) -> Option<PinnedWatcher> {
    let _guard = lock_tmux_watcher_registry();
    let (output_path, cancel) = registry
        .iter()
        .find(|entry| entry.key() == tmux_session_name)
        .map(|entry| (entry.output_path.clone(), entry.cancel.clone()))?;
    let owner_channel_id = registry.owner_channel_for_tmux_session(tmux_session_name)?;
    Some(PinnedWatcher {
        tmux_session_name: tmux_session_name.to_string(),
        owner_channel_id,
        output_path,
        cancel,
    })
}
