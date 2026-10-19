use super::*;

#[test]
fn intake_post_and_edit_expose_processing_not_standalone_dots() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let root = tempfile::tempdir().unwrap();
    let _root = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_ROOT_DIR",
        root.path(),
    );
    let (_, calls) = placeholder_tests::runtime().block_on(placeholder_tests::exercise(false));
    assert_eq!(calls.len(), 2);
    assert_eq!((&*calls[0].0, &*calls[1].0), ("POST", "PATCH"));
    for (_, _, body) in calls {
        let body: serde_json::Value = serde_json::from_str(&body).unwrap();
        let content = body["content"].as_str().unwrap();
        assert_eq!(content, "⠋ 계속 처리 중");
        assert!(
            crate::services::discord::placeholder_sweeper::is_message_still_placeholder(content)
        );
        assert!(!matches!(content.trim(), "..." | "…"));
    }
}
