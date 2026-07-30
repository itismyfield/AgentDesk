use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use super::super::TmuxWatcherRegistry;

pub(super) fn cancel_for_tmux_session(
    registry: &TmuxWatcherRegistry,
    tmux_session_name: &str,
) -> Option<Arc<AtomicBool>> {
    registry
        .iter()
        .find(|entry| entry.key() == tmux_session_name)
        .map(|entry| entry.cancel.clone())
}
