use super::super::environment_tests::claude_login_fixture;
use super::super::tests as fixture;
use super::super::*;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

const CHILD: &str = "ADK_AGY_CELL_CHILD";

/// A declared agy-headless instance under `<home>/.adk/dev/instances` with a fake agy CLI.
fn agy_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (temp, root, home) = claude_login_fixture(false);
    let login = home.join(".adk/dev/agy-login");
    let cli = home.join("agy-bin/agy");
    std::fs::create_dir_all(&login).unwrap();
    std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
    let capture = root.join("runtime/agy-env");
    std::fs::write(
        &cli,
        format!(
            "#!/bin/sh\n[ \"$1\" = --version ] && echo 1.0.0 && exit 0\n/usr/bin/env > {}\n",
            crate::services::process::shell_escape(&capture.to_string_lossy())
        ),
    )
    .unwrap();
    std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o700)).unwrap();
    let declaration = json!({"mode":"dedicated_home","login_home":login,"cli":cli});
    for name in ["manifest.json", "receipt.json"] {
        let path = root.join(name);
        let mut value = read_json(&path).unwrap();
        value["agy_login"] = declaration.clone();
        if name == "manifest.json" {
            value["profile"]["name"] = json!("agy-headless");
        }
        std::fs::write(path, value.to_string()).unwrap();
    }
    std::fs::write(root.join("config/org.yaml"), json!({"version":1,
        "agents":{"antigravity":{"display_name":"a","provider":"antigravity","auth_profile":"test-antigravity"}},
        "provider_auth_profiles":{"test-antigravity":{"provider":"antigravity","home":login,"env":{}}},
        "provider_auth_primary_profiles":{"antigravity":"test-antigravity"}}).to_string()).unwrap();
    (temp, root, home)
}

fn spawn_agy() -> Result<(), String> {
    use crate::services::provider::ProviderKind;
    let overlay = crate::services::provider_auth_profile::resolve(
        ProviderKind::Antigravity,
        Some("test-antigravity"),
        None,
        &Default::default(),
    )
    .map_err(|error| format!("{error:?}"))?;
    let request = crate::services::stream_json_cli::request::ProviderTurnRequest {
        provider: ProviderKind::Antigravity,
        prompt: "hello".into(),
        system_prompt: None,
        tool_policy: crate::services::stream_json_cli::policy::ConfiguredToolPolicy::for_new_stream_json_provider(),
        model: None,
        reasoning_effort: None,
        working_directory: current()?.unwrap().root.join("tmp"),
        session: None,
        timeout: std::time::Duration::from_secs(10),
        cancel: None,
        auth_overlay: overlay,
    };
    let prepared = crate::services::stream_json_cli::dialects::agy::prepare(&request)?;
    let (sender, _receiver) = std::sync::mpsc::channel();
    crate::services::stream_json_cli::runner::run_prepared(prepared, sender, request.timeout, None)
}

#[test]
fn agy_child_gets_only_declared_env_and_the_mcp_free_login_home() {
    let name = "config::test_instance::agy_cell::tests::agy_child_gets_only_declared_env_and_the_mcp_free_login_home";
    if std::env::var_os(CHILD).is_some() {
        initialize().unwrap();
        let _ = spawn_agy();
        return;
    }
    for with_mcp in [false, true] {
        let (_temp, root, home) = agy_fixture();
        let login = home.join(".adk/dev/agy-login");
        if with_mcp {
            std::fs::create_dir_all(login.join(".gemini/config")).unwrap();
            std::fs::write(
                login.join(".gemini/config/mcp_config.json"),
                r#"{"mcpServers":{"memento":{"url":"http://127.0.0.1:1/mcp"}}}"#,
            )
            .unwrap();
        }
        let mut child = fixture::command(&root);
        let output = child
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, "1")
            .env("SYNTHETIC_AMBIENT", "dcserver-only")
            .output()
            .unwrap();
        fixture::assert_single_test_passed(&output, name);
        let captured = std::fs::read_to_string(root.join("runtime/agy-env"));
        if with_mcp {
            assert!(
                captured.is_err(),
                "agy spawned despite an MCP server in its login home"
            );
            continue;
        }
        let env: std::collections::BTreeMap<_, _> = captured
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        assert_eq!(env.get("HOME").map(String::as_str), login.to_str());
        assert_eq!(
            env.get("PATH").map(String::as_str),
            Some(
                format!(
                    "{}:/usr/bin:/bin",
                    root.join("fixture-worktree/target/debug").display()
                )
                .as_str()
            )
        );
        assert!(
            !env.contains_key("SYNTHETIC_AMBIENT"),
            "dcserver env leaked into agy: {env:?}"
        );
    }
}

#[test]
fn agy_login_declaration_refuses_foreign_homes_and_profile_mismatch() {
    let load = |root: &Path| {
        load_instance_declaration(
            &root.join("manifest.json"),
            root,
            &root.join("config/agentdesk.yaml"),
        )
    };
    let (_temp, root, _) = agy_fixture();
    load(&root).unwrap();
    for case in ["real_home", "symlink_home", "no_login", "tui_live_name"] {
        let (_temp, root, home) = agy_fixture();
        if case == "symlink_home" {
            let login = home.join(".adk/dev/agy-login");
            std::fs::remove_dir(&login).unwrap();
            std::os::unix::fs::symlink(&home, &login).unwrap();
        }
        for name in ["manifest.json", "receipt.json"] {
            let path = root.join(name);
            let mut value = read_json(&path).unwrap();
            match case {
                "real_home" => value["agy_login"]["login_home"] = json!(home),
                "no_login" => drop(value.as_object_mut().unwrap().remove("agy_login")),
                "tui_live_name" if name == "manifest.json" => {
                    value["profile"]["name"] = json!("tui-live")
                }
                _ => {}
            }
            std::fs::write(path, value.to_string()).unwrap();
        }
        assert!(load(&root).is_err(), "accepted {case}");
    }
}
