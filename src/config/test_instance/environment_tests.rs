use super::*;

#[test]
fn instance_startup_rejects_home_api_label_memento_and_old_declarations() {
    if std::env::var_os("I2B_STARTUP_CHILD").is_some() {
        initialize().unwrap();
        return;
    }
    let (_temp, root) = tests::fixture();
    let name = "config::test_instance::environment_tests::instance_startup_rejects_home_api_label_memento_and_old_declarations";
    let run = |changes: &[(&str, &str)]| {
        let mut child = tests::command(&root);
        child
            .args(["--exact", name, "--nocapture"])
            .env("I2B_STARTUP_CHILD", "1");
        for (key, value) in changes {
            child.env(key, value);
        }
        child.output().unwrap()
    };
    assert!(run(&[]).status.success());
    for (key, value) in [
        ("HOME", "/synthetic/foreign-home"),
        ("TMPDIR", "/synthetic/foreign-tmp"),
        ("XDG_CONFIG_HOME", "/synthetic/foreign-xdg"),
        ("CLAUDE_CONFIG_DIR", "/synthetic/foreign-claude"),
        ("CODEX_HOME", "/synthetic/foreign-codex"),
        ("AGENTDESK_API_URL", "http://127.0.0.1:8791"),
        ("ADK_API_URL", "http://127.0.0.1:18792"),
        ("AGENTDESK_DCSERVER_LABEL", "com.agentdesk.release"),
        ("AGENTDESK_RELEASE_TMUX_SESSION", "AgentDesk-release"),
        ("MEMENTO_ACCESS_KEY", "synthetic-key-never-print"),
    ] {
        assert!(!run(&[(key, value)]).status.success(), "accepted {key}");
        let mut missing = tests::command(&root);
        let result = missing
            .args(["--exact", name])
            .env("I2B_STARTUP_CHILD", "1")
            .env_remove(key)
            .output()
            .unwrap();
        if key != "MEMENTO_ACCESS_KEY" {
            assert!(!result.status.success(), "missing {key}");
        }
    }
    for relative in ["manifest.json", "config/agentdesk.yaml", "config/org.yaml"] {
        let path = root.join(relative);
        let bytes = std::fs::read(&path).unwrap();
        let mut child = tests::command(&root);
        let mut value = read_json(&path).unwrap();
        match relative {
            "manifest.json" => value["api"]["port"] = serde_json::json!(8791),
            "config/agentdesk.yaml" => {
                value["mcp_servers"]["memento"] =
                    serde_json::json!({"url":"http://127.0.0.1:57332/mcp"})
            }
            _ => value["provider_auth_primary_profiles"]["codex"] = serde_json::json!("default"),
        }
        std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        // Preserve the valid launch environment while mutating only the declaration.
        let result = child
            .args(["--exact", name])
            .env("I2B_STARTUP_CHILD", "1")
            .output()
            .unwrap();
        assert!(!result.status.success(), "accepted {relative}");
        std::fs::write(&path, bytes).unwrap();
    }
}

#[test]
fn instance_backend_refuses_foreign_files_before_create_or_spawn() {
    if std::env::var_os("I2B_BACKEND_CHILD").is_some() {
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        initialize().unwrap();
        assert!(crate::bootstrap::initialize().is_err());
        use crate::services::session_backend::{ProcessBackend, SessionBackend, SessionConfig};
        let root = &current().unwrap().unwrap().root;
        let foreign = root.parent().unwrap().join("foreign-output");
        let config = SessionConfig {
            session_name: format!("{}-test", current().unwrap().unwrap().tmux.prefix),
            working_dir: root.to_string_lossy().into_owned(),
            agentdesk_exe: "/synthetic/never-spawn".into(),
            output_path: foreign.to_string_lossy().into_owned(),
            prompt_path: root.join("prompt").to_string_lossy().into_owned(),
            wrapper_subcommand: "tmux-wrapper".into(),
            wrapper_args: vec![],
            env_vars: vec![],
            unset_env: vec![],
        };
        assert!(ProcessBackend::new().create_session(&config).is_err());
        assert!(!foreign.exists());
        let capture = root.join("backend-env");
        let wrapper = root.join("fake-wrapper");
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(
            &wrapper,
            format!(
                "#!/bin/sh\n{} > '{}'\n",
                crate::services::provider_auth_profile::instance_env_tests::CAPTURE,
                capture.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let config = SessionConfig {
            agentdesk_exe: wrapper.to_string_lossy().into_owned(),
            output_path: root.join("backend-output").to_string_lossy().into_owned(),
            ..config
        };
        let handle = ProcessBackend::new().create_session(&config).unwrap();
        if let crate::services::session_backend::SessionHandle::Process { child, .. } = handle {
            assert!(
                child
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .wait()
                    .unwrap()
                    .success()
            );
        } else {
            panic!("expected a real child");
        }
        crate::services::provider_auth_profile::instance_env_tests::assert_environment_capture(
            &std::fs::read(&capture).unwrap(),
            root,
            "tmux-wrapper",
        );
        std::fs::remove_file(&capture).unwrap();
        for (key, value) in [
            ("HOME", "/synthetic/foreign-home"),
            ("MEMENTO_ACCESS_KEY", "synthetic-secret"),
            ("OPENAI_API_KEY", "synthetic-secret"),
            ("BASH_ENV", "/synthetic/foreign-env"),
            ("XDG_DATA_HOME", "/synthetic/foreign-data"),
        ] {
            assert!(
                ProcessBackend::new()
                    .create_session_with_command_env(&config, |command| {
                        command.env(key, value);
                    })
                    .is_err()
            );
            assert!(!capture.exists());
        }
        let _home = crate::config::TestEnvVarGuard::set_value_after_shared_test_env_lock(
            "HOME",
            std::ffi::OsStr::new("/synthetic/foreign-home"),
        );
        assert!(configure_tmux_command(&mut Command::new("/bin/false")).is_err());
        return;
    }
    let (_temp, root) = tests::fixture();
    let output = tests::command(&root).args(["--exact", "config::test_instance::environment_tests::instance_backend_refuses_foreign_files_before_create_or_spawn", "--nocapture"]).env("I2B_BACKEND_CHILD", "1").output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
