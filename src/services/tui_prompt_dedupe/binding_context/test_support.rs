use crate::config::TestEnvVarGuard as Guard;
use std::fs;

#[cfg(test)]
pub(crate) fn fixture() -> (tempfile::TempDir, [Guard; 2]) {
    let root = tempfile::tempdir().unwrap();
    let env = Guard::set_path("AGENTDESK_ROOT_DIR", root.path());
    with_config(root, env)
}

/// `fixture` for a caller that already holds the shared env lock, so it can
/// take that lock before `TEST_LOCK` (the env -> dedupe order).
pub(crate) fn fixture_after_shared_test_env_lock() -> (tempfile::TempDir, [Guard; 2]) {
    crate::config::test_env_lock::assert_shared_test_env_lock_held();
    let root = tempfile::tempdir().unwrap();
    let env = Guard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());
    with_config(root, env)
}

fn with_config(root: tempfile::TempDir, env: Guard) -> (tempfile::TempDir, [Guard; 2]) {
    let config = root.path().join("config.yaml");
    fs::write(&config, "server: {}").unwrap();
    let config_env = Guard::set_path_after_shared_test_env_lock("AGENTDESK_CONFIG", &config);
    (root, [config_env, env])
}
