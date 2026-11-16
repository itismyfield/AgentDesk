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
    let home = temp.path().canonicalize().unwrap();
    let home = if quoted {
        home.join("login home 'active'")
    } else {
        home
    };
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
    std::fs::write(home.join("active-slot"), "3").unwrap();
    let cswap = home.join(".local/bin/cswap");
    std::fs::write(&cswap, format!("#!/bin/sh\nvalue=$(/bin/cat {})\n[ \"$value\" != unavailable ] || exit 1\nprintf '{{\"activeAccountNumber\":%s}}\\n' \"$value\"\n", crate::services::process::shell_escape(&home.join("active-slot").to_string_lossy()))).unwrap();
    let metadata = home.join("metadata-probe");
    std::fs::write(&metadata, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    for path in [cswap, metadata] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    (temp, root, home)
}

fn bind_claude_login(root: &Path, home: &Path) {
    let login = serde_json::json!({"mode":"default_active","account_slot":3,
        "login_home":home,"config_dir":root.join("home/.adk/profiles/claude/test-claude"),"cli":home.join(".local/bin/claude")});
    for name in ["manifest.json", "receipt.json"] {
        let path = root.join(name);
        let mut value = read_json(&path).unwrap();
        value["claude_login"] = login.clone();
        std::fs::write(path, value.to_string()).unwrap();
    }
    let path = root.join("config/org.yaml");
    let mut org = read_json(&path).unwrap();
    org["provider_auth_profiles"]["test-claude"]["home"] =
        serde_json::json!(root.join("home/.adk/profiles/claude/test-claude"));
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
        if enabled {
            environment::set_metadata_program_for_tests(home.join("metadata-probe"));
        }
        assert_eq!(environment::claude_login_enabled(), enabled);
        let (binary, resolution) =
            crate::services::claude_command::ClaudeBinary::resolve().unwrap();
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
            assert_eq!(resolution.source.as_deref(), Some("env_override"));
            let expected_path = format!(
                "{}/fixture-worktree/target/debug:/usr/bin:/bin",
                root.display()
            );
            #[cfg(target_os = "macos")]
            let expected_path = format!(
                "{expected_path}:{}",
                ["/opt/homebrew/opt/tmux/bin", "/usr/local/opt/tmux/bin"]
                    .into_iter()
                    .map(Path::new)
                    .find(|p| p.exists())
                    .unwrap()
                    .canonicalize()
                    .unwrap()
                    .display()
            );
            assert_eq!(std::env::var("PATH").unwrap(), expected_path);
            assert_eq!(
                std::env::var("AGENTDESK_CLAUDE_PATH").unwrap(),
                home.join(".local/bin/claude").to_string_lossy()
            );
            assert_eq!(
                crate::services::claude_tui::hook_output_guard::configured_claude_projects_root(),
                Some(root.join("home/.adk/profiles/claude/test-claude/projects"))
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
                root.join("home/.adk/profiles/claude/test-claude").to_str()
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
            assert_eq!(
                overlay.home,
                Some(root.join("home/.adk/profiles/claude/test-claude"))
            );
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
                .is_err()
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

pub(super) fn review_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (temp, root, home) = claude_login_fixture(true);
    bind_claude_login(&root, &home);
    let sentinel = root.join("runtime/provider-started");
    let body = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then printf '2.1.325\\n'; exit 0; fi\ntouch {}\nprintf 'fixture-ai\\n'\n",
        crate::services::process::shell_escape(&sentinel.to_string_lossy())
    );
    std::fs::write(home.join(".local/bin/claude"), body).unwrap();
    let slot = home.join("active-slot");
    std::fs::write(&slot, "3").unwrap();
    let cswap = home.join(".local/bin/cswap");
    std::fs::write(&cswap, format!("#!/bin/sh\nvalue=$(/bin/cat {})\n[ \"$value\" != unavailable ] || exit 1\nprintf '{{\"activeAccountNumber\":%s}}\\n' \"$value\"\n",crate::services::process::shell_escape(&slot.to_string_lossy()))).unwrap();
    let meta = home.join("metadata-probe");
    std::fs::write(&meta, "#!/bin/sh\nexit 0\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    for p in [cswap, meta] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    (temp, root, home)
}

#[test]
fn review_simple_onboarding_uses_template_in_opt_in() {
    const CASE: &str = "ADK_REVIEW_SIMPLE_CHILD";
    if let Ok(mode) = std::env::var(CASE) {
        if mode != "operating" {
            initialize().unwrap();
        }
        let root = PathBuf::from(std::env::var("ADK_LOGIN_FIXTURE_ROOT").unwrap());
        environment::set_metadata_program_for_tests(
            root.ancestors().nth(4).unwrap().join("metadata-probe"),
        );
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (_, axum::Json(value)) = rt.block_on(crate::services::onboarding::generate_prompt(
            crate::services::onboarding::GeneratePromptBody {
                name: "fixture".into(),
                description: "fixture".into(),
                provider: Some("claude".into()),
            },
        ));
        assert_eq!(
            value["source"],
            if mode == "opt-in" { "template" } else { "ai" }
        );
        assert_eq!(
            root.join("runtime/provider-started").exists(),
            mode != "opt-in"
        );
        return;
    }
    for mode in ["operating", "legacy", "opt-in"] {
        let (_temp, root, home) = review_fixture();
        if mode != "opt-in" {
            for name in ["manifest.json", "receipt.json"] {
                let path = root.join(name);
                let mut value = read_json(&path).unwrap();
                value.as_object_mut().unwrap().remove("claude_login");
                std::fs::write(path, value.to_string()).unwrap();
            }
            let path = root.join("config/org.yaml");
            let mut value = read_json(&path).unwrap();
            value["provider_auth_profiles"]["test-claude"]["home"] =
                serde_json::json!(root.join("home/.adk/profiles/claude/test-claude"));
            std::fs::write(path, value.to_string()).unwrap();
            std::fs::copy(
                home.join(".local/bin/claude"),
                root.join("fixture-worktree/target/debug/claude"),
            )
            .unwrap();
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
        let selector = "config::test_instance::environment_tests::review_simple_onboarding_uses_template_in_opt_in";
        let out = child
            .args(["--exact", selector, "--nocapture"])
            .env(CASE, mode)
            .env("ADK_LOGIN_FIXTURE_ROOT", &root)
            .output()
            .unwrap();
        tests::assert_single_test_passed(&out, selector);
    }
}

#[test]
fn review_codex_override_is_never_executed_in_opt_in() {
    const CASE: &str = "ADK_REVIEW_CODEX_CHILD";
    if std::env::var_os(CASE).is_some() {
        initialize().unwrap();
        let root = current().unwrap().unwrap().root.clone();
        environment::set_metadata_program_for_tests(
            root.ancestors().nth(4).unwrap().join("metadata-probe"),
        );
        let fake = std::env::var("AGENTDESK_CODEX_PATH").unwrap();
        let registry = serde_json::json!({"schema_version":1,"updated_at":"2026-11-16T00:00:00Z","providers":{"codex":{"current":{"path":fake,"canonical_path":fake,"version":"1.2.3","source":"env_override","checked_at":"2026-11-16T00:00:00Z"}}}});
        let registry: crate::services::provider_cli::registry::ProviderCliRegistry =
            serde_json::from_value(registry).unwrap();
        crate::services::provider_cli::io::save_registry(&root, &registry).unwrap();
        let probe = crate::services::platform::binary_resolver::with_provider_execution_context(
            crate::services::provider_cli::ProviderExecutionContext {
                provider: "codex".into(),
                ..Default::default()
            },
            || crate::services::platform::probe_provider_binary_version("codex"),
        );
        assert!(probe.resolution.resolved_path.is_none());
        let injected = crate::services::platform::BinaryResolution {
            requested_binary: "codex".into(),
            resolved_path: Some(fake.clone()),
            canonical_path: Some(fake.clone()),
            source: Some("env_override".into()),
            attempts: vec![],
            failure_kind: None,
            exec_path: None,
        };
        assert_eq!(
            crate::services::platform::binary_resolver::probe_resolved_binary_version(
                &fake, &injected
            )
            .1
            .as_deref(),
            Some("test_instance_provider_disabled")
        );
        assert!(
            crate::services::codex::execute_command_simple_cancellable("fixture", None).is_err()
        );
        let evidence = crate::services::cluster::readiness::collect_for_login_test();
        assert!(!evidence.providers["codex"].cli_installed);
        assert!(!evidence.providers["codex"].cli_usable);
        assert!(evidence.providers["claude"].cli_usable);
        assert_eq!(
            evidence.providers["claude"].credential_profiles["test-claude"],
            true
        );
        assert!(!root.join("runtime/codex-executed").exists());
        return;
    }
    let (_temp, root, home) = review_fixture();
    let fake = home.join(".local/bin/codex");
    std::fs::write(
        &fake,
        format!(
            "#!/bin/sh\ntouch {}\nprintf '1.2.3\\n'\n",
            crate::services::process::shell_escape(
                &root.join("runtime/codex-executed").to_string_lossy()
            )
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o700)).unwrap();
    let selector = "config::test_instance::environment_tests::review_codex_override_is_never_executed_in_opt_in";
    let out = tests::command(&root)
        .args(["--exact", selector, "--nocapture"])
        .env(CASE, "1")
        .env("AGENTDESK_CODEX_PATH", fake)
        .output()
        .unwrap();
    tests::assert_single_test_passed(&out, selector);
}

#[test]
fn review_tui_script_rechecks_slot_at_exec() {
    const CASE: &str = "ADK_REVIEW_SLOT_CHILD";
    if std::env::var_os(CASE).is_some() {
        initialize().unwrap();
        let root = current().unwrap().unwrap().root.clone();
        environment::set_metadata_program_for_tests(
            root.ancestors().nth(4).unwrap().join("metadata-probe"),
        );
        let home = root.ancestors().nth(4).unwrap();
        let (binary, _) = crate::services::claude_command::ClaudeBinary::resolve().unwrap();
        let config = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
            tmux_session_name: "review-slot".into(),
            working_dir: root.clone(),
            claude_bin: binary,
            agentdesk_exe: PathBuf::from("/fixture/agentdesk"),
            hook_endpoint: "http://127.0.0.1:18791".into(),
            session_id: "00000000-0000-4000-8000-000000000001".into(),
            system_prompt: None,
            model: None,
            resume: false,
        };
        let files =
            crate::services::claude_tui::session::prepare_claude_tui_launch(&config).unwrap();
        for slot in ["3", "2", "1", "unavailable"] {
            std::fs::write(home.join("active-slot"), slot).unwrap();
            let marker = root.join("runtime/provider-started");
            if marker.exists() {
                std::fs::remove_file(&marker).unwrap();
            }
            let out = Command::new("/bin/bash")
                .arg(&files.launch_script_path)
                .output()
                .unwrap();
            assert_eq!(
                marker.exists(),
                slot == "3",
                "slot={slot}, status={}",
                out.status
            );
        }
        return;
    }
    let (_temp, root, _) = review_fixture();
    let selector =
        "config::test_instance::environment_tests::review_tui_script_rechecks_slot_at_exec";
    let out = tests::command(&root)
        .args(["--exact", selector, "--nocapture"])
        .env(CASE, "1")
        .output()
        .unwrap();
    tests::assert_single_test_passed(&out, selector);
}

#[test]
fn review_builder_purposes_and_metadata_admission() {
    const CHILD: &str = "ADK_REVIEW_BUILDER_CHILD";
    if let Ok(mode) = std::env::var(CHILD) {
        let root = PathBuf::from(std::env::var("ADK_LOGIN_FIXTURE_ROOT").unwrap());
        if mode != "operating" {
            initialize().unwrap();
        }
        let home = root.ancestors().nth(4).unwrap();
        environment::set_metadata_program_for_tests(home.join("metadata-probe"));
        use crate::services::claude_command::{ClaudeBinary, ClaudeCommandBuilder as B};
        let (binary, resolution) = ClaudeBinary::resolve().unwrap();
        let cli = home.join(".local/bin/claude");
        let constructors = [
            B::for_simple(&binary, &resolution),
            B::for_agents(&binary, &resolution),
            B::for_legacy(&binary, &resolution),
            B::for_future_for_test(&binary, &resolution),
            B::for_resolved_version_probe(&binary, &resolution),
            B::for_version_smoke(cli.to_str().unwrap(), cli.to_str().unwrap()),
            B::for_tmux_wrapper_argv(cli.to_str().unwrap()),
        ];
        for (i, mut builder) in constructors.into_iter().enumerate() {
            builder
                .command_mut()
                .args(["--fixture", "unchanged"])
                .env("FIXTURE", "same");
            let mut baseline = Command::new(if i < 5 {
                resolution.resolved_path.as_deref().unwrap()
            } else {
                cli.to_str().unwrap()
            });
            if i < 5 {
                crate::services::platform::apply_binary_resolution(&mut baseline, &resolution);
            } else {
                crate::services::platform::augment_exec_path(&mut baseline, &cli);
            }
            baseline
                .args(["--fixture", "unchanged"])
                .env("FIXTURE", "same");
            let expected_program = baseline.get_program().to_owned();
            let expected_args = baseline
                .get_args()
                .map(std::ffi::OsStr::to_owned)
                .collect::<Vec<_>>();
            let expected_env = baseline
                .get_envs()
                .map(|(k, v)| (k.to_owned(), v.map(std::ffi::OsStr::to_owned)))
                .collect::<Vec<_>>();
            let result = builder.into_command();
            if mode.starts_with("opt-in") && (i < 4 || i == 6) {
                assert!(result.is_err(), "purpose {i} must refuse before spawn");
            } else {
                let command = result.unwrap();
                assert_eq!(command.get_program(), expected_program);
                assert_eq!(
                    command.get_args().collect::<Vec<_>>(),
                    expected_args
                        .iter()
                        .map(|x| x.as_os_str())
                        .collect::<Vec<_>>()
                );
                assert_eq!(
                    command
                        .get_envs()
                        .map(|(k, v)| (k.to_owned(), v.map(std::ffi::OsStr::to_owned)))
                        .collect::<Vec<_>>(),
                    expected_env
                );
            }
        }
        if mode.starts_with("opt-in") {
            use std::os::unix::fs::PermissionsExt;
            for (body, present) in [
                ("#!/bin/sh\nexit 0\n", true),
                ("#!/bin/sh\nexit 1\n", false),
                ("#!/usr/bin/python3\nimport time\ntime.sleep(5)\n", false),
            ] {
                std::fs::write(home.join("metadata-probe"), body).unwrap();
                std::fs::set_permissions(
                    home.join("metadata-probe"),
                    std::fs::Permissions::from_mode(0o700),
                )
                .unwrap();
                assert_eq!(environment::claude_login_presence(), Some(present));
                assert_eq!(environment::claude_launch_admission().is_ok(), present);
                let evidence = crate::services::cluster::readiness::collect_for_login_test();
                assert_eq!(
                    evidence.providers["claude"].credential_profiles["test-claude"],
                    present
                );
                let (binary, _) = ClaudeBinary::resolve().unwrap();
                let config = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
                    tmux_session_name: "metadata".into(),
                    working_dir: root.clone(),
                    claude_bin: binary,
                    agentdesk_exe: PathBuf::from("/fixture/agentdesk"),
                    hook_endpoint: "http://127.0.0.1:18791".into(),
                    session_id: "00000000-0000-4000-8000-000000000001".into(),
                    system_prompt: None,
                    model: None,
                    resume: false,
                };
                let files =
                    crate::services::claude_tui::session::prepare_claude_tui_launch(&config)
                        .unwrap();
                let mut wrapper = B::for_tmux_wrapper_argv(cli.to_str().unwrap());
                wrapper.command_mut().args(
                    crate::services::claude_tui::session::build_claude_tui_args(
                        &config,
                        &files.hook_settings_path,
                    ),
                );
                assert_eq!(wrapper.into_command().is_ok(), present);
            }
            assert!(!root.join("runtime/provider-started").exists());
        }
        return;
    }
    for mode in ["operating", "legacy", "opt-in", "opt-in-cluster"] {
        let (_temp, root, home) = review_fixture();
        if !mode.starts_with("opt-in") {
            for name in ["manifest.json", "receipt.json"] {
                let path = root.join(name);
                let mut value = read_json(&path).unwrap();
                value.as_object_mut().unwrap().remove("claude_login");
                std::fs::write(path, value.to_string()).unwrap();
            }
            let path = root.join("config/org.yaml");
            let mut value = read_json(&path).unwrap();
            value["provider_auth_profiles"]["test-claude"]["home"] =
                serde_json::json!(root.join("home/.adk/profiles/claude/test-claude"));
            std::fs::write(path, value.to_string()).unwrap();
        }
        if mode == "opt-in-cluster" {
            let path = root.join("config/agentdesk.yaml");
            let mut value = read_json(&path).unwrap();
            value["cluster"]["enabled"] = serde_json::json!(true);
            std::fs::write(path, value.to_string()).unwrap();
        }
        let selector = "config::test_instance::environment_tests::review_builder_purposes_and_metadata_admission";
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
            .env(CHILD, mode)
            .env("ADK_LOGIN_FIXTURE_ROOT", &root)
            .output()
            .unwrap();
        tests::assert_single_test_passed(&out, selector);
    }
}

#[test]
fn review_legacy_streaming_and_process_backend_do_not_start_opt_in_cli() {
    const CASE: &str = "ADK_REVIEW_LEGACY_CHILD";
    let selector = "config::test_instance::environment_tests::review_legacy_streaming_and_process_backend_do_not_start_opt_in_cli";
    if let Ok(mode) = std::env::var(CASE) {
        let _env = crate::config::test_env_lock::acquire_shared_test_env_lock();
        initialize().unwrap();
        let root = &current().unwrap().unwrap().root;
        environment::set_metadata_program_for_tests(
            root.ancestors().nth(4).unwrap().join("metadata-probe"),
        );
        if mode == "wrapper" {
            std::fs::write(root.join("runtime/wrapper-entered"), "fixture").unwrap();
            let prompt = root.join("runtime/wrapper-prompt");
            std::fs::write(&prompt, "fixture").unwrap();
            let cli = root.ancestors().nth(4).unwrap().join(".local/bin/claude");
            crate::services::tmux_wrapper::run(
                root.join("runtime/wrapper-output").to_str().unwrap(),
                root.join("runtime/wrapper-input").to_str().unwrap(),
                prompt.to_str().unwrap(),
                root.to_str().unwrap(),
                &[
                    cli.display().to_string(),
                    "--input-format".into(),
                    "stream-json".into(),
                ],
                crate::services::tmux_wrapper::InputMode::Pipe,
            );
            return;
        }
        let wrapper = root.join("runtime/test-process-wrapper");
        std::fs::write(&wrapper,format!("#!/bin/bash\nunset AGENTDESK_TEST_CLAUDE_LOGIN\n{}export {CASE}=wrapper\nexec {} --exact {selector} --nocapture\n",environment::shell_environment().unwrap(),crate::services::process::shell_escape(&std::env::current_exe().unwrap().to_string_lossy()))).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _wrapper = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "ADK_TEST_PROCESS_WRAPPER_EXE",
            &wrapper,
        );
        let (sender, _) = std::sync::mpsc::channel();
        let result = crate::services::claude::execute_command_streaming(
            "fixture",
            None,
            root.to_str().unwrap(),
            sender.clone(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            0,
            None,
            None,
        );
        assert!(result.is_err());
        let result = crate::services::claude::execute_streaming_local_process(
            &[],
            "fixture",
            root.to_str().unwrap(),
            sender,
            None,
            "unbound-instance-process",
            (None, 0),
        );
        let _result = result;
        assert!(
            root.join("runtime/wrapper-entered").exists(),
            "actual wrapper entry must run"
        );
        assert!(!root.join("runtime/provider-started").exists());
        return;
    }
    let (_temp, root, _) = review_fixture();
    let output = tests::command(&root)
        .args(["--exact", selector, "--nocapture"])
        .env(CASE, "1")
        .output()
        .unwrap();
    tests::assert_single_test_passed(&output, selector);
}

mod login_admission_tests {
    use crate::config::test_instance::{environment, tests};
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedExecution, HostedOwner, HostedRecord, HostedState,
    };
    use crate::db::dispatched_sessions::hosted_execution::{HostedLocation, SourceRef};
    use crate::services::claude::herdr_turn::*;
    use crate::services::claude_tui::hook_server::HookEvent;
    use crate::services::herdr_launch::{HerdrLaunchEndpoint, HerdrLaunchHost};
    use crate::services::session_host::HerdrTarget;
    use crate::services::session_host::herdr_socket_rig_tests::{
        HerdrRig, KEY, NODE, PANE, SESSION,
    };
    use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
    use std::sync::Arc;
    use tokio::sync::broadcast;

    #[test]
    fn slot_and_metadata_refuse_new_bound_pending_before_host_or_input() {
        const CASE: &str = "ADK_REVIEW_HERDR_CHILD";
        let selector = "config::test_instance::environment_tests::login_admission_tests::slot_and_metadata_refuse_new_bound_pending_before_host_or_input";
        if std::env::var_os(CASE).is_none() {
            let (_temp, root, _) = environment::login_fixture_for_tests();
            let out = tests::command(&root)
                .args(["--exact", selector, "--nocapture"])
                .env(CASE, "1")
                .output()
                .unwrap();
            tests::assert_single_test_passed(&out, selector);
            return;
        }
        crate::config::test_instance::initialize().unwrap();
        let instance = crate::config::test_instance::current().unwrap().unwrap();
        let root = &instance.root;
        let home = root.ancestors().nth(4).unwrap();
        environment::set_metadata_program_for_tests(home.join("metadata-probe"));
        struct Ports(std::sync::atomic::AtomicUsize);
        impl HerdrTurnPorts for Ports {
            fn launch_host(&self) -> Option<Arc<dyn HerdrLaunchHost>> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                None
            }
            fn hook_events(&self) -> broadcast::Receiver<HookEvent> {
                panic!("no launch host")
            }
            fn attach(&self, _: &AttachRequest<'_>) -> Result<bool, String> {
                Err("fixture attach".into())
            }
            fn confirm_bound(
                &self,
                _: &HostedOwner,
                _: &HostedExecution,
                _: &HerdrTarget,
            ) -> Result<(), String> {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err("fixture bound".into())
            }
        }
        let ports = Ports(std::sync::atomic::AtomicUsize::new(0));
        let rig = HerdrRig::start();
        let _registry = rig.registry_on_this_thread();
        let nonce = "00112233445566778899aabbccddee01";
        let logical = format!("{}-login", instance.tmux.prefix);
        let owner = HostedOwner {
            provider: "claude".into(),
            discord_token_hash: "fixture".into(),
            channel_id: "1".into(),
            logical_key: logical.clone(),
            owner_node: NODE.into(),
            runtime_root: root.display().to_string(),
        };
        let record = HostedExecution {
            schema: 1,
            state: HostedState::Bound,
            execution_nonce: nonce.into(),
            owner: owner.clone(),
            location: Some(HostedLocation {
                host: "herdr".into(),
                execution_node: NODE.into(),
                endpoint_config_key: KEY.into(),
                socket_addr: rig.socket().display().to_string(),
                named_session: SESSION.into(),
                pane_id: PANE.into(),
            }),
            expected: Some(rig.expected(nonce)),
            source_ref: SourceRef {
                runtime_root: root.display().to_string(),
                channel: "1".into(),
                provider: "claude".into(),
                logical_key: logical.clone(),
                execution_nonce: nonce.into(),
                initial_source: None,
                baseline_event_seq: None,
            },
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _entered = runtime.enter();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@127.0.0.1:1/unused")
            .unwrap();
        let endpoint = HerdrLaunchEndpoint {
            execution_node: NODE.into(),
            config_key: KEY.into(),
            socket_addr: rig.socket().display().to_string(),
            herdr_session: SESSION.into(),
        };
        for slot in ["3", "2", "1", "unavailable"] {
            std::fs::write(home.join("active-slot"), slot).unwrap();
            for state in [None, Some(HostedState::Bound), Some(HostedState::Pending)] {
                let row = state.map(|state| {
                    HostedRecord::Known(HostedExecution {
                        state,
                        ..record.clone()
                    })
                });
                let turn = HerdrTurn {
                    pool: &pool,
                    owner: owner.clone(),
                    channel_id: 1,
                    endpoint: endpoint.clone(),
                    row: row.as_ref(),
                    prompt: "fixture",
                    working_dir: root.to_str().unwrap(),
                    session_id: None,
                    force_fresh: false,
                    system_prompt: None,
                    model: None,
                    hook_endpoint: Some("http://127.0.0.1:18791".into()),
                    cancel: None,
                };
                let before = ports.0.load(std::sync::atomic::Ordering::SeqCst);
                let (tx, _) = std::sync::mpsc::channel();
                let result = execute(turn, &ports, tx);
                if slot != "3" {
                    assert_eq!(
                        result.unwrap_err(),
                        "test instance Claude login admission refused"
                    );
                    assert_eq!(ports.0.load(std::sync::atomic::Ordering::SeqCst), before);
                    assert!(rig.sends().is_empty());
                } else {
                    assert_ne!(
                        result.err().as_deref(),
                        Some("test instance Claude login admission refused")
                    );
                }
            }
        }
        std::fs::write(home.join("active-slot"), "3").unwrap();
        std::fs::write(home.join("metadata-probe"), "#!/bin/sh\nexit 1\n").unwrap();
        let turn = HerdrTurn {
            pool: &pool,
            owner,
            channel_id: 1,
            endpoint,
            row: None,
            prompt: "fixture",
            working_dir: root.to_str().unwrap(),
            session_id: None,
            force_fresh: true,
            system_prompt: None,
            model: None,
            hook_endpoint: None,
            cancel: None,
        };
        let (tx, _) = std::sync::mpsc::channel();
        assert_eq!(
            execute(turn, &ports, tx).unwrap_err(),
            "test instance Claude login admission refused"
        );
        assert!(rig.sends().is_empty());
    }
    #[test]
    fn slot_recheck_protects_created_existing_pending_first_input() {
        const CASE: &str = "ADK_REVIEW_INPUT_CHILD";
        let selector = "config::test_instance::environment_tests::login_admission_tests::slot_recheck_protects_created_existing_pending_first_input";
        if std::env::var_os(CASE).is_none() {
            let (_temp, root, _) = environment::login_fixture_for_tests();
            let out = tests::command(&root)
                .args(["--exact", selector, "--nocapture"])
                .env(CASE, "1")
                .output()
                .unwrap();
            tests::assert_single_test_passed(&out, selector);
            return;
        }
        crate::config::test_instance::initialize().unwrap();
        let instance = crate::config::test_instance::current().unwrap().unwrap();
        let root = &instance.root;
        let home = root.ancestors().nth(4).unwrap();
        environment::set_metadata_program_for_tests(home.join("metadata-probe"));
        for lineage in [
            crate::services::claude_tui::owner_input::Lineage::Created,
            crate::services::claude_tui::owner_input::Lineage::Existing,
            crate::services::claude_tui::owner_input::Lineage::Pending,
        ] {
            for slot in ["3", "2", "1", "unavailable"] {
                std::fs::write(home.join("active-slot"), slot).unwrap();
                let first_input = std::cell::Cell::new(0);
                let result = with_input_admission(|| {
                    first_input.set(first_input.get() + 1);
                    Ok(())
                });
                assert_eq!(
                    first_input.get(),
                    usize::from(slot == "3"),
                    "{lineage:?}: {slot}"
                );
                assert_eq!(result.is_ok(), slot == "3");
            }
        }
    }

    #[test]
    fn new_herdr_script_admits_only_the_still_active_declared_slot() {
        const CASE: &str = "ADK_REVIEW_HERDR_SCRIPT_CHILD";
        let selector = "config::test_instance::environment_tests::login_admission_tests::new_herdr_script_admits_only_the_still_active_declared_slot";
        if std::env::var_os(CASE).is_none() {
            let (_temp, root, _) = environment::login_fixture_for_tests();
            let out = tests::command(&root)
                .args(["--exact", selector, "--nocapture"])
                .env(CASE, "1")
                .output()
                .unwrap();
            tests::assert_single_test_passed(&out, selector);
            return;
        }
        crate::config::test_instance::initialize().unwrap();
        let instance = crate::config::test_instance::current().unwrap().unwrap();
        let root = &instance.root;
        let home = root.ancestors().nth(4).unwrap();
        environment::set_metadata_program_for_tests(home.join("metadata-probe"));
        let prepared = PreparedIncarnation::prepare_at(
            "claude",
            &format!("{}-new-herdr", instance.tmux.prefix),
            Some(1),
            None,
            false,
            None,
        )
        .unwrap();
        let overlay = environment::resolve_profile("claude", Some("test-claude"))
            .unwrap()
            .unwrap();
        let auth = environment::provider_shell_environment(&overlay).unwrap();
        let command = prepare_herdr_for_login_test(
            &prepared,
            root,
            "00000000-0000-4000-8000-000000000001",
            None,
            None,
            "http://127.0.0.1:18791".into(),
            &auth,
        )
        .unwrap();
        for slot in ["3", "2", "1", "unavailable"] {
            std::fs::write(home.join("active-slot"), slot).unwrap();
            let marker = root.join("runtime/provider-started");
            if marker.exists() {
                std::fs::remove_file(&marker).unwrap();
            }
            let out = std::process::Command::new("/bin/bash")
                .args(["-c", &command.command])
                .output()
                .unwrap();
            assert_eq!(marker.exists(), slot == "3", "slot {slot}: {}", out.status);
        }
    }
}

fn keychain_case(selector: &str, check: impl FnOnce(&Path, &Path)) {
    const CASE: &str = "ADK_KEYCHAIN_CONTEXT_CHILD";
    if std::env::var_os(CASE).is_some() {
        initialize().unwrap();
        let root = current().unwrap().unwrap().root.clone();
        let home = root.ancestors().nth(4).unwrap();
        let probe = home.join("metadata-probe");
        std::fs::write(
            &probe,
            format!(
                "#!/bin/sh\n[ \"$HOME\" = {} ]\n",
                crate::services::process::shell_escape(&home.to_string_lossy())
            ),
        )
        .unwrap();
        environment::set_metadata_program_for_tests(probe);
        check(&root, home);
        return;
    }
    let (_temp, root, _) = review_fixture();
    let out = tests::command(&root)
        .args(["--exact", selector, "--nocapture"])
        .env(CASE, "1")
        .output()
        .unwrap();
    tests::assert_single_test_passed(&out, selector);
}

#[test]
fn keychain_readiness_uses_login_home_without_changing_core_home() {
    let name = "config::test_instance::environment_tests::keychain_readiness_uses_login_home_without_changing_core_home";
    keychain_case(name, |root, home| {
        assert_eq!(
            std::env::var("HOME").unwrap(),
            root.join("home").to_string_lossy()
        );
        let command = environment::claude_login_metadata_command();
        assert!(
            command
                .get_envs()
                .any(|(key, value)| key == "HOME" && value == Some(home.as_os_str()))
        );
        assert_eq!(environment::claude_login_presence(), Some(true));
        assert_eq!(
            std::env::var("HOME").unwrap(),
            root.join("home").to_string_lossy()
        );
    });
}

#[test]
fn keychain_admission_uses_login_home_for_presence() {
    let name =
        "config::test_instance::environment_tests::keychain_admission_uses_login_home_for_presence";
    keychain_case(name, |root, _| {
        assert!(environment::claude_launch_admission().is_ok());
        assert_eq!(
            std::env::var("HOME").unwrap(),
            root.join("home").to_string_lossy()
        );
    });
}

#[test]
fn keychain_tui_pre_exec_separates_login_home_and_data() {
    let name = "config::test_instance::environment_tests::keychain_tui_pre_exec_separates_login_home_and_data";
    keychain_case(name, |root, home| {
        let output = root.join("runtime/provider-context");
        let cli = home.join(".local/bin/claude");
        std::fs::write(&cli, format!("#!/bin/sh\nprintf '%s\\n%s\\n' \"$HOME\" \"$CLAUDE_CONFIG_DIR\" > {}\nmkdir -p \"$CLAUDE_CONFIG_DIR/projects/fixture\"\nprintf '{{}}\\n' > \"$CLAUDE_CONFIG_DIR/projects/fixture/fixture.jsonl\"\n", crate::services::process::shell_escape(&output.to_string_lossy()))).unwrap();
        let (binary, _) = crate::services::claude_command::ClaudeBinary::resolve().unwrap();
        let config = crate::services::claude_tui::session::ClaudeTuiLaunchConfig {
            tmux_session_name: "keychain-context".into(),
            working_dir: root.into(),
            claude_bin: binary,
            agentdesk_exe: PathBuf::from("/fixture/agentdesk"),
            hook_endpoint: "http://127.0.0.1:18791".into(),
            session_id: "00000000-0000-4000-8000-000000000001".into(),
            system_prompt: None,
            model: None,
            resume: false,
        };
        let files =
            crate::services::claude_tui::session::prepare_claude_tui_launch(&config).unwrap();
        let out = Command::new("/bin/bash")
            .arg(&files.launch_script_path)
            .output()
            .unwrap();
        assert!(out.status.success());
        let data = root.join("home/.adk/profiles/claude/test-claude");
        assert_eq!(
            std::fs::read_to_string(output).unwrap(),
            format!("{}\n{}\n", home.display(), data.display())
        );
        assert!(data.join("projects/fixture/fixture.jsonl").is_file());
        assert!(!home.join(".claude/projects").exists());
        assert!(
            environment::validate_path(&home.join(".claude/projects/fixture/fixture.jsonl"))
                .is_err()
        );
    });
}

#[test]
fn keychain_herdr_overlay_is_scoped_and_global_data_is_refused() {
    const BAD: &str = "ADK_KEYCHAIN_GLOBAL_DATA_CHILD";
    let name = "config::test_instance::environment_tests::keychain_herdr_overlay_is_scoped_and_global_data_is_refused";
    if std::env::var_os(BAD).is_some() {
        assert!(initialize().is_err());
        return;
    }
    keychain_case(name, |root, home| {
        let data = root.join("home/.adk/profiles/claude/test-claude");
        let overlay = environment::resolve_profile("claude", Some("test-claude"))
            .unwrap()
            .unwrap();
        assert_eq!(overlay.home, Some(data.clone()));
        let shell =
            crate::services::provider_auth_profile::overlay_shell_env_lines(&overlay).unwrap();
        let out = Command::new("/bin/bash")
            .args([
                "-c",
                &format!("{shell}\nprintf '%s\\n%s\\n' \"$HOME\" \"$CLAUDE_CONFIG_DIR\""),
            ])
            .output()
            .unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8(out.stdout).unwrap(),
            format!("{}\n{}\n", home.display(), data.display())
        );
        assert_eq!(overlay.env["CLAUDE_SECURESTORAGE_CONFIG_DIR"], "");
    });
    if std::env::var_os("ADK_KEYCHAIN_CONTEXT_CHILD").is_none() {
        let (_temp, root, home) = review_fixture();
        let mut child = tests::command(&root);
        for file in ["manifest.json", "receipt.json"] {
            let path = root.join(file);
            let mut value = read_json(&path).unwrap();
            value["claude_login"]["config_dir"] = serde_json::json!(home.join(".claude"));
            std::fs::write(path, value.to_string()).unwrap();
        }
        let path = root.join("config/org.yaml");
        let mut org = read_json(&path).unwrap();
        org["provider_auth_profiles"]["test-claude"]["home"] =
            serde_json::json!(home.join(".claude"));
        std::fs::write(path, org.to_string()).unwrap();
        let out = child
            .args(["--exact", name, "--nocapture"])
            .env(BAD, "1")
            .env("CLAUDE_CONFIG_DIR", home.join(".claude"))
            .output()
            .unwrap();
        tests::assert_single_test_passed(&out, name);
    }
}

#[test]
fn claude_login_requires_the_exact_declared_pool() {
    let (_temp, root, home) = claude_login_fixture(false);
    bind_claude_login(&root, &home);
    let manifest = read_json(&root.join("manifest.json")).unwrap();
    let config = read_json(&root.join("config/agentdesk.yaml")).unwrap();
    assert!(environment::Environment::load(&root, &manifest, &config).is_ok());
    let other = home
        .join("other/pool/instances")
        .join(root.file_name().unwrap());
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::rename(&root, &other).unwrap();
    for name in [
        "manifest.json",
        "receipt.json",
        "config/agentdesk.yaml",
        "config/org.yaml",
    ] {
        let path = other.join(name);
        let body = std::fs::read_to_string(&path)
            .unwrap()
            .replace(root.to_str().unwrap(), other.to_str().unwrap());
        std::fs::write(path, body).unwrap();
    }
    let manifest = read_json(&other.join("manifest.json")).unwrap();
    let config = read_json(&other.join("config/agentdesk.yaml")).unwrap();
    assert!(environment::Environment::load(&other, &manifest, &config).is_err());
}

#[test]
fn claude_login_tmux_is_visible_and_absent_session_is_not_a_probe_error() {
    let name = concat!(
        module_path!(),
        "::claude_login_tmux_is_visible_and_absent_session_is_not_a_probe_error"
    );
    let name = name.strip_prefix("agentdesk::").unwrap_or(name);
    if std::env::var_os("ADK_TMUX_BINDING_CHILD").is_some() {
        initialize().unwrap();
        let command = crate::services::platform::binary_resolver::runtime_command("tmux").unwrap();
        #[cfg(target_os = "macos")]
        let bin = ["/opt/homebrew/opt/tmux/bin", "/usr/local/opt/tmux/bin"]
            .into_iter()
            .map(Path::new)
            .find(|p| p.exists())
            .unwrap()
            .canonicalize()
            .unwrap();
        #[cfg(not(target_os = "macos"))]
        let bin = current()
            .unwrap()
            .unwrap()
            .root
            .join("fixture-worktree/target/debug");
        assert_eq!(command.get_program(), bin.join("tmux"));
        assert!(crate::services::platform::binary_resolver::runtime_command("codex").is_err());
        let session = format!("{}-missing", current().unwrap().unwrap().tmux.prefix);
        assert_eq!(
            crate::services::platform::tmux::pane_liveness(&session),
            crate::services::platform::tmux::PaneLiveness::DeadOrAbsent
        );
        return;
    }
    let (_temp, root, home) = claude_login_fixture(false);
    bind_claude_login(&root, &home);
    #[cfg(not(target_os = "macos"))]
    {
        use std::os::unix::fs::PermissionsExt;
        let tmux = root.join("fixture-worktree/target/debug/tmux");
        std::fs::write(
            &tmux,
            "#!/bin/sh\necho 'no server running on fixture socket' >&2\nexit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let result = tests::command(&root)
        .args(["--exact", name, "--nocapture"])
        .env("ADK_TMUX_BINDING_CHILD", "1")
        .output()
        .unwrap();
    tests::assert_single_test_passed(&result, name);
}
