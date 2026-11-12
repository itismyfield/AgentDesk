use super::*;

pub(crate) fn with_instance(test_name: &str, check: impl FnOnce()) {
    if std::env::var_os("I2B_PROVIDER_CHILD").is_none() {
        let (_temporary, root) = crate::config::test_instance::tests::fixture();
        let output = crate::config::test_instance::tests::command(&root)
            .args([
                "--exact",
                test_name.split_once("::").unwrap().1,
                "--nocapture",
            ])
            .env("I2B_PROVIDER_CHILD", "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
        return;
    }
    let _lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    crate::config::test_instance::initialize().unwrap();
    check();
}

fn overlay(provider: ProviderKind) -> ProviderAuthOverlay {
    let id = format!("test-{}", provider.as_str());
    resolve(provider, Some(&id), None, &queries::provider_auth_catalog()).unwrap()
}

pub(crate) const CAPTURE: &str = r#"printf '%s\n' "$HOME" "$TMPDIR" "$XDG_CONFIG_HOME" "$CLAUDE_CONFIG_DIR" "$CODEX_HOME" "$AGENTDESK_API_URL" "$ADK_API_URL" "$AGENTDESK_DCSERVER_LABEL" "$AGENTDESK_RELEASE_TMUX_SESSION" "$AGENTDESK_TEST_MANIFEST" "$AGENTDESK_ROOT_DIR" "$AGENTDESK_CONFIG" "${MEMENTO_ACCESS_KEY+present}" "${ANTHROPIC_API_KEY+present}" "${OPENAI_API_KEY+present}" "${OP_SERVICE_ACCOUNT_TOKEN+present}" "$1""#;

pub(crate) fn assert_child_environment(output: std::process::Output, root: &Path, argument: &str) {
    assert!(output.status.success());
    assert_environment_capture(&output.stdout, root, argument);
}

pub(crate) fn assert_environment_capture(value: &[u8], root: &Path, argument: &str) {
    let value = std::str::from_utf8(value).unwrap();
    let lines: Vec<_> = value.lines().collect();
    let id = root.file_name().unwrap().to_str().unwrap();
    assert_eq!(lines.len(), 17);
    for (index, relative) in [
        (0, "home"),
        (1, "tmp"),
        (2, "xdg"),
        (3, "home/.adk/profiles/claude/test-claude"),
        (4, "home/.adk/profiles/codex/test-codex"),
    ] {
        assert_eq!(lines[index], root.join(relative).to_str().unwrap());
    }
    assert_eq!(lines[5], std::env::var("AGENTDESK_API_URL").unwrap());
    assert_eq!(lines[6], lines[5]);
    assert_eq!(lines[7], format!("com.agentdesk.test.{id}"));
    assert_eq!(lines[8], format!("AdkTest-{id}-dcserver"));
    assert_eq!(lines[9], root.join("manifest.json").to_str().unwrap());
    assert_eq!(lines[10], root.to_str().unwrap());
    assert_eq!(
        lines[11],
        root.join("config/agentdesk.yaml").to_str().unwrap()
    );
    assert_eq!(&lines[12..16], &["", "", "", ""]);
    assert_eq!(lines[16], argument);
}

#[test]
fn test_instance_provider_resolver_command_and_shell_share_scope() {
    with_instance(
        concat!(
            module_path!(),
            "::test_instance_provider_resolver_command_and_shell_share_scope"
        ),
        || {
            let instance = crate::config::test_instance::current().unwrap().unwrap();
            let catalog = queries::provider_auth_catalog();
            assert_eq!(catalog.len(), 2);
            assert_eq!(
                extra_accounts_root().unwrap(),
                instance.root.join("home/.adk/profiles")
            );
            for provider in [ProviderKind::Claude, ProviderKind::Codex] {
                let mut overlay = overlay(provider);
                for key in ["HOME", "AGENTDESK_API_URL", "AGENTDESK_TEST_MANIFEST"] {
                    overlay.unset.insert(key.into());
                }
                let mut direct = Command::new("/bin/sh");
                direct.args(["-c", CAPTURE, "fake-provider", "provider-argument"]);
                apply_overlay_to_command(&mut direct, &overlay).unwrap();
                assert_child_environment(
                    direct.output().unwrap(),
                    &instance.root,
                    "provider-argument",
                );
                let script = format!(
                    "{}\n[ -z \"${{MEMENTO_MCP_TOKEN+x}}\" ] && [ -z \"${{AGENT_OP_TEST_SECRET+x}}\" ] && [ -z \"${{XDG_DATA_HOME+x}}\" ] && [ -z \"${{CLAUDE_CODE_OAUTH_TOKEN+x}}\" ] || exit 71\n{}",
                    overlay_shell_env_lines(&overlay).unwrap(),
                    CAPTURE
                );
                let output = Command::new("/bin/sh")
                    .args(["-c", &script, "fake-provider", "provider-argument"])
                    .env_clear()
                    .env("MEMENTO_ACCESS_KEY", "synthetic-parent")
                    .env("ANTHROPIC_API_KEY", "synthetic-parent")
                    .env("OPENAI_API_KEY", "synthetic-parent")
                    .env("OP_SERVICE_ACCOUNT_TOKEN", "synthetic-parent")
                    .env("MEMENTO_MCP_TOKEN", "synthetic-parent")
                    .env("AGENT_OP_TEST_SECRET", "synthetic-parent")
                    .env("XDG_DATA_HOME", "/synthetic/herdr-data")
                    .env("CLAUDE_CODE_OAUTH_TOKEN", "synthetic-parent")
                    .output()
                    .unwrap();
                assert_child_environment(output, &instance.root, "provider-argument");
            }
        },
    );
}

#[test]
fn test_instance_provider_rejects_default_unknown_foreign_and_linked_homes() {
    with_instance(
        concat!(
            module_path!(),
            "::test_instance_provider_rejects_default_unknown_foreign_and_linked_homes"
        ),
        || {
            use std::os::unix::fs::symlink;
            let catalog = queries::provider_auth_catalog();
            for provider in [ProviderKind::Claude, ProviderKind::Codex] {
                for requested in [None, Some("default"), Some("unknown")] {
                    assert!(resolve(provider.clone(), requested, None, &catalog).is_err());
                }
                let id = format!("test-{}", provider.as_str());
                let mut foreign = catalog.clone();
                foreign.get_mut(&id).unwrap().home = Some("/synthetic/foreign/home".into());
                assert!(resolve(provider.clone(), Some(&id), None, &foreign).is_err());
                let home = overlay(provider.clone()).home.unwrap();
                let moved = home.with_extension("moved");
                std::fs::rename(&home, &moved).unwrap();
                symlink(&moved, &home).unwrap();
                assert!(resolve(provider.clone(), Some(&id), None, &catalog).is_err());
                std::fs::remove_file(&home).unwrap();
                std::fs::rename(&moved, &home).unwrap();
                assert!(resolve(provider, Some(&id), None, &catalog).is_ok());
            }
            assert!(resolve(ProviderKind::Qwen, Some("test-qwen"), None, &catalog).is_err());
            let instance = crate::config::test_instance::current().unwrap().unwrap();
            let org = instance.root.join("config/org.yaml");
            let saved = instance.root.join("saved-org.json");
            let foreign = instance.root.join("foreign-org.json");
            std::fs::copy(&org, &foreign).unwrap();
            std::fs::rename(&org, &saved).unwrap();
            symlink(&foreign, &org).unwrap();
            assert!(queries::provider_auth_catalog().is_empty());
            std::fs::remove_file(&org).unwrap();
            std::fs::rename(&saved, &org).unwrap();
            assert_eq!(queries::provider_auth_catalog().len(), 2);

            let mut credential = Command::new("/bin/false");
            credential.env("MEMENTO_ACCESS_KEY", "synthetic-child");
            assert!(
                apply_overlay_to_command(&mut credential, &overlay(ProviderKind::Codex)).is_err()
            );
            let mut foreign = overlay(ProviderKind::Codex);
            foreign
                .env
                .insert("HOME".into(), "/synthetic/foreign/home".into());
            assert!(apply_overlay_to_command(&mut Command::new("/bin/false"), &foreign).is_err());
            assert!(overlay_shell_env_lines(&foreign).is_err());
        },
    );
}

#[test]
fn test_instance_provider_snapshot_survives_marker_removal_and_refuses_home_drift() {
    with_instance(
        concat!(
            module_path!(),
            "::test_instance_provider_snapshot_survives_marker_removal_and_refuses_home_drift"
        ),
        || {
            use crate::config::TestEnvVarGuard as Guard;
            let overlay = overlay(ProviderKind::Claude);
            let _home = Guard::set_value_after_shared_test_env_lock(
                "HOME",
                std::ffi::OsStr::new("/synthetic/foreign/home"),
            );
            let _marker = Guard::remove_after_shared_test_env_lock("AGENTDESK_TEST_MANIFEST");
            assert!(crate::config::test_instance::requested());
            assert!(resolve(ProviderKind::Claude, None, None, &HashMap::new()).is_err());
            assert!(apply_overlay_to_command(&mut Command::new("/bin/false"), &overlay).is_err());
            assert!(overlay_shell_env_lines(&overlay).is_err());
            assert!(crate::services::claude_tui::transcript_tail::default_claude_home().is_none());
            assert!(crate::services::codex_tui::rollout_tail::default_codex_home().is_none());
        },
    );
}

#[test]
fn test_instance_restart_discovery_reads_only_declared_provider_homes() {
    with_instance(
        concat!(
            module_path!(),
            "::test_instance_restart_discovery_reads_only_declared_provider_homes"
        ),
        || {
            use crate::services::claude_tui::transcript_tail::claude_transcript_path;
            use crate::services::codex_tui::rollout_tail::{
                find_rollout_by_session_id, find_rollout_by_session_id_under,
            };
            let instance = crate::config::test_instance::current().unwrap().unwrap();
            let session = "c48f4954-32b9-4d48-b2eb-bcf7eb2a35a1";
            let named = instance
                .root
                .join("home/.adk/profiles/codex/test-codex/sessions");
            let foreign = instance.root.join("home/.codex/sessions");
            for directory in [&named, &foreign] {
                std::fs::create_dir_all(directory).unwrap();
                std::fs::write(directory.join(format!("rollout-{session}.jsonl")), b"{}\n")
                    .unwrap();
            }
            assert_eq!(
                find_rollout_by_session_id(session),
                Some(named.join(format!("rollout-{session}.jsonl")))
            );
            assert!(find_rollout_by_session_id_under(&foreign, session).is_none());
            let owned = named.join(format!("rollout-{session}.jsonl"));
            let saved = instance.root.join("owned-rollout.jsonl");
            std::fs::rename(&owned, &saved).unwrap();
            std::os::unix::fs::symlink(foreign.join(format!("rollout-{session}.jsonl")), &owned)
                .unwrap();
            assert!(find_rollout_by_session_id(session).is_none());
            std::fs::remove_file(&owned).unwrap();
            std::fs::rename(&saved, &owned).unwrap();
            let nested = "00112233-4455-4677-8899-aabbccddeeff";
            std::fs::write(foreign.join(format!("rollout-{nested}.jsonl")), b"{}\n").unwrap();
            std::os::unix::fs::symlink(&foreign, named.join("foreign-date")).unwrap();
            assert!(find_rollout_by_session_id(nested).is_none());
            std::fs::remove_file(named.join("foreign-date")).unwrap();

            let claude = overlay(ProviderKind::Claude).home.unwrap();
            let transcript = claude_transcript_path(&instance.root, session, None).unwrap();
            std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
            std::fs::write(&transcript, b"{}\n").unwrap();
            assert!(transcript.starts_with(&claude));
            let saved = instance.root.join("owned-transcript.jsonl");
            std::fs::rename(&transcript, &saved).unwrap();
            std::os::unix::fs::symlink(&saved, &transcript).unwrap();
            assert!(claude_transcript_path(&instance.root, session, None).is_err());
            std::fs::remove_file(&transcript).unwrap();
            std::fs::rename(&saved, &transcript).unwrap();
            let project = transcript.parent().unwrap();
            let saved = instance.root.join("owned-project");
            std::fs::rename(project, &saved).unwrap();
            std::os::unix::fs::symlink(&saved, project).unwrap();
            assert!(claude_transcript_path(&instance.root, session, None).is_err());
            std::fs::remove_file(project).unwrap();
            std::fs::rename(&saved, project).unwrap();

            assert_eq!(
                claude_transcript_path(&instance.root, session, Some(&claude)).unwrap(),
                transcript
            );
            assert!(
                claude_transcript_path(
                    &instance.root,
                    session,
                    Some(&instance.root.join("home/.claude"))
                )
                .is_err()
            );
            let _foreign = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                "CODEX_HOME",
                &foreign,
            );
            assert!(find_rollout_by_session_id(session).is_none());
        },
    );
}

pub(crate) fn fake_provider(root: &Path, provider: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let binary = root.join("fixture-worktree/target/debug").join(provider);
    let argv = root.join(format!("{provider}.argv"));
    let version = if provider == "claude" {
        "2.1.0 (Claude Code)"
    } else {
        "codex-cli 0.160.0"
    };
    let body = format!(
        "#!/bin/bash\nif [ \"$1\" = --version ]; then echo '{version}'; exit 0; fi\nif [ \"$1 $2\" = 'resume --help' ]; then echo '--no-daemon --dangerously-bypass-hook-trust'; exit 0; fi\nprintf '%s\\n' \"$@\" > {}\nset -- native-capture\n{CAPTURE}\n",
        crate::services::process::shell_escape(&argv.to_string_lossy()),
    );
    std::fs::write(&binary, body).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    binary
}

pub(crate) fn capturing_tmux(root: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let tmux = root.join("fixture-worktree/target/debug/tmux");
    std::fs::write(
        &tmux,
        r#"#!/bin/bash
while [ "$1" = -u ] || [ "$1" = -S ]; do
  if [ "$1" = -S ]; then shift 2; else shift; fi
done
printf '%s\n' "$*" >> "$AGENTDESK_ROOT_DIR/tmux.calls"
case "$1" in
new-session|respawn-pane)
  for command in "$@"; do :; done
  /bin/bash -c "$command" > "$AGENTDESK_ROOT_DIR/native.capture" || exit 42
  touch "$AGENTDESK_ROOT_DIR/native.executed"
  echo 'synthetic tmux refusal after child capture' >&2
  exit 1 ;;
list-panes) echo 1 ;;
has-session) exit 1 ;;
*) exit 0 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o700)).unwrap();
}
