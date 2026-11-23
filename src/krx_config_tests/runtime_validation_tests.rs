//! Runtime-validation tests that drive root boot and the KE audit loader, moved out of KRX config.

use crate::config::runtime_validation::tests::{REMOVED, ResetHosting, fixture, write_config};
use crate::services::provider::ProviderKind;
use crate::services::provider_hosting;

#[test]
fn final_boot_boundary_rejects_retired_before_installing_mirrors() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _lock = provider_hosting::TEST_CONFIG_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let _reset = ResetHosting;
    provider_hosting::install_provider_hosting_config(&fixture(Some("pipe"), false, "claude"));
    let before = provider_hosting::resolve_provider_session_selection(&ProviderKind::Claude);
    let config = fixture(Some("claude_e"), false, "claude");
    let error = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(crate::bootstrap::prepare_and_install_boot_snapshots(
            &config, None,
        ))
        .unwrap_err();
    assert!(format!("{error:#}").contains(REMOVED), "{error:#}");
    assert_eq!(
        provider_hosting::resolve_provider_session_selection(&ProviderKind::Claude),
        before
    );
}

#[test]
fn runtime_audit_load_preserves_removed_runtime_cause() {
    let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for legacy in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = if legacy {
            crate::runtime_layout::legacy_config_file_path(dir.path())
        } else {
            crate::runtime_layout::config_file_path(dir.path())
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        write_config(&path, &fixture(Some("claude-e"), false, "claude"));
        let error = match crate::services::discord_config_audit::load_runtime_config(dir.path()) {
            Err(error) => error,
            Ok(_) => panic!("runtime config load accepted a removed runtime"),
        };
        assert!(error.contains(REMOVED), "{error}");
    }
}
