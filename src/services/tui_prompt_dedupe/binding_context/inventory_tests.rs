use super::super::tests::fixture_after_shared_test_env_lock;
use super::*;

fn context(nonce: char) -> PreparedIncarnation {
    PreparedIncarnation::create(BindingContext {
        schema: 1,
        provider: "codex".into(),
        created_at: Utc::now(),
        execution_nonce: nonce.to_string().repeat(32),
        tmux_session: format!("inventory-{nonce}"),
        channel_id: Some(7308),
        owner_runtime_root: tc::current_tmux_owner_marker(),
        host: Some("test-host".into()),
        expected_native_session_id: None,
        launch_mode: "fresh".into(),
        provider_root: Some(PathBuf::from("/test/sessions")),
        first_prompt_digest: None,
        source_policy: Some("shadow".into()),
    })
    .unwrap()
}

#[test]
fn complete_context_inventory_requires_eof_at_the_exact_entry_budget() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_root, _env) = fixture_after_shared_test_env_lock();
    assert!(
        inventory("codex", 2).is_err(),
        "missing directory is unavailable"
    );
    let b = context('b');
    let a = context('a');
    let before = fs::read(&a.path).unwrap();
    assert_eq!(inventory("codex", 2).unwrap(), vec![a.context, b.context]);
    assert!(
        inventory("codex", 1).is_err(),
        "a truncated census is not Complete"
    );
    assert_eq!(
        fs::read(a.path).unwrap(),
        before,
        "observation is read-only"
    );
}

#[test]
fn context_inventory_refuses_partial_corrupt_and_mismatched_entries() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (_root, _env) = fixture_after_shared_test_env_lock();
    let a = context('a');
    let original = fs::read(&a.path).unwrap();
    for bytes in [
        b"{".to_vec(),
        {
            let mut wrong = a.context.clone();
            wrong.execution_nonce = "b".repeat(32);
            serde_json::to_vec(&wrong).unwrap()
        },
        {
            let mut wrong = a.context.clone();
            wrong.provider = "claude".into();
            serde_json::to_vec(&wrong).unwrap()
        },
        {
            let mut wrong = a.context.clone();
            wrong.schema = 2;
            serde_json::to_vec(&wrong).unwrap()
        },
    ] {
        fs::write(&a.path, bytes).unwrap();
        assert!(inventory("codex", 10).is_err());
    }
    fs::write(&a.path, &original).unwrap();
    let unfinished = a.path.parent().unwrap().join(".preparing.tmp");
    fs::write(&unfinished, &original).unwrap();
    assert!(
        inventory("codex", 10).is_err(),
        "prepared entries cannot disappear"
    );
    fs::remove_file(unfinished).unwrap();
    let alias = a
        .path
        .parent()
        .unwrap()
        .join(format!("{}.json", "c".repeat(32)));
    std::os::unix::fs::symlink(&a.path, &alias).unwrap();
    assert!(
        inventory("codex", 10).is_err(),
        "aliases are not immutable canonical contexts"
    );
}
