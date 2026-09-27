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

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Claude,
            6294,
            None,
            42,
            123,
            0,
            "operator disposition".into(),
            None,
            Some("disposition-fixture".into()),
            None,
            None,
            0,
        )
    }

    #[test]
    fn operator_disposition_pin_mismatch_never_unlinks() {
        let root = tempfile::tempdir().unwrap();
        let row = row();
        let path = inflight_state_path(root.path(), &ProviderKind::Claude, row.channel_id);
        let guard = lock_inflight_state_path(&path).unwrap();
        let bytes = serde_json::to_vec(&row).unwrap();
        fs::write(&path, &bytes).unwrap();
        let mut successor = row.clone();
        successor.user_msg_id += 1;
        let mut other_nonce = row.clone();
        other_nonce.turn_nonce = Some("other-episode".into());
        for mismatched in [successor, other_nonce] {
            let result = operator_disposition_remove_pinned(
                &guard,
                &InflightEpisodePin::from_state(&mismatched),
            );
            assert_eq!(result.0, GuardedClearOutcome::UserMsgMismatch);
            assert!(result.1.is_none());
            assert_eq!(fs::read(&path).unwrap(), bytes);
        }
    }

    #[test]
    fn operator_disposition_rereads_guarded_bytes_and_removes_rebind_row() {
        let root = tempfile::tempdir().unwrap();
        let mut row = row();
        row.rebind_origin = true;
        let path = inflight_state_path(root.path(), &ProviderKind::Claude, row.channel_id);
        let guard = lock_inflight_state_path(&path).unwrap();
        let pin = InflightEpisodePin::from_state(&row);
        let mut replacement = row.clone();
        replacement.user_msg_id += 1;
        fs::write(&path, serde_json::to_vec(&replacement).unwrap()).unwrap();
        assert_eq!(
            operator_disposition_remove_pinned(&guard, &pin).0,
            GuardedClearOutcome::UserMsgMismatch
        );
        fs::write(&path, serde_json::to_vec(&row).unwrap()).unwrap();
        let (outcome, removed) = operator_disposition_remove_pinned(&guard, &pin);
        assert_eq!(outcome, GuardedClearOutcome::Cleared);
        assert!(removed.unwrap().rebind_origin);
        assert!(!path.exists());
        assert!(super::super::super::super::second_handle_try_lock(&path).is_err());
    }
}
