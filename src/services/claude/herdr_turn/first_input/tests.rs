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
fn cold_herdr_submit_returns_before_source_and_emits_init_before_one_paste_enter() {
    run_case(false);
}

#[test]
fn cold_herdr_payload_commit_refusal_preserves_raw_and_hold_with_zero_keys() {
    run_case(true);
}

fn run_case(refuse_commit: bool) {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let context = rig.context(NONCE);
    rig.run_provider(&context, false);
    let logical = "cold-claude-submit";
    std::fs::write(
        crate::services::tmux_common::session_temp_path(logical, "host_kind"),
        "herdr",
    )
    .unwrap();
    let (owner, target) = bound_target(&rig, logical);
    rig.answer("pane.read", serde_json::json!({"type":"pane_read","read":{"pane_id":PANE,
        "source":"recent_unwrapped","text":pane("✻ Baked for 2s", ""),"revision":1,"truncated":false}}));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .on_thread_start(rig.thread_installer(NODE, KEY, rig.socket(), SESSION))
        .enable_all()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://localhost/unused_cold_submit_test")
        .unwrap();
    let transcript = root.path().join("not-created.jsonl");
    let prompt = "reply _ literal\nattachment + voice";
    let attached = Attached {
        nonce: NONCE.into(),
        session_id: "cold-native".into(),
        transcript: transcript.clone(),
        target,
        bound: true,
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let registry = crate::services::turn_orchestrator::ChannelMailboxRegistry::default();
    let mailbox = registry.handle(poise::serenity_prelude::ChannelId::new(727398));
    runtime.block_on(mailbox.v2_cold_fixture(root.path().join("herdr-owner.json"),serde_json::json!({"nonce":"claude-herdr-scope","provider":"claude","target":{"host":"herdr","owner":logical,"root":"test","execution_nonce":NONCE},"source":{"kind":"pre_source","expected_session_id":null,"expected_transcript":null,"binding_watermark":null}}),prompt.into(),727398)).unwrap();
    let socket = rig.socket().display().to_string();
    let commit_path = crate::config::runtime_root()
        .unwrap()
        .join("runtime/discord_pending_queue/cold-matrix/727398-herdr-owner.json");
    let effect_mailbox = mailbox.clone();
    let (_, result) = runtime
        .block_on(
            mailbox.v2_cold_bootstrap(1, std::sync::Arc::new(|| 10000), move |frame| {
                assert!(
                    crate::services::tui_input::human_frame::history::v2::scoped_source().is_some(),
                    "actual Claude Herdr submit requires exact source scope"
                );
                assert!(
                    crate::services::claude_tui::owner_input::active(),
                    "actual Claude Herdr submit requires owner scope"
                );
                use crate::services::claude_tui::host_input::{HerdrInput, InputTransport};
                let mut preparation = HerdrInput::new(&attached.target);
                InputTransport::load_buffer(&mut preparation, "memory-only", "not submitted")
                    .unwrap();
                drop(preparation);
                let preparing = tokio::runtime::Handle::current()
                    .block_on(effect_mailbox.v2_request(|slot| slot.as_ref().unwrap().json()))
                    .unwrap();
                assert_eq!(
                    preparing["head_injection"]["payload_window"]["kind"], "bootstrap",
                    "memory buffer cannot start payload confirmation"
                );
                assert!(preparing["head_injection"]["confirmation_deadline_ms"].is_null());
                let turn = HerdrTurn {
                    pool: &pool,
                    owner,
                    channel_id: 1,
                    endpoint: HerdrLaunchEndpoint {
                        execution_node: NODE.into(),
                        config_key: KEY.into(),
                        socket_addr: socket,
                        herdr_session: SESSION.into(),
                    },
                    row: None,
                    prompt: &frame,
                    working_dir: "/tmp",
                    system_prompt: None,
                    model: None,
                    hook_endpoint: None,
                    cancel: None,
                };

                let _watch = refuse_commit.then(|| {
                    use crate::services::discord::runtime_store::durable_probe::{
                        Plan, Step, Watch,
                    };
                    Watch::new(
                        &commit_path,
                        Plan {
                            fail: vec![Step::ParentSync],
                            ..Default::default()
                        },
                    )
                });
                submit(&turn, attached, &tx)
            }),
        )
        .unwrap();
    let owner_snapshot = runtime
        .block_on(mailbox.v2_request(|s| s.as_ref().unwrap().json()))
        .unwrap();
    if refuse_commit {
        assert_eq!(
            rig.sends().len(),
            0,
            "refused owner commit permits no Herdr content or Enter"
        );
        assert!(
            result.is_err(),
            "payload commit refusal must reach the caller"
        );
        assert_eq!(
            owner_snapshot["head_injection"]["payload_window"]["kind"],
            "bootstrap"
        );
        assert_eq!(owner_snapshot["head_injection"]["original"]["text"], prompt);
        assert!(
            not_held(NONCE).is_err(),
            "refused first input preserves its provider hold"
        );
        return;
    }
    assert_eq!(
        owner_snapshot["head_injection"]["payload_window"]["kind"], "started",
        "actual submit must commit payload in owner scope"
    );
    let submitted = result
        .unwrap()
        .expect("physical Herdr submit returns its observer");
    assert_eq!(submitted.before, 0);
    assert!(!transcript.exists(), "Herdr submit cannot wait for Source");
    assert!(
        matches!(rx.try_recv(), Ok(StreamMessage::Init { .. })),
        "Init precedes input and observer"
    );
    let sends = rig.sends();
    assert_eq!(
        sends.len(),
        2,
        "Claude Herdr paste and Enter each occur once: {sends:?}"
    );
    let encoded = serde_json::to_string(&sends).unwrap();
    assert!(
        encoded.contains("attachment + voice") && encoded.contains("literal"),
        "Herdr keeps the prepared body"
    );
    assert!(
        not_held(NONCE).is_ok(),
        "applied Claude input releases its hold before observation"
    );
}
