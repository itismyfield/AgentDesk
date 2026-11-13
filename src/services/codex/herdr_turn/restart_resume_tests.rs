use super::*;

#[tokio::test]
async fn ordinary_codex_herdr_launch_stays_fresh_with_existing_native_selector() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let pool = sqlx::postgres::PgPoolOptions::new()
        .connect_lazy("postgres://unused@127.0.0.1/unused")
        .unwrap();
    let owner = HostedOwner {
        provider: "codex".into(),
        discord_token_hash: "ordinary-token".into(),
        channel_id: "8128001".into(),
        logical_key: "ordinary-herdr-fresh".into(),
        owner_node: "test".into(),
        runtime_root: "/test".into(),
    };
    crate::services::tui_prompt_dedupe::register_provider_session(
        "codex",
        &owner.logical_key,
        "019e660d-4859-7522-9cee-8ba7c4e7c745",
    );
    let turn = CodexHerdrTurn {
        pool: &pool,
        owner,
        channel_id: 8128001,
        endpoint: HerdrLaunchEndpoint {
            execution_node: "test".into(),
            config_key: "test".into(),
            socket_addr: "/test/herdr.sock".into(),
            herdr_session: "test".into(),
        },
        row: None,
        prompt: "ordinary input",
        working_dir: "/tmp",
        system_prompt: None,
        allowed_tools: &[],
        model: None,
        fast_mode: None,
        goals: None,
        compact_token_limit: None,
        cancel: None,
    };
    let launch = launch_of(&turn, Some(Path::new("/test")));
    assert!(
        !launch.resume,
        "only explicit restart intent can request resume"
    );
    assert!(launch.expected_native_session_id.is_none());
}
