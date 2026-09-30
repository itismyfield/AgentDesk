//! Hosted binding of an inflight row: `sessions.id`, execution nonce and host locator.
//! A same-turn write may install a complete binding once and never change it afterwards.

use super::*;
use crate::services::discord::inflight::host_locator::PersistedHostLocator;

type HostedBinding<'a> = (
    Option<i64>,
    Option<&'a str>,
    Option<&'a PersistedHostLocator>,
);

/// The binding as one comparable value; all `None` is a legacy row.
pub(super) fn hosted_binding(state: &InflightTurnState) -> HostedBinding<'_> {
    (
        state.hosted_record_id,
        state.hosted_execution_nonce.as_deref(),
        state.host_locator.as_ref(),
    )
}

/// A legacy row takes only a complete binding with a readable locator; a bound row
/// takes only its own binding, so another nonce, record or locator is refused.
pub(super) fn hosted_binding_write_admitted(
    durable: &InflightTurnState,
    requested: &InflightTurnState,
) -> bool {
    let requested_binding = hosted_binding(requested);
    match hosted_binding(durable) {
        (None, None, None) => match requested_binding {
            (None, None, None) => true,
            (Some(_), Some(nonce), Some(PersistedHostLocator::Known(_))) => {
                !nonce.trim().is_empty()
            }
            _ => false,
        },
        durable_binding => durable_binding == requested_binding,
    }
}

pub(super) fn copy_hosted_binding(target: &mut InflightTurnState, source: &InflightTurnState) {
    target.hosted_record_id = source.hosted_record_id;
    target
        .hosted_execution_nonce
        .clone_from(&source.hosted_execution_nonce);
    target.host_locator.clone_from(&source.host_locator);
}

#[cfg(test)]
mod tests {
    use super::super::runtime_stamp::stamp_runtime_handoff_if_matches_identity_in_root as stamp;
    use super::super::save_inflight_state_if_identity_unchanged_in_root as save_gated;
    use super::*;
    use crate::services::session_host::{HostKind, HostedRuntimeLocator};

    const TMUX: &str = "AgentDesk-claude-hosted";

    fn seed(channel_id: u64) -> InflightTurnState {
        InflightTurnState::new(
            ProviderKind::Claude,
            channel_id,
            Some("adk-hosted".to_string()),
            343_742_347_365_974_026,
            77_010,
            18,
            "hosted binding".to_string(),
            Some("provider-session".to_string()),
            Some(TMUX.to_string()),
            Some("/seeded/output.jsonl".to_string()),
            None,
            512,
        )
    }

    fn bound(
        mut state: InflightTurnState,
        record: i64,
        nonce: &str,
        host: &str,
    ) -> InflightTurnState {
        state.hosted_record_id = Some(record);
        state.hosted_execution_nonce = Some(nonce.to_string());
        state.host_locator = Some(PersistedHostLocator::Known(HostedRuntimeLocator {
            execution_node: None,
            host_kind: HostKind::Tmux,
            host_session_id: host.to_string(),
            pane: None,
        }));
        state
    }

    fn raw(root: &Path, channel_id: u64) -> String {
        let path = inflight_state_path(root, &ProviderKind::Claude, channel_id);
        std::fs::read_to_string(path).expect("read inflight row")
    }

    fn load(root: &Path, channel_id: u64) -> InflightTurnState {
        serde_json::from_str(&raw(root, channel_id)).expect("parse inflight row")
    }

    #[test]
    fn hosted_binding_is_installed_once_and_only_its_own_nonce_and_locator_may_write() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_634_001;
        let legacy = seed(channel_id);
        save_inflight_state_in_root(root.path(), &legacy).expect("seed row");
        let first = bound(legacy.clone(), 7, "nonce-a", TMUX);
        let expected = InflightTurnIdentity::from_state(&legacy);
        // Runtime handoff callers stamp against the persisted baseline they read.
        let mut local = first.clone();
        assert_eq!(
            stamp(
                root.path(),
                (&legacy, &mut local),
                &expected,
                "test::first_bind"
            ),
            GuardedSaveOutcome::Saved
        );
        let row = load(root.path(), channel_id);
        assert_eq!(hosted_binding(&row), hosted_binding(&first));

        let mut same = row.clone();
        same.output_path = Some("/runtime/same-nonce.jsonl".to_string());
        let expected = InflightTurnIdentity::from_state(&row);
        assert_eq!(
            stamp(root.path(), &same, &expected, "test::same_nonce"),
            GuardedSaveOutcome::Saved,
            "the binding's own nonce and locator restamp"
        );
        let row = load(root.path(), channel_id);
        assert_eq!(
            row.output_path.as_deref(),
            Some("/runtime/same-nonce.jsonl")
        );
        assert_eq!(hosted_binding(&row), hosted_binding(&first));

        let refused = [
            ("another nonce", bound(row.clone(), 7, "nonce-b", TMUX)),
            (
                "another locator",
                bound(row.clone(), 7, "nonce-a", "AgentDesk-claude-other"),
            ),
            ("another record", bound(row.clone(), 8, "nonce-a", TMUX)),
            ("binding erased", {
                let mut erased = row.clone();
                copy_hosted_binding(&mut erased, &legacy);
                erased
            }),
        ];
        let expected = InflightTurnIdentity::from_state(&row);
        for (label, write) in refused {
            // The full-state save refuses any output path change, so it carries other progress.
            let mut save = write.clone();
            save.last_offset += 1;
            let mut write = write;
            write.output_path = Some("/runtime/must-not-land.jsonl".to_string());
            assert_eq!(
                stamp(root.path(), &write, &expected, "test::refused_stamp"),
                GuardedSaveOutcome::AuthorityPinned,
                "{label}"
            );
            assert_eq!(
                save_gated(root.path(), &save, "test::refused_save"),
                GuardedSaveOutcome::AuthorityPinned,
                "{label}: a full-state save is refused too"
            );
            let after = load(root.path(), channel_id);
            assert_eq!(after.output_path, row.output_path, "{label}");
            assert_eq!(after.last_offset, row.last_offset, "{label}");
            assert_eq!(hosted_binding(&after), hosted_binding(&row), "{label}");
        }
        let mut own = row.clone();
        own.last_offset += 1;
        assert_eq!(
            save_gated(root.path(), &own, "test::own_save"),
            GuardedSaveOutcome::Saved,
            "a full-state save with the row's own binding still lands"
        );
    }

    #[test]
    fn legacy_rows_without_a_binding_stamp_and_reload_unchanged() {
        let root = tempfile::tempdir().expect("runtime root");
        let channel_id = 42_634_002;
        let legacy = seed(channel_id);
        save_inflight_state_in_root(root.path(), &legacy).expect("seed row");
        let mut stamped = legacy.clone();
        stamped.runtime_kind = Some(RuntimeHandoffKind::ClaudeTui);
        stamped.output_path = Some("/runtime/legacy-stamp.jsonl".to_string());
        let expected = InflightTurnIdentity::from_state(&legacy);
        assert_eq!(
            stamp(root.path(), &stamped, &expected, "test::legacy_stamp"),
            GuardedSaveOutcome::Saved
        );
        let text = raw(root.path(), channel_id);
        for key in ["host_locator", "hosted_record_id", "hosted_execution_nonce"] {
            assert!(!text.contains(key), "a legacy row must not gain {key}");
        }
        let row = load(root.path(), channel_id);
        assert_eq!(
            row.output_path.as_deref(),
            Some("/runtime/legacy-stamp.jsonl")
        );
        assert_eq!(hosted_binding(&row), (None, None, None));

        let mut nonce_only = row.clone();
        nonce_only.hosted_execution_nonce = Some("nonce-a".to_string());
        let mut unreadable = bound(row.clone(), 7, "nonce-a", TMUX);
        unreadable.host_locator = Some(PersistedHostLocator::Unknown(
            serde_json::json!({"host_kind": "zellij"}),
        ));
        let expected = InflightTurnIdentity::from_state(&row);
        for (label, write) in [
            ("nonce only", nonce_only),
            ("unreadable locator", unreadable),
        ] {
            assert_eq!(
                stamp(root.path(), &write, &expected, "test::partial_bind"),
                GuardedSaveOutcome::AuthorityPinned,
                "{label}: only a complete binding installs"
            );
            assert_eq!(
                hosted_binding(&load(root.path(), channel_id)),
                (None, None, None)
            );
        }
    }
}
