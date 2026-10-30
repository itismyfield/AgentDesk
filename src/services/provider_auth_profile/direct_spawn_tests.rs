//! Direct CLI launches must use the same configured account home as hosted launches.
use super::*;
use crate::config::TestEnvVarGuard as Guard;
use crate::services::agent_protocol::StreamMessage;
use crate::services::provider_cli::ProviderExecutionContext;
use std::os::unix::fs::PermissionsExt;

struct Fixture {
    _guards: Vec<Guard>,
    root: tempfile::TempDir,
    capture: PathBuf,
    inherited: PathBuf,
}

impl Fixture {
    /// The caller owns the shared environment lock throughout fixture cleanup.
    fn new(provider: &ProviderKind) -> Self {
        let root = tempfile::tempdir().unwrap();
        let inherited = root.path().join("inherited-account");
        let fake_home = root.path().join("user-home");
        std::fs::create_dir_all(&fake_home).unwrap();
        std::fs::create_dir_all(root.path().join("config")).unwrap();
        let config = root.path().join("config/agentdesk.yaml");
        std::fs::write(&config, "server: {}\n").unwrap();
        let capture = root.path().join("child-account");
        let cli = root.path().join("fake-provider");
        let (home_key, credential_key, version, response) = match provider {
            ProviderKind::Claude => (
                "CLAUDE_CONFIG_DIR",
                "ANTHROPIC_API_KEY",
                "2.1.0 (Claude Code)",
                r#"{"type":"result","subtype":"success","result":"ok","session_id":"fixture-session"}"#,
            ),
            ProviderKind::Codex => (
                "CODEX_HOME",
                "OPENAI_API_KEY",
                "codex-cli 0.157.1",
                r#"{"type":"item.completed","item":{"type":"agent_message","text":"ok"}}"#,
            ),
            _ => unreachable!(),
        };
        std::fs::write(&cli, format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then printf '%s\\n' '{version}'; exit 0; fi\nprintf '%s\\n%s\\n' \"${home_key}\" \"${{{credential_key}+set}}\" > \"$ADK_AUTH_CAPTURE\"\ncat >/dev/null\nprintf '%s\\n' '{response}'\n",
        )).unwrap();
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
        let path = Guard::set_path_after_shared_test_env_lock;
        let guards = vec![
            path("AGENTDESK_ROOT_DIR", root.path()),
            path("HOME", &fake_home),
            path("AGENTDESK_CONFIG", &config),
            path("CLAUDE_CONFIG_DIR", &inherited),
            path("CODEX_HOME", &inherited),
            path("AGENTDESK_CLAUDE_PATH", &cli),
            path("AGENTDESK_CODEX_PATH", &cli),
            path("ADK_AUTH_CAPTURE", &capture),
            Guard::set_value_after_shared_test_env_lock(
                credential_key,
                std::ffi::OsStr::new("fixture-parent-key"),
            ),
        ];
        Self {
            _guards: guards,
            root,
            capture,
            inherited,
        }
    }

    fn configure_profile(&self, provider: &ProviderKind, profile: &str) -> PathBuf {
        let home = create_empty_profile_home_at(&extra_accounts_root().unwrap(), provider, "work")
            .unwrap();
        let schema = serde_json::json!({
            "version": 1,
            "agents": {"profile-agent": {"display_name":"Profile fixture", "provider":provider.as_str(), "auth_profile":profile}},
            "provider_auth_profiles": {"work": {"provider":provider.as_str(), "home":home}},
        });
        std::fs::write(
            self.root.path().join("config/org.yaml"),
            serde_yaml::to_string(&schema).unwrap(),
        )
        .unwrap();
        home
    }

    fn run(&self, provider: &ProviderKind) -> Result<(), String> {
        let context = ProviderExecutionContext {
            provider: provider.as_str().into(),
            agent_id: Some("profile-agent".into()),
            channel_id: Some("61018001".into()),
            execution_mode: Some("discord_turn".into()),
            ..Default::default()
        };
        let (tx, rx) = std::sync::mpsc::channel::<StreamMessage>();
        let working_dir = self.root.path().to_str().unwrap();
        let result =
            crate::services::platform::with_provider_execution_context(
                context,
                || match provider {
                    ProviderKind::Claude => crate::services::claude::execute_command_streaming(
                        "fixture prompt",
                        None,
                        working_dir,
                        tx,
                        Some("fixture role"),
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
                    ),
                    ProviderKind::Codex => crate::services::codex::execute_command_streaming(
                        "fixture prompt",
                        None,
                        working_dir,
                        tx,
                        Some("fixture role"),
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
                        true,
                    ),
                    _ => unreachable!(),
                },
            );
        if result.is_ok() {
            assert!(
                rx.try_iter()
                    .any(|message| matches!(message, StreamMessage::Done { .. })),
                "fake provider must actually finish its direct child turn"
            );
        }
        result
    }
}

#[test]
fn direct_process_preserves_default_and_named_auth_profiles_in_actual_child() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        let fixture = Fixture::new(&provider);
        fixture.run(&provider).unwrap();
        assert_eq!(
            std::fs::read_to_string(&fixture.capture).unwrap(),
            format!("{}\nset\n", fixture.inherited.display())
        );
        let home = fixture.configure_profile(&provider, "work");
        fixture.run(&provider).unwrap();
        assert_eq!(
            std::fs::read_to_string(&fixture.capture).unwrap(),
            format!("{}\n\n", home.display()),
            "named account must replace the inherited home and remove its credential"
        );
    }
}

#[test]
fn direct_process_refuses_unknown_auth_profile_before_starting_child() {
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        let fixture = Fixture::new(&provider);
        fixture.configure_profile(&provider, "missing");
        let error = fixture.run(&provider).unwrap_err();
        assert!(
            error.contains("auth_profile") && error.contains("missing"),
            "configured account failure must be explicit"
        );
        assert!(
            !fixture.capture.exists(),
            "invalid accounts must not execute under the inherited home"
        );
    }
}
