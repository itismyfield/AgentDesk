use super::*;

pub(super) fn remove_identity_matched_state(
    path: &std::path::Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    state: InflightTurnState,
    reason: &'static str,
) -> (GuardedClearOutcome, Option<InflightTurnState>) {
    if receipt_protected_path(path) && !receipt_mutant("identity-clear") {
        return (GuardedClearOutcome::IoError, None);
    }
    if let Some(marker) = state.legacy_replay_coordinate.as_ref() {
        let Some(session) = state.tmux_session_name.as_deref() else {
            return (GuardedClearOutcome::IoError, None);
        };
        // Keep the canonical row lock and source authority through removal so a
        // replacement cannot turn an unconfirmed terminal into debt deletion.
        return crate::services::tmux_common::try_with_tmux_source_authority(session, |_| {
            let Some(source) = marker.installed.as_ref() else {
                return (GuardedClearOutcome::IoError, None);
            };
            let current_file = fs::File::open(&source.output_path)
                .and_then(|file| file.metadata())
                .ok()
                .filter(|metadata| metadata.is_file());
            #[cfg(unix)]
            let file_matches = current_file.is_some_and(|metadata| {
                use std::os::unix::fs::MetadataExt;
                (metadata.dev(), metadata.ino()) == source.file_identity
            });
            #[cfg(not(unix))]
            let file_matches = {
                let _ = current_file;
                false
            };
            if marker.version != 1
                || serde_json::to_value(marker.phase).ok() != Some(serde_json::json!("installed"))
                || state.output_path.as_deref() != Some(source.output_path.as_str())
                || source.generation_mtime_ns == 0
                || crate::services::discord::outbound::delivery_record::current_generation_mtime_ns(
                    session,
                ) != source.generation_mtime_ns
                || !file_matches
            {
                return (GuardedClearOutcome::IoError, None);
            }
            remove_identity_matched_state_unchecked(
                path,
                provider,
                channel_id,
                expected,
                state.clone(),
                reason,
            )
        })
        .unwrap_or((GuardedClearOutcome::IoError, None));
    }
    remove_identity_matched_state_unchecked(path, provider, channel_id, expected, state, reason)
}
