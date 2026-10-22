use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedState, SourceRef,
};
use crate::services::session_host::herdr_endpoints;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};

const NONCE: &str = "e-claude-composer";
fn pane(above: &str, input: &str) -> String {
    format!(
        "{above}\n────────────────────────────────────────────────────\n❯ {input}\n────────────────────────────────────────────────────\n  🤖 Opus(H) │ 7% │ MCP: 2"
    )
}

#[test]
fn herdr_claude_entry_holds_draft_modal_busy_and_failed_capture_without_paste_or_enter() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let context = rig.context(NONCE);
    rig.run_provider(&context, false);
    let logical = "e-claude-composer";
    std::fs::write(
        crate::services::tmux_common::session_temp_path(logical, "host_kind"),
        "herdr",
    )
    .unwrap();
    let owner = HostedOwner {
        provider: "claude".into(),
        discord_token_hash: "hash".into(),
        channel_id: "1".into(),
        logical_key: logical.into(),
        owner_node: NODE.into(),
        runtime_root: "test".into(),
    };
    let record = HostedExecution {
        schema: 1,
        state: HostedState::Bound,
        execution_nonce: NONCE.into(),
        owner: owner.clone(),
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        }),
        expected: Some(rig.expected(NONCE)),
        source_ref: SourceRef {
            runtime_root: "test".into(),
            channel: "1".into(),
            provider: "claude".into(),
            logical_key: logical.into(),
            execution_nonce: NONCE.into(),
            initial_source: None,
            baseline_event_seq: None,
        },
    };
    let target = herdr_endpoints().target(&record).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused_composer_test")
        .unwrap();
    let cancel = std::sync::Arc::new(CancelToken::new());
    let on_send = cancel.clone();
    rig.on_send(move |_| {
        on_send
            .cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    });
    let _admission = crate::services::herdr_admission::force_for_test(
        crate::services::herdr_admission::Admission::new(None, Some(root.path().join("no-stop"))),
    );
    let endpoint = crate::services::herdr_launch::HerdrLaunchEndpoint {
        execution_node: NODE.into(),
        config_key: KEY.into(),
        socket_addr: rig.socket().display().to_string(),
        herdr_session: SESSION.into(),
    };
    for (name, screen) in [
        ("draft", Some(pane("✻ Baked for 2s", "my unsent draft"))),
        (
            "modal",
            Some(pane(
                "Do you want to proceed?\n❯ 1. Yes\nEnter to confirm",
                "",
            )),
        ),
        (
            "busy",
            Some(pane("✳ Architecting… (12s · esc to interrupt)", "")),
        ),
        ("capture failure", None),
    ] {
        cancel
            .cancelled
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let result = screen.map_or_else(|| serde_json::json!({"type":"ok"}), |text| serde_json::json!({"type":"pane_read", "read":{"pane_id":PANE, "source":"recent_unwrapped", "text":text, "revision":1, "truncated":false}}));
        rig.answer("pane.read", result);
        let turn = super::super::HerdrTurn {
            pool: &pool,
            owner: owner.clone(),
            channel_id: 1,
            endpoint: endpoint.clone(),
            row: None,
            prompt: "preserve queued input",
            working_dir: "/tmp",
            system_prompt: None,
            model: None,
            hook_endpoint: None,
            cancel: Some(cancel.clone()),
        };
        let attached = super::super::Attached {
            nonce: NONCE.into(),
            session_id: "same-session".into(),
            transcript: root.path().join("same-transcript.jsonl"),
            target: target.clone(),
            bound: true,
        };
        let (sender, _receiver) = std::sync::mpsc::channel();
        assert!(
            super::super::prompt_and_read(&turn, runtime.handle(), attached, sender).is_err(),
            "composer_guard_refuses_before_input_contract: {name}"
        );
        assert!(
            rig.sends().is_empty(),
            "composer_guard_zero_mutation_contract: {name}"
        );
        assert!(
            super::super::not_held(NONCE).is_ok(),
            "an untouched refusal releases only its own transient hold"
        );
    }
    let ready_screen = pane("✻ Baked for 2s", "").replace("❯", "\x1b[36m❯\x1b[0m");
    assert!(
        ready(Some(&ready_screen)),
        "human_finished_composer_ready_contract"
    );
    rig.answer("pane.read", serde_json::json!({"type":"pane_read", "read":{"pane_id":PANE, "source":"recent_unwrapped", "text":ready_screen, "revision":2, "truncated":false}}));
    let plan = [
        TuiInputAction::PasteBuffer("after human draft".into()),
        TuiInputAction::Enter,
    ];
    assert!(
        matches!(run(&target, &plan, None), InputRun::Applied),
        "human_finished_actual_send_contract"
    );
    assert_eq!(rig.sends().len(), 2);
}
