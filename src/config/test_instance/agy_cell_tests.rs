use super::super::tests as fixture;
use super::super::*;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

const CHILD: &str = "ADK_AGY_CELL_CHILD";

pub(crate) use super::super::test_support::agy_fixture;

fn write_credential(root: &Path, name: &str) -> String {
    let value = format!("SYNTHETIC_GEMINI_{name}");
    let path = root.join("credential").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, &value).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    value
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
    // agy itself leaves a zero-byte MCP config behind; the key file reaches only the turn.
    for case in ["plain", "mcp", "empty_mcp", "key"] {
        let with_mcp = case == "mcp";
        let (_temp, root, home) = agy_fixture();
        let login = home.join(".adk/dev/agy-login");
        if case.ends_with("mcp") {
            std::fs::create_dir_all(login.join(".gemini/config")).unwrap();
            std::fs::write(
                login.join(".gemini/config/mcp_config.json"),
                if with_mcp {
                    r#"{"mcpServers":{"memento":{"url":"http://127.0.0.1:1/mcp"}}}"#
                } else {
                    ""
                },
            )
            .unwrap();
        }
        let key = (case == "key").then(|| write_credential(&root, "gemini_api_key"));
        let mut child = fixture::command(&root);
        let output = child
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, "1")
            .env("SYNTHETIC_AMBIENT", "dcserver-only")
            .output()
            .unwrap();
        fixture::assert_single_test_passed(&output, name);
        let printed = [output.stdout.as_slice(), &output.stderr].concat();
        assert!(!String::from_utf8_lossy(&printed).contains("SYNTHETIC_GEMINI"));
        let mut runs: Vec<(String, std::collections::BTreeMap<String, String>)> = Vec::new();
        for line in std::fs::read_to_string(root.join("runtime/agy-env"))
            .unwrap_or_default()
            .lines()
        {
            if let Some(kind) = line.strip_prefix("ADK_CHILD ") {
                runs.push((kind.to_owned(), Default::default()));
            } else if let (Some((_, env)), Some((key, value))) =
                (runs.last_mut(), line.split_once('='))
            {
                env.insert(key.to_owned(), value.to_owned());
            }
        }
        let turns = runs.iter().filter(|(kind, _)| kind != "--version").count();
        assert!(turns < runs.len(), "version probe launch was not observed");
        assert_eq!(
            turns,
            usize::from(!with_mcp),
            "turn launches with MCP={with_mcp}"
        );
        let path = format!(
            "{}:/usr/bin:/bin",
            root.join("fixture-worktree/target/debug").display()
        );
        for (kind, env) in &runs {
            // The version probe keeps the instance HOME; only the turn runs from the login home.
            let home = if kind == "--version" {
                root.join("home")
            } else {
                login.clone()
            };
            assert_eq!(env.get("HOME").map(String::as_str), home.to_str(), "{kind}");
            assert_eq!(env.get("PATH"), Some(&path), "{kind}");
            let turn_key = key.as_ref().filter(|_| kind != "--version");
            assert_eq!(env.get("GEMINI_API_KEY"), turn_key, "{case} {kind}");
            assert!(
                !env.contains_key("SYNTHETIC_AMBIENT"),
                "dcserver env leaked into {kind}: {env:?}"
            );
        }
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
