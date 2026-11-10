use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn i8069_fresh_codex_launch_removes_inherited_runtime_before_create_pg() {
    let root = crate::config::TestRuntimeRootGuard::new();
    let _namespace = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "TMUX_TMPDIR",
        &root.path().canonicalize().unwrap(),
    );
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let mut launch = launch(Some(endpoint()));
    launch.owner.provider = "codex".into();
    launch.owner.logical_key = "AgentDesk-codex-fresh-marker-fixture".into();
    let owner = &launch.owner;
    let key = format!(
        "codex/{}/test-node:{}",
        owner.discord_token_hash, owner.logical_key
    );
    sqlx::query("INSERT INTO sessions (session_key, provider, status, identity_kind, discord_token_hash, channel_id) VALUES ($1, 'codex', 'idle', 'discord_channel', $2, $3)")
        .bind(&key)
        .bind(&owner.discord_token_hash)
        .bind(&owner.channel_id)
        .execute(&pool).await.unwrap();
    let runtime = std::path::PathBuf::from(crate::services::tmux_common::session_temp_path(
        &owner.logical_key,
        crate::services::tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT,
    ));
    std::fs::write(&runtime, "codex_tui").unwrap();
    let mut host = FakeHost::created("fresh-codex-pane");
    let at_create = runtime.clone();
    host.on_create = Box::new(move || {
        assert!(
            !at_create.exists(),
            "provider create inherited the old TUI runtime hint"
        );
    });
    let host = Arc::new(host);
    let result = launch_herdr_session(&pool, launch, command, host.clone()).await;
    assert!(
        matches!(result, Ok(HerdrLaunchOutcome::Launched { .. })),
        "{result:?}"
    );
    assert_eq!(host.creates.load(Ordering::SeqCst), 1);
    assert!(!runtime.exists());
    pool.close().await;
    db.drop().await;
}
