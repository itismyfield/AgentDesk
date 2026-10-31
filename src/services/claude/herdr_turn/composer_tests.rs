use super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedState, SourceRef,
};
use crate::services::session_host::herdr_endpoints;
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};

const NONCE: &str = "00112233445566778899aabbccddee01";
fn pane(above: &str, input: &str) -> String {
    format!(
        "{above}\n\n────────────────────────────────────────────────────\n❯ {input}\n────────────────────────────────────────────────────\n  🤖 Opus(H) │ 7% │ MCP: 2\n  ⏵⏵ bypass permissions on (shift+tab to cycle)"
    )
}

fn bound_target(rig: &HerdrRig, logical: &str) -> (HostedOwner, HerdrTarget) {
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
    (owner, herdr_endpoints().target(&record).unwrap())
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
    let (owner, target) = bound_target(&rig, logical);
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
        if let Some(capture) = screen.as_deref() {
            if name == "draft" {
                assert_eq!(
                    composer_owner(capture),
                    ComposerOwner::Person,
                    "draft_fixture_has_measured_person_ownership_contract"
                );
            }
            if name == "modal" {
                assert!(
                    crate::services::tmux_common::tmux_capture_indicates_claude_tui_interactive_modal(capture),
                    "modal_fixture_reaches_actual_modal_detector_contract"
                );
            }
        }
        cancel
            .cancelled
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let result = screen.map_or_else(|| serde_json::json!({"type":"ok"}), |text| serde_json::json!({"type":"pane_read", "read":{"pane_id":PANE, "source":"recent_unwrapped", "text":text, "revision":1, "truncated":false}}));
        rig.answer("pane.read", result);
        let reads_before = rig
            .requests()
            .iter()
            .filter(|request| request["method"] == "pane.read")
            .count();
        let turn = super::super::HerdrTurn {
            pool: &pool,
            owner: owner.clone(),
            channel_id: 1,
            endpoint: endpoint.clone(),
            row: None,
            prompt: "preserve queued input",
            working_dir: "/tmp",
            session_id: None,
            force_fresh: false,
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
            physical: None,
        };
        let (sender, _receiver) = std::sync::mpsc::channel();
        assert!(
            super::super::prompt_and_read(
                &turn,
                runtime.handle(),
                attached,
                crate::services::claude_tui::owner_input::Lineage::Existing,
                sender
            )
            .is_err(),
            "composer_guard_refuses_before_input_contract: {name}"
        );
        assert!(
            rig.sends().is_empty(),
            "composer_guard_zero_mutation_contract: {name}"
        );
        if matches!(name, "draft" | "modal") {
            let reads_after = rig
                .requests()
                .iter()
                .filter(|request| request["method"] == "pane.read")
                .count();
            assert_eq!(
                reads_after,
                reads_before + 1,
                "human_composer_refuses_immediately_contract: {name}"
            );
        }
        assert!(
            super::super::not_held(NONCE).is_ok(),
            "an untouched refusal releases only its own transient hold"
        );
    }
    let ready_screen = pane("✻ Baked for 2s", "").replace("❯", "\x1b[36m❯\x1b[0m");
    assert!(
        matches!(
            observation(Some(&ready_screen), false),
            ControlFlow::Break(true)
        ),
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

#[test]
fn herdr_claude_composer_waits_from_busy_to_ready_before_exactly_one_submit() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let context = rig.context(NONCE);
    rig.run_provider(&context, false);
    let logical = "i7309-claude-composer";
    std::fs::write(
        crate::services::tmux_common::session_temp_path(logical, "host_kind"),
        "herdr",
    )
    .unwrap();
    let (_owner, target) = bound_target(&rig, logical);
    let _admission = crate::services::herdr_admission::force_for_test(
        crate::services::herdr_admission::Admission::new(None, Some(root.path().join("no-stop"))),
    );
    let plan = [
        TuiInputAction::PasteBuffer("cold start input".into()),
        TuiInputAction::Enter,
    ];
    let cancel = CancelToken::new();
    cancel
        .cancelled
        .store(true, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        run(&target, &plan, Some(&cancel)),
        InputRun::Cancelled { confirmed: 0 },
        "cancel_before_composer_capture_contract"
    );
    assert!(
        rig.requests().is_empty(),
        "cancelled_composer_reads_zero_contract"
    );
    assert!(
        rig.sends().is_empty(),
        "cancelled_composer_sends_zero_contract"
    );
    cancel
        .cancelled
        .store(false, std::sync::atomic::Ordering::SeqCst);
    let response = |text| {
        serde_json::json!({"type":"pane_read", "read":{
            "pane_id":PANE, "source":"recent_unwrapped", "text":text,
            "revision":1, "truncated":false
        }})
    };
    rig.answer_sequence(
        "pane.read",
        [
            response(pane("✳ Architecting… (12s · esc to interrupt)", "")),
            response(pane("✻ Baked for 2s", "")),
        ],
    );
    assert_eq!(
        run(&target, &plan, Some(&cancel)),
        InputRun::Applied,
        "busy_then_ready_composer_applies_contract"
    );
    assert_eq!(
        rig.sends().len(),
        2,
        "busy_then_ready_composer_sends_two_contract"
    );
    let reads = rig
        .requests()
        .iter()
        .filter(|request| request["method"] == "pane.read")
        .count();
    assert_eq!(
        reads, 2,
        "busy_then_ready_composer_reads_both_screens_contract"
    );
}

// An `/exit` sends nothing while a draft is stashed under an empty composer, where a prompt goes on;
// with no stash both are sent.
#[test]
fn an_exit_is_never_sent_over_a_stashed_draft() {
    let clear = pane("✻ Baked for 2s", "");
    let stash_row = format!("{:>50}", "\u{203a} stashed");
    let stashed = clear.replacen("\n\n", &format!("\n{stash_row}\n"), 1);
    for (capture, exit, sent) in [
        (&stashed, true, false),
        (&stashed, false, true),
        (&clear, true, true),
    ] {
        let called = std::cell::Cell::new(false);
        let send = || {
            called.set(true);
            InputRun::Applied
        };
        let capture = || Some(capture.clone());
        let run = match exit {
            true => when_ready_to_exit(capture, None, send),
            false => when_ready(capture, None, send),
        };
        assert_eq!(called.get(), sent, "exit {exit}: {run:?}");
    }
}
