//! Whether O owes a turn's body on a channel it owns: its init covers the turn's transcript from
//! at or before the turn's start, so Legacy may settle the turn without sending its body.

use crate::services::discord::inflight::{self, GuardedSaveOutcome, InflightTurnState};
use crate::services::provider::ProviderKind;

/// `channel`'s init holds `path`'s file with delivery starting at or before `start`; the caller
/// has already seen O own the channel.
pub(in crate::services::discord) fn owes(channel: u64, path: &str, start: u64) -> bool {
    use crate::services::tui_o::shadow::capture::file_identity;
    if path.is_empty() {
        return false;
    }
    let root = crate::config::runtime_root();
    let store = root.and_then(|root| crate::services::tui_o::store::OStore::existing(&root));
    let Some(Ok(Some(init))) = store.map(|store| store.read_init(channel)) else {
        return false;
    };
    // The path and the file it names now must both match, so a replaced file is not owed.
    let Ok(file) = std::fs::metadata(path).map(|meta| file_identity(&meta)) else {
        return false;
    };
    init.sources.iter().any(|source| {
        let id = &source.source_id;
        id.path == std::path::Path::new(path)
            && (id.dev, id.ino) == file
            && source.delivery_start <= start
    })
}

/// `owes` for a row's own channel, transcript and turn start.
pub(in crate::services::discord) fn owes_row(state: &InflightTurnState) -> bool {
    let start = state.turn_start_offset.unwrap_or(state.last_offset);
    let path = state.output_path.as_deref().unwrap_or_default();
    owes(state.channel_id, path, start)
}

/// Settles a stale leak O owes: the row's response offset moves to `end` as a delivered tail does.
pub(in crate::services::discord) fn settle_owed_leak(
    provider: &ProviderKind,
    state: &InflightTurnState,
    end: usize,
) -> bool {
    owes_row(state)
        && matches!(
            inflight::persist_leak_recovery_response_offset_if_matches_identity_locked(
                provider,
                state.channel_id,
                state,
                end,
            ),
            GuardedSaveOutcome::Saved
        )
}
