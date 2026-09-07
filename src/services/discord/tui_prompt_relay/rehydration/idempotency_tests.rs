use super::*;
use crate::services::discord::inflight::{
    GuardedSaveOutcome, InflightTurnIdentity, load_inflight_state_read_only, save_inflight_state,
    save_stream_tick_state_if_bridge_authority,
};
use crate::services::tui_prompt_dedupe as dedupe;
use std::sync::Barrier;
use std::sync::atomic::AtomicUsize;

const SESSION: &str = "AgentDesk-codex-5755-idempotency";
const CHANNEL: u64 = 5755;

fn fixture(test: impl FnOnce(&Path)) {
    let temp = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(temp.path());
    let _dedupe = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    dedupe::reset_state_for_tests();
    test(temp.path());
    dedupe::reset_state_for_tests();
}

fn rollout(root: &Path, id: &str) -> PathBuf {
    let path = root.join(format!("{id}.jsonl"));
    std::fs::write(
        &path,
        format!("{{\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\"}}}}\n"),
    )
    .unwrap();
    crate::services::codex_tui::session::write_codex_tui_rollout_marker(SESSION, &path, Some(id))
        .unwrap();
    path
}

fn rehydrate(observe: impl FnOnce()) -> Option<dedupe::TuiRuntimeBinding> {
    rehydrate_codex_tui_binding_transaction(
        SESSION,
        CHANNEL,
        &claimed_codex_tui_rollout_paths(),
        &HashSet::new(),
        &HashSet::new(),
        false,
        observe,
    )
}

#[test]
fn second_rehydration_is_noop_and_preserves_stream_row_identity() {
    fixture(|root| {
        let path = rollout(root, "first");
        let first = rehydrate(|| {}).expect("initial restore");
        let mut progressed = first.clone();
        progressed.last_offset = 17;
        progressed.relay_last_offset = Some(11);
        dedupe::register_tmux_runtime_binding(SESSION, progressed.clone());
        let mut state = InflightTurnState::new(
            ProviderKind::Codex,
            CHANNEL,
            None,
            123,
            456,
            789,
            "working".into(),
            Some("first".into()),
            Some(SESSION.into()),
            Some(path.display().to_string()),
            None,
            17,
        );
        state.runtime_kind = Some(RuntimeHandoffKind::CodexTui);
        state.full_response = "partial answer".into();
        save_inflight_state(&state).unwrap();
        let before = load_inflight_state_read_only(&ProviderKind::Codex, CHANNEL).unwrap();
        let second = rehydrate(|| panic!("valid binding must not be re-registered"));
        let after = load_inflight_state_read_only(&ProviderKind::Codex, CHANNEL)
            .expect("active stream row must survive rehydration");
        assert_eq!(
            InflightTurnIdentity::from_state(&after),
            InflightTurnIdentity::from_state(&before)
        );
        assert_eq!(
            serde_json::to_value(&after).unwrap(),
            serde_json::to_value(&before).unwrap()
        );
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session(SESSION),
            Some(progressed)
        );
        assert!(
            second.is_none(),
            "second successful observation must be a no-op, not another rehydration"
        );
        let identity = InflightTurnIdentity::from_state(&before);
        let (msg, len) = (before.current_msg_id, before.current_msg_len);
        let mut baseline = before.clone();
        let mut streaming = before;
        streaming.full_response.push_str(" continues");
        assert_eq!(
            save_stream_tick_state_if_bridge_authority(
                &mut baseline,
                &mut streaming,
                &identity,
                msg,
                len,
                "rehydration::idempotency_tests::streaming_edit",
            ),
            GuardedSaveOutcome::Saved,
            "the active bridge must retain its guarded streaming save authority"
        );
    });
}

#[test]
fn live_marker_rotation_replaces_even_when_old_rollout_still_exists() {
    fixture(|root| {
        let old = rollout(root, "old");
        rehydrate(|| {}).unwrap();
        let new = rollout(root, "new");
        let replaced = AtomicUsize::new(0);
        let fresh = rehydrate(|| {
            replaced.fetch_add(1, Ordering::SeqCst);
        })
        .unwrap();
        assert!(old.exists());
        assert_eq!(fresh.output_path, new.display().to_string());
        assert_eq!(fresh.session_id.as_deref(), Some("new"));
        assert_eq!(replaced.load(Ordering::SeqCst), 1);
        assert!(rehydrate(|| panic!("same replacement must settle")).is_none());
    });
}

#[test]
fn namespace_change_repairs_relay_path_without_rewinding_rollout() {
    fixture(|root| {
        rollout(root, "same-session");
        let mut previous = rehydrate(|| {}).unwrap();
        previous.last_offset = 13;
        previous.relay_output_path =
            Some(root.join("previous-namespace.jsonl").display().to_string());
        dedupe::register_tmux_runtime_binding(SESSION, previous);
        let repaired = rehydrate(|| {}).expect("namespace repair");
        assert_eq!(repaired.last_offset, 13);
        assert_eq!(
            repaired.relay_output_path,
            Some(crate::services::tmux_common::session_temp_path(
                SESSION, "jsonl"
            ))
        );
        assert!(rehydrate(|| panic!("namespace repair must settle")).is_none());
    });
}

#[test]
fn recreated_tmux_with_evicted_binding_can_rehydrate_same_rollout() {
    fixture(|root| {
        rollout(root, "surviving-rollout");
        let first = rehydrate(|| {}).unwrap();
        assert!(dedupe::evict_dead_tmux_mirror(SESSION));
        let restored = rehydrate(|| {}).expect("new pane may restore the surviving rollout");
        assert_eq!(restored.output_path, first.output_path);
        assert!(rehydrate(|| panic!("recreated pane must settle")).is_none());
    });
}

#[test]
fn concurrent_rehydrations_install_once() {
    fixture(|root| {
        rollout(root, "concurrent");
        let ready = Barrier::new(2);
        let installs = AtomicUsize::new(0);
        let restored = std::thread::scope(|scope| {
            let run = || {
                ready.wait();
                rehydrate(|| {
                    assert!(
                        crate::services::tmux_common::try_with_tmux_source_authority(
                            SESSION,
                            |_| ()
                        )
                        .is_none()
                    );
                    installs.fetch_add(1, Ordering::SeqCst);
                })
                .is_some()
            };
            let left = scope.spawn(run);
            let right = scope.spawn(run);
            usize::from(left.join().unwrap()) + usize::from(right.join().unwrap())
        });
        assert_eq!(installs.load(Ordering::SeqCst), 1);
        assert_eq!(restored, 1, "only the winning call may report rehydration");
    });
}

#[test]
fn unusable_marker_preserves_valid_binding_and_cursor() {
    fixture(|root| {
        rollout(root, "valid");
        let existing = rehydrate(|| {}).unwrap();
        let foreign = rollout(root, "foreign");
        for (claims, duplicates) in [
            (
                HashSet::from([canonical_rollout_claim_path(&foreign)]),
                HashSet::new(),
            ),
            (
                HashSet::new(),
                HashSet::from([canonical_rollout_claim_path(&foreign)]),
            ),
        ] {
            assert!(
                rehydrate_codex_tui_binding_transaction(
                    SESSION,
                    CHANNEL,
                    &claims,
                    &HashSet::new(),
                    &duplicates,
                    false,
                    || panic!("foreign or duplicate marker must not replace a valid binding"),
                )
                .is_none()
            );
            assert_eq!(
                dedupe::runtime_binding_for_tmux_session(SESSION),
                Some(existing.clone())
            );
        }
        std::fs::remove_file(foreign).unwrap();
        assert!(rehydrate(|| panic!("stale marker must not replace a valid binding")).is_none());
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session(SESSION),
            Some(existing)
        );
    });
}

#[test]
fn rollout_alias_and_channel_mirror_repair_preserve_binding() {
    fixture(|root| {
        let path = rollout(root, "aliased");
        let existing = rehydrate(|| {}).unwrap();
        let alias = root.join("alias.jsonl");
        std::os::unix::fs::symlink(path, &alias).unwrap();
        crate::services::codex_tui::session::write_codex_tui_rollout_marker(
            SESSION,
            &alias,
            Some("aliased"),
        )
        .unwrap();
        dedupe::register_tmux_channel(SESSION, CHANNEL + 1);
        assert!(
            rehydrate(|| panic!("alias and channel mirror repair must not re-register source"))
                .is_none()
        );
        assert_eq!(
            dedupe::owner_channel_for_tmux_session(SESSION),
            Some(CHANNEL)
        );
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session(SESSION),
            Some(existing)
        );
    });
}

#[test]
fn incomplete_session_metadata_does_not_reset_a_valid_rollout_cursor() {
    fixture(|root| {
        rollout(root, "metadata");
        let mut existing = rehydrate(|| {}).unwrap();
        existing.session_id = None;
        existing.last_offset = 13;
        dedupe::register_tmux_runtime_binding(SESSION, existing.clone());
        assert!(
            rehydrate(|| panic!("learning optional metadata must not reset the source")).is_none()
        );
        assert_eq!(
            dedupe::runtime_binding_for_tmux_session(SESSION),
            Some(existing)
        );
    });
}
