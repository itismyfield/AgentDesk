use super::*;

/// Operator-only removal permits pinned restart/rebind rows after exact episode validation.
/// The guard selects the path; no caller-supplied row or unrelated lock can authorize unlink.
#[allow(dead_code)]
pub(in crate::services::discord) fn operator_disposition_remove_pinned(
    guard: &super::super::super::store::InflightStateFileLock,
    expected: &InflightEpisodePin,
) -> (GuardedClearOutcome, Option<InflightTurnState>) {
    let path = guard.state_path();
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (GuardedClearOutcome::Missing, None);
        }
        Err(_) => return (GuardedClearOutcome::IoError, None),
    };
    let Ok(state) = serde_json::from_slice::<InflightTurnState>(&bytes) else {
        return (GuardedClearOutcome::IoError, None);
    };
    if !expected.matches_state(&state) {
        return (GuardedClearOutcome::UserMsgMismatch, None);
    }
    let Some(provider) = state.provider_kind() else {
        return (GuardedClearOutcome::IoError, None);
    };
    let identity = InflightTurnIdentity::from_state(&state);
    super::remove_identity_matched_state(
        path,
        &provider,
        state.channel_id,
        &identity,
        state,
        "operator_disposition_remove_pinned",
    )
}
