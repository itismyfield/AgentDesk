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
    if tests::instance_child(
        concat!(
            module_path!(),
            "::instance_backend_refuses_foreign_files_before_create_or_spawn"
        ),
        "I2B_BACKEND_CHILD",
    ) {
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        initialize().unwrap();
        crate::TEST_PARSE_OUTCOME.with(|slot| {
            *slot.borrow_mut() = Some(crate::cli::args::ParseOutcome::RunServer);
        });
        assert!(crate::run_from_args().is_err());
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
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nprintf '%s' \"$PATH\" > '{}'\n{} > '{}'\n",
                root.join("backend-argv").display(),
                root.join("backend-path").display(),
                crate::services::provider_auth_profile::instance_env_tests::CAPTURE,
                capture.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let shell =
            crate::services::provider_auth_profile::instance_env_tests::synthetic_login_shell(root);
        let _shell =
            crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("SHELL", &shell);
        let _binary = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "AGENTDESK_CODEX_PATH",
            &wrapper,
        );
        let resolution = crate::services::platform::resolve_provider_binary("codex");
        let config = SessionConfig {
            agentdesk_exe: wrapper.to_string_lossy().into_owned(),
            output_path: root.join("backend-output").to_string_lossy().into_owned(),
            env_vars: vec![("PATH".into(), resolution.exec_path.unwrap())],
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
        assert_eq!(
            std::fs::read_to_string(root.join("backend-path")).unwrap(),
            std::env::var("PATH").unwrap()
        );
        let argv = std::fs::read_to_string(root.join("backend-argv")).unwrap();
        assert_eq!(argv.lines().next(), Some("tmux-wrapper"));
        for expected in [
            "--output-file",
            "--input-fifo",
            "--prompt-file",
            "--cwd",
            "--input-mode",
            "pipe",
        ] {
            assert!(argv.lines().any(|argument| argument == expected));
        }
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
    }
}

#[test]
fn instance_reentry_accepts_yaml_org_with_same_scope() {
    const CHILD: &str = "I3_ORG_YAML_CHILD";
    if std::env::var_os(CHILD).is_some() {
        initialize().unwrap();
        assert!(current().unwrap().is_some());
        return;
    }
    let (_temp, root) = tests::fixture();
    let path = root.join("config/org.yaml");
    let org = read_json(&path).unwrap();
    let mut child = tests::command(&root);
    std::fs::write(&path, serde_yaml::to_string(&org).unwrap()).unwrap();
    let result = child
        .args(["--exact", "config::test_instance::environment_tests::instance_reentry_accepts_yaml_org_with_same_scope", "--nocapture"])
        .env(CHILD, "1")
        .output()
        .unwrap();
    tests::assert_single_test_passed(
        &result,
        "config::test_instance::environment_tests::instance_reentry_accepts_yaml_org_with_same_scope",
    );
    let mut changed = org;
    changed["provider_auth_profiles"]["test-codex"]["home"] =
        serde_json::json!("/synthetic/foreign");
    std::fs::write(&path, serde_yaml::to_string(&changed).unwrap()).unwrap();
    assert!(
        load_instance_declaration(
            &root.join("manifest.json"),
            &root,
            &root.join("config/agentdesk.yaml")
        )
        .is_err()
    );
}

#[test]
fn instance_other_declarations_remain_json_only() {
    let (_temp, root) = tests::fixture();
    for name in ["manifest.json", "receipt.json", "config/agentdesk.yaml"] {
        let path = root.join(name);
        let bytes = std::fs::read(&path).unwrap();
        let value = read_json(&path).unwrap();
        std::fs::write(&path, serde_yaml::to_string(&value).unwrap()).unwrap();
        assert!(
            load_instance_declaration(
                &root.join("manifest.json"),
                &root,
                &root.join("config/agentdesk.yaml")
            )
            .is_err(),
            "{name}"
        );
        std::fs::write(path, bytes).unwrap();
    }
}

#[test]
fn unscoped_initialization_does_not_read_org() {
    const CHILD: &str = "I3_UNSCOPED_ORG_CHILD";
    if std::env::var_os(CHILD).is_some() {
        assert!(!requested());
        initialize().unwrap();
        assert!(current().unwrap().is_none());
        return;
    }
    let (_temp, root) = tests::fixture();
    let mut child = tests::command(&root);
    let path = root.join("config/org.yaml");
    std::fs::write(&path, "invalid: [").unwrap();
    let result = child
        .args([
            "--exact",
            "config::test_instance::environment_tests::unscoped_initialization_does_not_read_org",
            "--nocapture",
        ])
        .env(CHILD, "1")
        .env_remove(MARKER)
        .output()
        .unwrap();
    tests::assert_single_test_passed(
        &result,
        "config::test_instance::environment_tests::unscoped_initialization_does_not_read_org",
    );
    assert_eq!(std::fs::read_to_string(path).unwrap(), "invalid: [");
}

fn claude_login_fixture(quoted: bool) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (temp, previous) = tests::fixture();
    let home = temp.path().canonicalize().unwrap().join(if quoted {
        "login home 'active'"
    } else {
        "login-home"
    });
    let root = home
        .join(".adk/dev/instances")
        .join(previous.file_name().unwrap());
    std::fs::create_dir_all(root.parent().unwrap()).unwrap();
    std::fs::rename(&previous, &root).unwrap();
    for name in [
        "manifest.json",
        "receipt.json",
        "config/agentdesk.yaml",
        "config/org.yaml",
    ] {
        let path = root.join(name);
        let body = std::fs::read_to_string(&path)
            .unwrap()
            .replace(previous.to_str().unwrap(), root.to_str().unwrap());
        std::fs::write(path, body).unwrap();
    }
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    for binary in [
        home.join(".local/bin/claude"),
        root.join("fixture-worktree/target/debug/claude"),
    ] {
        std::fs::create_dir_all(binary.parent().unwrap()).unwrap();
        std::fs::write(&binary, "#!/bin/sh\nprintf '2.1.325\\n'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(binary, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }
    (temp, root, home)
}

fn bind_claude_login(root: &Path, home: &Path) {
    let login = serde_json::json!({"mode":"default_active","account_slot":3,
        "login_home":home,"config_dir":home.join(".claude"),"cli":home.join(".local/bin/claude")});
    for name in ["manifest.json", "receipt.json"] {
        let path = root.join(name);
        let mut value = read_json(&path).unwrap();
        value["claude_login"] = login.clone();
        std::fs::write(path, value.to_string()).unwrap();
    }
    let path = root.join("config/org.yaml");
    let mut org = read_json(&path).unwrap();
    org["provider_auth_profiles"]["test-claude"]["home"] = serde_json::json!(home.join(".claude"));
    std::fs::write(path, org.to_string()).unwrap();
}

#[test]
fn claude_login_contract_and_operating_golden() {
    const MODE: &str = "ADK_LOGIN_CONTRACT_CHILD";
    if let Ok(mode) = std::env::var(MODE) {
        let _guard = crate::config::test_env_lock::acquire_shared_test_env_lock();
        if mode != "operating" {
            initialize().unwrap();
        }
        let root = PathBuf::from(std::env::var("ADK_LOGIN_FIXTURE_ROOT").unwrap());
        let home = PathBuf::from(std::env::var("ADK_LOGIN_FIXTURE_HOME").unwrap());
        let enabled = mode == "opt-in";
        assert_eq!(environment::claude_login_enabled(), enabled);
        let (binary, _) = crate::services::claude_command::ClaudeBinary::resolve().unwrap();
        let launch = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
            tmux_session_name: "fixture".into(),
            working_dir: root.clone(),
            claude_bin: binary,
            agentdesk_exe: PathBuf::from("/fixture/agentdesk"),
            hook_endpoint: "http://127.0.0.1:18791".into(),
            session_id: "fixture-session".into(),
            system_prompt: None,
            model: None,
            resume: false,
        };
        let args = crate::services::claude_tui::session::build_claude_tui_args(
            &launch,
            Path::new("/fixture/hooks.json"),
        );
        let original = [
            "--dangerously-skip-permissions",
            "--session-id",
            "fixture-session",
            "--settings",
            "/fixture/hooks.json",
        ];
        assert_eq!(&args[..5], original);
        if enabled {
            assert_eq!(
                crate::services::claude_tui::hook_output_guard::configured_claude_projects_root(),
                Some(home.join(".claude/projects"))
            );
            assert!(args.iter().any(|s| s == "--strict-mcp-config"));
            assert!(
                args.windows(2)
                    .any(|p| p == ["--mcp-config", r#"{"mcpServers":{}}"#])
            );
            assert!(args.windows(2).any(|p| p == ["--setting-sources", ""]));
            let catalog = crate::services::provider_auth_profile::queries::provider_auth_catalog();
            assert_eq!(
                catalog.get("test-claude").and_then(|p| p.home.as_deref()),
                home.join(".claude").to_str()
            );
            let mut other = catalog["test-claude"].clone();
            other.home = Some(home.join(".claude").to_string_lossy().into_owned());
            assert!(
                crate::services::provider_auth_profile::validate_profile_def(
                    "another-profile",
                    &other
                )
                .is_err()
            );
            use crate::services::provider::ProviderKind;
            let overlay = crate::services::provider_auth_profile::resolve(
                ProviderKind::Claude,
                Some("test-claude"),
                None,
                &std::collections::HashMap::new(),
            )
            .unwrap();
            assert_eq!(overlay.home, Some(home.join(".claude")));
            assert_eq!(overlay.env["HOME"], home.to_string_lossy());
            let mut child = Command::new("/usr/bin/env");
            crate::services::provider_auth_profile::apply_overlay_to_command(&mut child, &overlay)
                .unwrap();
            let out = child.output().unwrap();
            let env = String::from_utf8(out.stdout).unwrap();
            assert!(env.lines().any(|s| s == format!("HOME={}", home.display())));
            assert!(env.lines().any(|s| s
                == format!(
                    "CODEX_HOME={}",
                    root.join("home/.adk/profiles/codex/test-codex").display()
                )));
            let shell =
                crate::services::provider_auth_profile::overlay_shell_env_lines(&overlay).unwrap();
            let out = Command::new("sh")
                .args(["-c", &format!("{shell}\nprintf '%s' \"$HOME\"")])
                .output()
                .unwrap();
            assert_eq!(
                String::from_utf8(out.stdout).unwrap(),
                home.to_string_lossy()
            );
            let mut bad = Command::new("/usr/bin/true");
            bad.env("AGENTDESK_API_URL", "http://127.0.0.1:8791");
            assert!(environment::configure_child_command(&mut bad).is_err());
            assert!(
                environment::validate_path(&home.join(".claude/projects/operating/session.jsonl"))
                    .is_err()
            );
            let own = crate::services::claude_tui::transcript_tail::encode_project_path(
                &root.join("workspaces/claude"),
            );
            assert!(
                environment::validate_path(
                    &home
                        .join(".claude/projects")
                        .join(own)
                        .join("00000000-0000-4000-8000-000000000001.jsonl")
                )
                .is_ok()
            );
        } else {
            assert_eq!(args, original);
        }
        use crate::services::claude_tui::hook_bundle::{
            HookBundleConfig, render_claude_hook_settings,
        };
        let mut config = HookBundleConfig {
            endpoint: if enabled {
                "http://127.0.0.1:18791"
            } else {
                "http://127.0.0.1:8791"
            }
            .into(),
            provider: "claude".into(),
            session_id: "fixture-session".into(),
            agentdesk_exe: "/fixture/agentdesk".into(),
        };
        let settings = render_claude_hook_settings(&config);
        let relay = settings["hooks"]["Stop"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        if enabled {
            assert!(relay.starts_with("/usr/bin/env "));
            assert!(relay.contains("AGENTDESK_API_URL=http://127.0.0.1:18791"));
            assert!(!relay.contains(":8791"));
            let fake = root.join("runtime/fake-relay");
            std::fs::write(
                &fake,
                "#!/bin/sh\nprintf '%s\\n' \"$HOME\" \"$AGENTDESK_API_URL\"\n",
            )
            .unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            let mut own = config.clone();
            own.agentdesk_exe = fake.to_string_lossy().into_owned();
            let callback = render_claude_hook_settings(&own);
            let output = Command::new("sh")
                .args([
                    "-c",
                    callback["hooks"]["Stop"][0]["hooks"][0]["command"]
                        .as_str()
                        .unwrap(),
                ])
                .env("HOME", &home)
                .env("AGENTDESK_API_URL", "http://127.0.0.1:8791")
                .output()
                .unwrap();
            assert!(output.status.success());
            assert_eq!(
                String::from_utf8(output.stdout).unwrap(),
                format!("{}\nhttp://127.0.0.1:18791\n", root.join("home").display())
            );
            config.endpoint = "http://127.0.0.1:8791".into();
            let refused = render_claude_hook_settings(&config);
            assert_eq!(
                refused["hooks"]["Stop"][0]["hooks"][0]["command"],
                "/usr/bin/false"
            );
            let sentinel = root.join("global-statusline-was-run");
            std::fs::write(home.join(".claude/settings.json"),serde_json::json!({"statusLine":{"type":"command","command":format!("touch {}",crate::services::process::shell_escape(&sentinel.to_string_lossy()))}}).to_string()).unwrap();
            use std::io::Write;
            use std::process::Stdio;
            let mut child = Command::new("sh")
                .args(["-c", settings["statusLine"]["command"].as_str().unwrap()])
                .env("HOME", &home)
                .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"{\"session_id\":\"fixture-session\"}")
                .unwrap();
            assert!(child.wait_with_output().unwrap().status.success());
            assert!(!sentinel.exists());
            assert!(settings["hooks"]["SessionStart"].is_array());
        } else {
            assert_eq!(
                relay,
                "/fixture/agentdesk claude-hook-relay --endpoint http://127.0.0.1:8791 --provider claude --event Stop --session-id fixture-session"
            );
            let original_dir = std::env::temp_dir().join("agentdesk-claude-rate-limits");
            let status = settings["statusLine"]["command"]
                .as_str()
                .unwrap()
                .replace(original_dir.to_str().unwrap(), "/golden/rate");
            use sha2::Digest;
            assert_eq!(
                format!("{:x}", sha2::Sha256::digest(status.as_bytes())),
                "37dfbd2b9a5084c9a6700d769336afe9d6a8b9074db5dcddaea80a22813b0931"
            );
        }
        return;
    }
    let selector = concat!(
        module_path!(),
        "::claude_login_contract_and_operating_golden"
    );
    let selector = selector.strip_prefix("agentdesk::").unwrap_or(selector);
    for mode in ["legacy", "opt-in", "operating"] {
        let (_temp, root, home) = claude_login_fixture(mode == "opt-in");
        if mode == "opt-in" {
            bind_claude_login(&root, &home);
        }
        let mut child = if mode == "operating" {
            let mut c = Command::new(std::env::current_exe().unwrap());
            c.env_clear()
                .env("HOME", &home)
                .env(
                    "PATH",
                    format!("{}:/usr/bin:/bin", home.join(".local/bin").display()),
                )
                .env("TMPDIR", root.join("tmp"));
            c
        } else {
            tests::command(&root)
        };
        let out = child
            .args(["--exact", selector, "--nocapture"])
            .env(MODE, mode)
            .env("ADK_LOGIN_FIXTURE_ROOT", &root)
            .env("ADK_LOGIN_FIXTURE_HOME", &home)
            .output()
            .unwrap();
        tests::assert_single_test_passed(&out, selector);
    }
}

#[test]
fn claude_login_rejects_slot_one_unknown_keys_and_receipt_drift() {
    let selector = concat!(
        module_path!(),
        "::claude_login_rejects_slot_one_unknown_keys_and_receipt_drift"
    );
    let selector = selector.strip_prefix("agentdesk::").unwrap_or(selector);
    if std::env::var_os("ADK_LOGIN_REJECT_CHILD").is_some() {
        initialize().unwrap();
        return;
    }
    for mutation in ["unknown", "slot", "receipt"] {
        let (_temp, root, home) = claude_login_fixture(false);
        bind_claude_login(&root, &home);
        if mutation == "slot" {
            let path = root.join("manifest.json");
            let mut m = read_json(&path).unwrap();
            m["claude_login"]["account_slot"] = serde_json::json!(1);
            let config = read_json(&root.join("config/agentdesk.yaml")).unwrap();
            assert!(environment::Environment::load(&root, &m, &config).is_err());
            continue;
        }
        let mut child = tests::command(&root);
        let path = root.join(if mutation == "receipt" {
            "receipt.json"
        } else {
            "manifest.json"
        });
        let mut value = read_json(&path).unwrap();
        if mutation == "unknown" {
            value["unapproved_login"] = serde_json::json!(true);
        } else {
            value["claude_login"]["account_slot"] = serde_json::json!(2);
        }
        std::fs::write(path, value.to_string()).unwrap();
        let out = child
            .args(["--exact", selector])
            .env("ADK_LOGIN_REJECT_CHILD", "1")
            .output()
            .unwrap();
        assert!(!out.status.success(), "accepted {mutation}");
    }
}
