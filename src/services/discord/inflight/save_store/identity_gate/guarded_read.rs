//! Shared guarded-write read classification.

use super::*;

/// Caller must hold the canonical sidecar lock for `path`.
pub(super) fn read_inflight_state_for_guarded_write(
    path: &Path,
    provider: &ProviderKind,
    channel_id: u64,
    expected: &InflightTurnIdentity,
    caller: &'static str,
) -> Result<InflightTurnState, GuardedSaveOutcome> {
    let data = match fs::read_to_string(path) {
        Ok(data) => data,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            tracing::debug!(
                provider = %provider.as_str(),
                channel_id,
                caller,
                snapshot_identity = ?expected,
                "guarded inflight write skipped because the durable row is missing"
            );
            return Err(GuardedSaveOutcome::Missing);
        }
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id,
                caller,
                snapshot_identity = ?expected,
                error = %error,
                "guarded inflight write could not read the durable row"
            );
            return Err(GuardedSaveOutcome::IoError);
        }
    };
    serde_json::from_str::<InflightTurnState>(&data).map_err(|error| {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id,
            caller,
            snapshot_identity = ?expected,
            error = %error,
            "guarded inflight write found malformed durable JSON"
        );
        GuardedSaveOutcome::IoError
    })
}
