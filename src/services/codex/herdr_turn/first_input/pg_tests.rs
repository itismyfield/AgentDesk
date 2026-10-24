use super::*;
use crate::db::dispatched_sessions::hosted_execution::{HostedLocation, SourceRef};
use crate::services::session_host::herdr_socket_rig_tests::{HerdrRig, KEY, NODE, PANE, SESSION};
use serde_json::json;

#[test]
fn cold_codex_herdr_submit_keeps_hold_and_returns_before_session_start_pg() {
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::set_agentdesk_root_for_test(root.path());
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let _entered = rt.enter();
    let db = rt.block_on(crate::db::auto_queue::test_support::TestPostgresDb::create());
    let pool = rt.block_on(db.connect_and_migrate());
    let rig = HerdrRig::start();
    let _registry = rig.registry_on_this_thread();
    let nonce = "11223344556677889900aabbccddeeff";
    let context = rig.context(nonce);
    let mut prepared: crate::services::tui_prompt_dedupe::binding_context::BindingContext =
        serde_json::from_slice(&std::fs::read(&context).unwrap()).unwrap();
    prepared.provider = "codex".into();
    let context = PreparedIncarnation::create(prepared).unwrap().path;
    rig.run_provider(&context, false);
    let owner = HostedOwner {
        provider: "codex".into(),
        discord_token_hash: "cold-token".into(),
        channel_id: "7272392".into(),
        logical_key: "cold-codex-herdr-ticket".into(),
        owner_node: NODE.into(),
        runtime_root: "test".into(),
    };
    let mut expected = rig.expected(nonce);
    expected.binding_provider = "codex".into();
    let record = HostedExecution {
        schema: 1,
        state: HostedState::Pending,
        execution_nonce: nonce.into(),
        owner: owner.clone(),
        location: Some(HostedLocation {
            host: "herdr".into(),
            execution_node: NODE.into(),
            endpoint_config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            named_session: SESSION.into(),
            pane_id: PANE.into(),
        }),
        expected: Some(expected),
        source_ref: SourceRef {
            runtime_root: "test".into(),
            channel: "7272392".into(),
            provider: "codex".into(),
            logical_key: owner.logical_key.clone(),
            execution_nonce: nonce.into(),
            initial_source: None,
            baseline_event_seq: None,
        },
    };
    rt.block_on(sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id, hosted_execution) VALUES ($1, 'codex', 'idle', 'discord_channel', $2, $3, $4)")
        .bind(format!("codex/cold-token/{NODE}:{}", owner.logical_key)).bind("cold-token").bind("7272392").bind(serde_json::to_value(&record).unwrap()).execute(&pool)).unwrap();
    rig.answer("pane.read", json!({"type":"pane_read","read":{"pane_id":PANE,"source":"recent_unwrapped",
        "text":"earlier output\n\n›\n\n  gpt-5.5 xhigh · /fixture/workspace","revision":1,"truncated":false}}));
    let turn = CodexHerdrTurn {
        pool: &pool,
        owner,
        channel_id: 7272392,
        endpoint: HerdrLaunchEndpoint {
            execution_node: NODE.into(),
            config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            herdr_session: SESSION.into(),
        },
        row: None,
        prompt: "raw\nattachment + voice _ literal",
        working_dir: "/tmp",
        system_prompt: None,
        allowed_tools: &[],
        model: None,
        fast_mode: None,
        goals: None,
        compact_token_limit: None,
        cancel: None,
    };
    std::fs::write(
        crate::services::tmux_common::session_temp_path(&turn.owner.logical_key, "host_kind"),
        "herdr",
    )
    .unwrap();
    let submitted = submit(&turn, rt.handle(), nonce)
        .unwrap()
        .expect("Codex physical submit returns its observer");
    assert!(
        logged_source(&turn, nonce).is_none(),
        "cold Codex submit must not await its own Source"
    );
    assert!(
        not_held(nonce).is_err(),
        "Codex hold stays until observer binds its Source"
    );
    assert_eq!(
        rig.sends().len(),
        2,
        "Codex Herdr sends its one paste and Enter: {:?}",
        rig.sends()
    );
    assert!(
        !matches!(submitted.logged.has_changed(), Ok(true)),
        "no invented SessionStart wakes the observer"
    );
    assert!(
        serde_json::to_string(&rig.sends())
            .unwrap()
            .contains("attachment + voice"),
        "Codex Herdr preserves prepared bytes"
    );
}
