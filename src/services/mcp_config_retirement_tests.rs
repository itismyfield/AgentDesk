use super::*;

fn isolated(run: impl FnOnce(&Path)) {
    let name = std::thread::current().name().unwrap().to_string();
    if std::env::var("ADK_PROVIDER_SYNC_TEST").as_deref() == Ok(&name) {
        run(&PathBuf::from(std::env::var_os("HOME").unwrap()));
        return;
    }
    let temp = tempfile::tempdir().unwrap();
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", &name, "--nocapture"])
        .env("ADK_PROVIDER_SYNC_TEST", &name)
        .env("HOME", temp.path())
        .env("USERPROFILE", temp.path())
        .env("AGENTDESK_ROOT_DIR", temp.path().join("runtime"))
        .env_remove("AGENTDESK_CONFIG")
        .env_remove("GEMINI_CLI_HOME")
        .env_remove("MEMENTO_ACCESS_KEY")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn write(path: &Path, value: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, value).unwrap();
}

#[test]
fn retired_gemini_never_creates_settings_or_sync_state() {
    isolated(|home| {
        let mut config = Config::default();
        config.mcp_servers.insert(
            "fixture".into(),
            McpServerConfig {
                url: "http://fixture.test/mcp".into(),
                auth: None,
            },
        );
        let state = crate::runtime_layout::config_dir(&crate::config::runtime_root().unwrap())
            .join("gemini-mcp-sync-state.json");
        retired_gemini_sync(&config).unwrap();
        assert!(!home.join(".gemini/settings.json").exists());
        assert!(!state.exists());
    });
}

#[test]
fn retired_gemini_removes_only_recorded_servers_and_keeps_antigravity_bytes() {
    isolated(|home| {
        let settings = home.join(".gemini/settings.json");
        let state = crate::runtime_layout::config_dir(&crate::config::runtime_root().unwrap())
            .join("gemini-mcp-sync-state.json");
        let agy = [
            home.join(".gemini/config/mcp_config.json"),
            home.join(".gemini/antigravity-cli/settings.json"),
        ];
        for path in &agy {
            write(path, "{\"manual\":true}\n");
        }
        write(
            &settings,
            r#"{"mcpServers":{"managed":{"url":"http://old.test"},"manual":{"command":"user"}},"theme":"user"}"#,
        );
        write(&state, r#"{"managed_servers":["managed"]}"#);
        retired_gemini_sync(&Config::default()).unwrap();
        let value: Value =
            serde_json::from_str(&std::fs::read_to_string(&settings).unwrap()).unwrap();
        assert!(value["mcpServers"].get("managed").is_none());
        assert_eq!(value["mcpServers"]["manual"]["command"], "user");
        assert_eq!(value["theme"], "user");
        assert!(!state.exists());
        for path in &agy {
            assert_eq!(
                std::fs::read_to_string(path).unwrap(),
                "{\"manual\":true}\n"
            );
        }
        let bytes = std::fs::read(&settings).unwrap();
        retired_gemini_sync(&Config::default()).unwrap();
        assert_eq!(std::fs::read(&settings).unwrap(), bytes);
    });
}
