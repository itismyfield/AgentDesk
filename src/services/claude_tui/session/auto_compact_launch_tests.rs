//! #5172 R1: the generated launch artifact is the oracle; no Claude/tmux starts.
use super::*;

const SETTING: &str = "context_compact_window_claude";
const WINDOW_ENV: &str = "CLAUDE_CODE_AUTO_COMPACT_WINDOW";

fn launch_config(root: &Path, model: Option<&str>) -> ClaudeTuiLaunchConfig {
    ClaudeTuiLaunchConfig {
        tmux_session_name: format!("AgentDesk-claude-5172-{}", uuid::Uuid::new_v4()),
        working_dir: root.to_path_buf(),
        claude_bin: ClaudeBinary::from_tmux_wrapper_argv("/fixture/claude"),
        agentdesk_exe: PathBuf::from("/fixture/agentdesk"),
        hook_endpoint: "http://127.0.0.1:1".to_string(),
        session_id: "01234567-89ab-cdef-0123-456789abcdef".to_string(),
        system_prompt: None,
        model: model.map(str::to_string),
        resume: false,
    }
}

fn write_provider_setting(root: &Path, value: Option<u64>) -> PathBuf {
    let path = root.join("agentdesk.yaml");
    let mut config = serde_json::json!({
        "server": {},
        "data": { "dir": root.join("data") }
    });
    if let Some(value) = value {
        config["runtime"] = serde_json::json!({ (SETTING): value });
    }
    fs::write(&path, serde_yaml::to_string(&config).unwrap()).unwrap();
    serde_yaml::from_str::<crate::config::Config>(&fs::read_to_string(&path).unwrap())
        .expect("the exact provider fixture must parse as Config before checking launch output");
    path
}

fn generated_script(root: &Path, name: &str, model: Option<&str>) -> String {
    let script_path = root.join(name);
    write_launch_script(
        &script_path,
        &launch_config(root, model),
        &root.join("hook-settings.json"),
    )
    .expect("the production script writer must finish before checking its output");
    fs::read_to_string(script_path).unwrap()
}

fn assert_window(script: &str, expected: u64) {
    let unset = format!("unset {WINDOW_ENV}\n");
    let export = format!("export {WINDOW_ENV}={expected}\n");
    let export_prefix = format!("export {WINDOW_ENV}=");
    assert_eq!(
        script.matches(unset.as_str()).count(),
        1,
        "scrub inherited window once"
    );
    assert_eq!(
        script.matches(export.as_str()).count(),
        1,
        "#5172 R1 requires the absolute window in the actual launch script: {script}"
    );
    assert_eq!(script.matches(export_prefix.as_str()).count(), 1);
    assert!(script.find(unset.as_str()).unwrap() < script.find(export.as_str()).unwrap());
    assert!(script.find(export.as_str()).unwrap() < script.find("exec ").unwrap());
}

#[test]
fn default_absolute_window_is_exported_without_a_model() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _context = crate::services::claude_compact_context::state_test_guard();
    let root = tempfile::tempdir().unwrap();
    let path = write_provider_setting(root.path(), None);
    let _config_path = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &path,
    );
    let script = generated_script(root.path(), "default.sh", None);
    assert!(!script.contains("'--model'"));
    assert_window(&script, 700_000);
}

#[test]
fn absolute_window_is_identical_for_model_free_sonnet_and_opus_launches() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _context = crate::services::claude_compact_context::state_test_guard();
    let root = tempfile::tempdir().unwrap();
    let path = write_provider_setting(root.path(), None);
    let _config_path = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &path,
    );
    for model in [None, Some("sonnet"), Some("opus"), Some("sonnet[1m]")] {
        let script = generated_script(root.path(), "model.sh", model);
        assert_window(&script, 700_000);
    }
}

#[test]
fn explicit_absolute_window_override_and_inclusive_bounds_reach_the_script() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _context = crate::services::claude_compact_context::state_test_guard();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("agentdesk.yaml");
    let _config_path = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &path,
    );
    for window in [100_000, 350_000, 700_000, 1_000_000] {
        write_provider_setting(root.path(), Some(window));
        let script = generated_script(root.path(), "override.sh", None);
        assert_window(&script, window);
    }
}

#[test]
fn new_launch_override_does_not_rewrite_an_existing_launch_artifact() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _context = crate::services::claude_compact_context::state_test_guard();
    let root = tempfile::tempdir().unwrap();
    let path = write_provider_setting(root.path(), None);
    let _config_path = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &path,
    );
    let first = generated_script(root.path(), "existing.sh", None);
    write_provider_setting(root.path(), Some(350_000));
    let next = generated_script(root.path(), "new.sh", Some("opus"));
    assert_window(&first, 700_000);
    assert_window(&next, 350_000);
    assert_eq!(
        fs::read_to_string(root.path().join("existing.sh")).unwrap(),
        first
    );
}

#[test]
fn runtime_config_retains_the_explicit_absolute_window_without_changing_empty_semantics() {
    let empty: crate::config::RuntimeSettingsConfig = serde_yaml::from_str("{}").unwrap();
    assert!(
        empty.is_empty(),
        "adding an optional override must preserve empty config"
    );
    for window in [100_000, 700_000, 1_000_000] {
        let config: crate::config::RuntimeSettingsConfig =
            serde_yaml::from_str(&format!("{SETTING}: {window}\n")).unwrap();
        assert!(
            !config.is_empty(),
            "an explicit window must survive runtime serialization"
        );
        let value = serde_json::to_value(config).unwrap();
        assert_eq!(value.get(SETTING), Some(&serde_json::json!(window)));
    }
}

#[test]
fn configured_window_is_clamped_only_when_generating_a_launch() {
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let _context = crate::services::claude_compact_context::state_test_guard();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("agentdesk.yaml");
    let _config_path = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "AGENTDESK_CONFIG",
        &path,
    );
    for (raw, expected) in [
        (0, 100_000),
        (99_999, 100_000),
        (1_000_001, 1_000_000),
        (u64::MAX, 1_000_000),
    ] {
        write_provider_setting(root.path(), Some(raw));
        let script = generated_script(root.path(), "clamped.sh", None);
        assert_window(&script, expected);
        let persisted: crate::config::RuntimeSettingsConfig =
            serde_yaml::from_str(&format!("{SETTING}: {raw}\n")).unwrap();
        assert_eq!(
            serde_json::to_value(persisted).unwrap().get(SETTING),
            Some(&serde_json::json!(raw))
        );
    }
}
