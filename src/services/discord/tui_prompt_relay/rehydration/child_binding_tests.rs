use super::*;
use crate::services::codex_tui::session;
use crate::services::tui_o::shadow::ShadowProvider;
use crate::services::tui_o::writer::binding::{
    BindingEvents, BindingRecord, BindingTarget, ChannelBindingLog,
};
use crate::services::tui_prompt_dedupe as dedupe;
use std::os::unix::fs::PermissionsExt;

#[test]
fn child_marker_rehydrate_publishes_no_child_and_fallback_excludes_it() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _home = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "CODEX_HOME",
        root.path(),
    );
    let sessions = root.path().join("sessions");
    let bin = root.path().join("bin");
    std::fs::create_dir_all(&sessions).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let tmux = bin.join("tmux");
    let cwd = root.path().display().to_string().replace('\'', "'\\''");
    std::fs::write(&tmux, format!("#!/bin/sh\nprintf '%s\\n' '{cwd}'\n")).unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _path = crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(&bin);
    let _lock = dedupe::TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    dedupe::reset_state_for_tests();
    dedupe::binding_events::set_test_root(Some(root.path()));
    struct RestoreLogRoot;
    impl Drop for RestoreLogRoot {
        fn drop(&mut self) {
            dedupe::binding_events::set_test_root(None);
        }
    }
    let _log = RestoreLogRoot;
    let name = "codex-child-marker-test";
    let channel = 655401;
    let child = sessions.join("rollout-child.jsonl");
    let mut header: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../../tests/fixtures/tui_input/codex-subagent-session-meta.json"
    ))
    .unwrap();
    header["payload"]["cwd"] = serde_json::json!(root.path());
    std::fs::write(&child, format!("{header}\n")).unwrap();
    // Persist the pre-upgrade marker directly, bypassing today's writer admission.
    let marker_path = crate::services::tmux_common::session_temp_path(
        name,
        crate::services::tmux_common::CODEX_TUI_ROLLOUT_MARKER_TEMP_EXT,
    );
    std::fs::create_dir_all(Path::new(&marker_path).parent().unwrap()).unwrap();
    let old_marker =
        serde_json::json!({"rollout_path": child,"session_id": header["payload"]["id"]});
    std::fs::write(&marker_path, format!("{old_marker}\n")).unwrap();
    let empty = HashSet::new();
    let restored =
        rehydrate_codex_tui_binding_transaction(name, channel, &empty, &empty, &empty, true, || {});
    assert!(
        restored.is_none(),
        "rehydrate must not restore the child's binding"
    );
    assert!(dedupe::runtime_binding_for_tmux_session(name).is_none());
    assert!(
        ChannelBindingLog::new(channel, ShadowProvider::Codex)
            .binding_events_since(channel, 0)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        codex_tui_marker_rehydrate_decision(
            &session::read_codex_tui_rollout_marker(name).unwrap(),
            &empty,
            &empty
        ),
        CodexTuiMarkerRehydrateDecision::TryFallback,
    );
    let parent = sessions.join("rollout-parent.jsonl");
    std::fs::write(
        &parent,
        format!(
            "{}\n",
            serde_json::json!({"type":"session_meta","payload":{
                "id":"01a0be01-eb52-7240-a7e2-318369cf6a6c","cwd":root.path(),"source":"cli"
            }})
        ),
    )
    .unwrap();
    let restored =
        rehydrate_codex_tui_binding_transaction(name, channel, &empty, &empty, &empty, true, || {})
            .expect("the rejected child marker must permit parent fallback");
    assert_eq!(restored.output_path, parent.display().to_string());
    let events = ChannelBindingLog::new(channel, ShadowProvider::Codex)
        .binding_events_since(channel, 0)
        .unwrap();
    assert_eq!(events.len(), 1);
    let BindingRecord::Bound {
        new: BindingTarget::Source(source),
        ..
    } = &events[0].record
    else {
        panic!("fallback must publish a parent source to O")
    };
    assert_eq!(source.path, parent);
    dedupe::reset_state_for_tests();
}
