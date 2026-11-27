//! Test-instance fixtures upper-crate tests reuse once KRX is its own crate: the instance generator, its child
//! command, the single-pass assertion and the agy-headless cell; the subprocess runners stay with the unit tests.

use super::*;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

pub fn fixture() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::Builder::new()
        .prefix("adk-i2-")
        .tempdir_in("/tmp")
        .unwrap();
    let root = temp
        .path()
        .canonicalize()
        .unwrap()
        .join(uuid::Uuid::new_v4().to_string());
    for directory in [
        "",
        "config",
        "runtime",
        "runtime/tmux",
        "runtime/herdr",
        "herdr-home",
        "home/.adk/profiles/claude/test-claude",
        "home/.adk/profiles/codex/test-codex",
        "tmp",
        "xdg",
        "fixture-worktree/target/debug",
    ] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(root.join(directory), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
    }
    let id = root.file_name().unwrap().to_str().unwrap();
    let manifest = json!({
        "schema_version":1,"status":"DECLARED","live_ready":false,"instance_id":id,
        "root":root,"worktree":root.join("fixture-worktree"),"head":"a".repeat(40),"profile":{
            "features":{},"bots":{},"channels":[]},"applied_env":{},
        "api":{"host":"127.0.0.1","port":18791,"self_url":"http://127.0.0.1:18791"},"postgres":{},
        "config_path":root.join("config/agentdesk.yaml"),"execution_node_id":id,
        "preferred_node_seed_required":true,"hosts":{
            "tmux":{"socket":root.join("runtime/tmux/socket"),"tmpdir":root.join("runtime/tmux"),
                "prefix":format!("AdkTest-{id}")},
            "herdr":{"socket":root.join("runtime/herdr/socket"),"home":root.join("herdr-home"),
                "session":format!("adk-test-{id}"),"endpoint_key":format!("test-{id}")}}
    });
    let mut receipt = manifest.as_object().unwrap().clone();
    receipt.remove("profile");
    receipt.remove("config_path");
    for key in ["features", "bots", "channels"] {
        receipt.insert(key.into(), manifest["profile"][key].clone());
    }
    write(&root.join("manifest.json"), &manifest);
    write(&root.join("receipt.json"), &Value::Object(receipt));
    write(
        &root.join("config/agentdesk.yaml"),
        &json!({"cluster":{
        "instance_id":id,"gateway_preferred_instance_id":id},
        "server":{"host":"127.0.0.1","port":18791},"mcp_servers":{},
        "memory":{"backend":"file","mcp":{"endpoint":""}}}),
    );
    let mut profiles = serde_json::Map::new();
    let mut primary = serde_json::Map::new();
    let mut agents = serde_json::Map::new();
    for provider in ["claude", "codex"] {
        let name = format!("test-{provider}");
        profiles.insert(name.clone(), json!({"provider":provider,"home":root.join(format!("home/.adk/profiles/{provider}/{name}")),"env":{}}));
        primary.insert(provider.into(), json!(name));
        agents.insert(
            provider.into(),
            json!({"display_name":provider,"provider":provider,"auth_profile":name}),
        );
    }
    write(
        &root.join("config/org.yaml"),
        &json!({"version":1,"agents":agents,"provider_auth_profiles":profiles,"provider_auth_primary_profiles":primary}),
    );
    (temp, root)
}

pub fn command(root: &Path) -> Command {
    let manifest = read_json(&root.join("manifest.json")).unwrap();
    let config = read_json(&root.join("config/agentdesk.yaml")).unwrap();
    let env = environment::Environment::load(root, &manifest, &config).unwrap();
    let mut command = Command::new(std::env::current_exe().unwrap());
    command.env_clear().envs(&env.required).envs(&env.flags);
    for key in ["POSTGRES_TEST_DATABASE_URL_BASE", "AGENTDESK_REQUIRE_PG"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    command
}

pub fn assert_single_test_passed(output: &std::process::Output, selector: &str) {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let executed = format!("test {selector} ... ok");
    let summary = "test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured;";
    assert!(
        output.status.success()
            && stdout.lines().any(|line| line == executed)
            && stdout
                .lines()
                .filter(|line| line.starts_with(summary))
                .count()
                == 1,
        "expected exactly one successful test {selector}\n{stdout}\n{stderr}"
    );
}

pub(crate) fn write(path: &Path, value: &Value) {
    std::fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

/// A declared agy-headless instance under `<home>/.adk/dev/instances` with a fake agy CLI
/// that appends `ADK_CHILD <first arg>` and its env for every launch.
pub fn agy_fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let (temp, root, home) = super::login_test_support::claude_login_fixture(false);
    let login = home.join(".adk/dev/agy-login");
    let cli = home.join("agy-bin/agy");
    std::fs::create_dir_all(&login).unwrap();
    std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
    let capture = root.join("runtime/agy-env");
    std::fs::write(
        &cli,
        format!(
            "#!/bin/sh\n{{ echo \"ADK_CHILD $1\"; /usr/bin/env; }} >> {}\n[ \"$1\" = --version ] && echo 1.0.0\nexit 0\n",
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
