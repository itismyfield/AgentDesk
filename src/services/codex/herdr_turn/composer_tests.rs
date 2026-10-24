use super::*;
use crate::db::dispatched_sessions::hosted_execution::{HostedLocation, SourceRef};
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};

#[test]
fn codex_actual_composer_entry_preserves_draft_modal_busy_and_capture_failure() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let nonce = "e-codex-composer";
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "hash".into(),
        channel_id: "1".into(),
        logical_key: "e-codex-composer".into(),
        owner_node: NODE.into(),
        runtime_root: "test".into(),
    };
    let record = HostedExecution {
        schema: 1,
        state: HostedState::Bound,
        execution_nonce: nonce.into(),
        owner,
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        }),
        expected: Some(rig.expected(nonce)),
        source_ref: SourceRef {
            runtime_root: "test".into(),
            channel: "1".into(),
            provider: "claude".into(),
            logical_key: "e-codex-composer".into(),
            execution_nonce: nonce.into(),
            initial_source: None,
            baseline_event_seq: None,
        },
    };
    let target = crate::services::session_host::herdr_endpoints()
        .target(&record)
        .unwrap();
    for (name, screen) in [
        ("draft", Some("\n› my unsent draft\ngpt-5.4 xhigh · /tmp")),
        (
            "modal",
            Some("Approval required\n› 1. Yes\ngpt-5.4 xhigh · /tmp"),
        ),
        (
            "busy",
            Some("• Working (1s • esc to interrupt)\n\n› \ngpt-5.4 xhigh · /tmp"),
        ),
        ("capture failure", None),
    ] {
        rig.answer("pane.read", screen.map_or_else(|| serde_json::json!({"type":"ok"}), |text| serde_json::json!({"type":"pane_read", "read":{"pane_id":PANE, "source":"recent_unwrapped", "text":text, "revision":1, "truncated":false}})));
        assert!(
            empty_composer(&target, None).is_err(),
            "codex_composer_preserves_input_contract: {name}"
        );
        assert!(
            rig.sends().is_empty(),
            "codex_composer_zero_mutation_contract: {name}"
        );
    }
    rig.answer("pane.read", serde_json::json!({"type":"pane_read", "read":{"pane_id":PANE, "source":"recent_unwrapped", "text":"\n› \ngpt-5.4 xhigh · /tmp", "revision":2, "truncated":false}}));
    assert_eq!(
        empty_composer(&target, None),
        Ok(()),
        "codex_human_finished_composer_progress_contract"
    );
    assert!(rig.sends().is_empty());
}
