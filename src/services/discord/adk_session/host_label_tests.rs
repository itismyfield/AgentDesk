use super::*;

#[test]
fn saved_host_case_keeps_session_key() {
    let name =
        "services::discord::adk_session::host_label_tests::saved_host_case_keeps_session_key";
    if std::env::var_os("ADK_HOST_LABEL_CHILD").is_none() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("runtime")).unwrap();
        std::fs::write(
            root.path().join("runtime/session_host_label"),
            "MacBook-Pro-14\n",
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env("ADK_HOST_LABEL_CHILD", "1")
            .env("AGENTDESK_ROOT_DIR", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        return;
    }
    crate::services::platform::shell::with_hostname_for_test(
        "macbook-pro-14.tail9b57d2.ts.net",
        || {
            let path = crate::config::runtime_root()
                .unwrap()
                .join("runtime/session_host_label");
            crate::services::platform::with_session_host_label_path_for_test(&path, || {
                assert_eq!(
                    build_namespaced_session_key(
                        "fixture",
                        &ProviderKind::Claude,
                        "fixture-session"
                    ),
                    "claude/fixture/MacBook-Pro-14:fixture-session"
                );
                assert_eq!(
                    build_legacy_session_key("fixture-session"),
                    "MacBook-Pro-14:fixture-session"
                );
            });
        },
    );
}

#[tokio::test(flavor = "current_thread")]
async fn saved_host_case_resumes_original_selector_and_ignores_foreign_hosts_pg() {
    const NAME: &str = "services::discord::adk_session::host_label_tests::saved_host_case_resumes_original_selector_and_ignores_foreign_hosts_pg";
    if std::env::var_os("ADK_HOST_LABEL_CHILD").is_none() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("runtime")).unwrap();
        std::fs::write(
            root.path().join("runtime/session_host_label"),
            "MacBook-Pro-14\n",
        )
        .unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", NAME, "--nocapture"])
            .env("ADK_HOST_LABEL_CHILD", "1")
            .env("AGENTDESK_ROOT_DIR", root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
        return;
    }
    let _root = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let db = crate::db::auto_queue::test_support::TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    for (host, instance, name, selector) in [
        (
            "MacBook-Pro-14",
            "mac-book-release",
            "fixture-local",
            "original-selector",
        ),
        (
            "Mac-mini",
            "mac-mini-release",
            "fixture-foreign",
            "mini-selector",
        ),
        (
            "win-desktop",
            "wsl-release",
            "fixture-foreign",
            "wsl-selector",
        ),
    ] {
        sqlx::query("INSERT INTO sessions(session_key, provider, status, instance_id, claude_session_id) VALUES ($1, 'claude', 'idle', $2, $3)")
            .bind(format!("claude/fixture/{host}:{name}")).bind(instance).bind(selector)
            .execute(&pool).await.unwrap();
    }
    let keys = crate::services::platform::shell::with_hostname_for_test(
        "macbook-pro-14.tail9b57d2.ts.net",
        || {
            let path = crate::config::runtime_root()
                .unwrap()
                .join("runtime/session_host_label");
            crate::services::platform::with_session_host_label_path_for_test(&path, || {
                ["fixture-local", "fixture-foreign"].map(|name| {
                    build_namespaced_session_key("fixture", &ProviderKind::Claude, name)
                })
            })
        },
    );
    let local = crate::db::dispatched_sessions::load_provider_session_ids_pg(
        &pool,
        &keys[0],
        Some("claude"),
    )
    .await
    .unwrap()
    .expect("the old primary must be found before any status hook or alias write");
    assert_eq!(
        local.resolved_session_key,
        "claude/fixture/MacBook-Pro-14:fixture-local"
    );
    assert_eq!(
        crate::services::dispatched_sessions::selected_provider_resume_selector_for_provider(
            Some("claude"),
            &local
        ),
        Some("original-selector")
    );
    assert!(
        crate::db::dispatched_sessions::load_provider_session_ids_pg(
            &pool,
            &keys[1],
            Some("claude")
        )
        .await
        .unwrap()
        .is_none(),
        "mini and WSL selectors must not be resumed by a local key"
    );
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM sessions WHERE instance_id IN ('mac-mini-release', 'wsl-release') AND claude_session_id IS NOT NULL")
        .fetch_one(&pool).await.unwrap(), 2);
    pool.close().await;
    db.drop().await;
}
