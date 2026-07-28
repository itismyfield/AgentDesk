//! Bridge-entry lock-held narrow inflight patch (#4259 R4).

use super::*;
use crate::services::discord::inflight::store::persist_under_lock_with_snapshot;

fn field_is_contended<T: PartialEq>(before: &T, after: &T, durable: &T) -> bool {
    before != after && durable != before && durable != after
}

fn apply_changed<T: Clone + PartialEq>(before: &T, after: &T, durable: &mut T) {
    if before != after {
        durable.clone_from(after);
    }
}

/// Applies only fields bridge entry changed, onto a row freshly re-read under
/// the canonical per-channel sidecar lock. The UI/response fields are one CAS
/// group: if a watcher changed any field that this bridge also wants to change,
/// the whole group stays at the newer durable values instead of becoming a
/// mixed stale/new panel state. On success the merged row is copied back into
/// `after`, preventing later bridge saves from reintroducing the stale snapshot.
pub(in crate::services::discord) fn patch_bridge_entry_state_if_identity_unchanged(
    before: &InflightTurnState,
    after: &mut InflightTurnState,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    patch_bridge_entry_state_if_identity_unchanged_in_root(&root, before, after, caller)
}

pub(in crate::services::discord) fn patch_bridge_entry_state_tracking_placeholder_clear(
    before: &InflightTurnState,
    after: &mut InflightTurnState,
    placeholder_clear_applied: &mut bool,
    caller: &'static str,
) -> GuardedSaveOutcome {
    *placeholder_clear_applied = false;
    let Some(root) = inflight_runtime_root() else {
        return GuardedSaveOutcome::IoError;
    };
    patch_bridge_entry_state_if_identity_unchanged_in_root_impl(
        &root,
        before,
        after,
        Some(placeholder_clear_applied),
        caller,
    )
}

pub(in crate::services::discord::inflight) fn patch_bridge_entry_state_if_identity_unchanged_in_root(
    root: &Path,
    before: &InflightTurnState,
    after: &mut InflightTurnState,
    caller: &'static str,
) -> GuardedSaveOutcome {
    patch_bridge_entry_state_if_identity_unchanged_in_root_impl(root, before, after, None, caller)
}

fn patch_bridge_entry_state_if_identity_unchanged_in_root_impl(
    root: &Path,
    before: &InflightTurnState,
    after: &mut InflightTurnState,
    placeholder_clear_applied: Option<&mut bool>,
    caller: &'static str,
) -> GuardedSaveOutcome {
    let Some(provider) = before.provider_kind() else {
        return GuardedSaveOutcome::IoError;
    };
    let expected = InflightTurnIdentity::from_state(before);
    if before.provider != after.provider
        || before.channel_id != after.channel_id
        || !expected.matches_state(after)
    {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    let path = inflight_state_path(root, &provider, before.channel_id);
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return GuardedSaveOutcome::IoError;
    }
    let Ok(_lock) = lock_inflight_state_path(&path) else {
        return GuardedSaveOutcome::IoError;
    };
    let mut on_disk = match super::read_inflight_state_for_guarded_write(
        &path,
        &provider,
        before.channel_id,
        &expected,
        caller,
    ) {
        Ok(on_disk) => on_disk,
        Err(outcome) => return outcome,
    };
    if expected.user_msg_id == 0 && expected.turn_start_offset.is_none() {
        return GuardedSaveOutcome::IdentityMismatch;
    }
    if on_disk.restart_mode.is_some() || on_disk.rebind_origin || !expected.matches_state(&on_disk)
    {
        return GuardedSaveOutcome::IdentityMismatch;
    }

    let bridge_placeholder_clear_applied = before.long_running_placeholder_active
        && !after.long_running_placeholder_active
        && on_disk.long_running_placeholder_active == before.long_running_placeholder_active;
    if !field_is_contended(
        &before.watcher_owner_channel_id,
        &after.watcher_owner_channel_id,
        &on_disk.watcher_owner_channel_id,
    ) {
        apply_changed(
            &before.watcher_owner_channel_id,
            &after.watcher_owner_channel_id,
            &mut on_disk.watcher_owner_channel_id,
        );
    }

    let ui_patch_contended = field_is_contended(
        &before.status_message_id,
        &after.status_message_id,
        &on_disk.status_message_id,
    ) || field_is_contended(
        &before.status_panel_generation,
        &after.status_panel_generation,
        &on_disk.status_panel_generation,
    ) || field_is_contended(
        &before.current_msg_id,
        &after.current_msg_id,
        &on_disk.current_msg_id,
    ) || field_is_contended(
        &before.current_msg_len,
        &after.current_msg_len,
        &on_disk.current_msg_len,
    ) || field_is_contended(
        &before.full_response,
        &after.full_response,
        &on_disk.full_response,
    ) || field_is_contended(
        &before.response_sent_offset,
        &after.response_sent_offset,
        &on_disk.response_sent_offset,
    );
    if ui_patch_contended {
        tracing::warn!(
            provider = %provider.as_str(),
            channel_id = before.channel_id,
            caller,
            "bridge-entry UI patch skipped because a same-turn writer advanced an overlapping field"
        );
    } else {
        apply_changed(
            &before.status_message_id,
            &after.status_message_id,
            &mut on_disk.status_message_id,
        );
        apply_changed(
            &before.status_panel_generation,
            &after.status_panel_generation,
            &mut on_disk.status_panel_generation,
        );
        apply_changed(
            &before.current_msg_id,
            &after.current_msg_id,
            &mut on_disk.current_msg_id,
        );
        apply_changed(
            &before.current_msg_len,
            &after.current_msg_len,
            &mut on_disk.current_msg_len,
        );
        apply_changed(
            &before.full_response,
            &after.full_response,
            &mut on_disk.full_response,
        );
        apply_changed(
            &before.response_sent_offset,
            &after.response_sent_offset,
            &mut on_disk.response_sent_offset,
        );
    }

    if !field_is_contended(
        &before.long_running_placeholder_active,
        &after.long_running_placeholder_active,
        &on_disk.long_running_placeholder_active,
    ) {
        apply_changed(
            &before.long_running_placeholder_active,
            &after.long_running_placeholder_active,
            &mut on_disk.long_running_placeholder_active,
        );
    }

    match persist_under_lock_with_snapshot(
        root,
        &path,
        &on_disk,
        "src/services/discord/inflight/save_store/identity_gate/bridge_entry.rs:patch_bridge_entry_state_if_identity_unchanged_in_root",
    ) {
        Ok(Some(persisted)) => {
            after.clone_from(&persisted);
            if let Some(applied) = placeholder_clear_applied {
                *applied = bridge_placeholder_clear_applied;
            }
            GuardedSaveOutcome::Saved
        }
        Ok(None) => GuardedSaveOutcome::IdentityMismatch,
        Err(error) => {
            tracing::warn!(
                provider = %provider.as_str(),
                channel_id = before.channel_id,
                caller,
                error = %error,
                "bridge-entry inflight patch failed; leaving the durable row untouched"
            );
            GuardedSaveOutcome::IoError
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placeholder_state(channel_id: u64) -> InflightTurnState {
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            channel_id,
            Some("bridge-clear-effect".to_string()),
            343_742_347_365_974_026,
            77_010,
            18,
            "prompt".to_string(),
            Some("session".to_string()),
            Some("AgentDesk-bridge-clear-effect".to_string()),
            Some("/tmp/bridge-clear-effect.jsonl".to_string()),
            Some("/tmp/bridge-clear-effect.input".to_string()),
            512,
        );
        state.long_running_placeholder_active = true;
        state.current_msg_id = 901;
        state.current_msg_len = 12;
        state.full_response = "partial".to_string();
        state
    }

    #[test]
    fn placeholder_clear_effect_reports_only_the_bridge_winner() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_594_250;
        let before = placeholder_state(channel_id);
        let mut durable = before.clone();
        durable.long_running_placeholder_active = false;
        durable.current_msg_id = 902;
        durable.current_msg_len = 24;
        durable.full_response = "partial watcher completion".to_string();
        durable.response_sent_offset = durable.full_response.len();
        save_inflight_state_in_root(root.path(), &durable).expect("seed watcher winner");

        let mut after = before.clone();
        after.long_running_placeholder_active = false;
        let mut clear_applied = true;
        assert_eq!(
            patch_bridge_entry_state_if_identity_unchanged_in_root_impl(
                root.path(),
                &before,
                &mut after,
                Some(&mut clear_applied),
                "test::watcher_placeholder_clear_winner",
            ),
            GuardedSaveOutcome::Saved,
        );
        assert!(!clear_applied);
        assert_eq!(after.current_msg_id, 902);
        assert_eq!(after.full_response, "partial watcher completion");
        let persisted = load_inflight_state_unlocked(&inflight_state_path(
            root.path(),
            &ProviderKind::Codex,
            channel_id,
        ))
        .expect("load watcher-winner row");
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            serde_json::to_value(&persisted).unwrap(),
        );

        let bridge_root = tempfile::tempdir().expect("bridge-winner runtime root");
        save_inflight_state_in_root(bridge_root.path(), &before)
            .expect("seed bridge-owned placeholder");
        after = before.clone();
        after.long_running_placeholder_active = false;
        assert_eq!(
            patch_bridge_entry_state_if_identity_unchanged_in_root_impl(
                bridge_root.path(),
                &before,
                &mut after,
                Some(&mut clear_applied),
                "test::bridge_placeholder_clear_winner",
            ),
            GuardedSaveOutcome::Saved,
        );
        assert!(clear_applied);
        let persisted = load_inflight_state_unlocked(&inflight_state_path(
            bridge_root.path(),
            &ProviderKind::Codex,
            channel_id,
        ))
        .expect("load bridge-winner row");
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            serde_json::to_value(&persisted).unwrap(),
        );
    }
}
