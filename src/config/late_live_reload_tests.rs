use super::*;

fn config(on: bool) -> Config {
    serde_json::from_value(serde_json::json!({"server": {},
        "agents": [{"id": "fixture", "name": "Fixture", "channels": {"claude": {"id": "41", "runtime": "tui"}}}],
        "tui_o": {"writer": {"channels": [41], "late_adopt": on}}
    })).unwrap()
}

#[test]
fn default_off_omits_key_and_config_changes_do_not_replace_boot_capability() {
    let config: Config = serde_json::from_value(serde_json::json!({"server": {}})).unwrap();
    assert!(!config.tui_o.as_ref().is_some_and(|c| c.writer.late_adopt));
    assert!(
        !serde_yaml::to_string(&config)
            .unwrap()
            .contains("late_adopt")
    );
    let off = crate::services::tui_o::channel_policy::BootChannels::validate(&self::config(false))
        .unwrap();
    let on = crate::services::tui_o::channel_policy::BootChannels::validate(&self::config(true))
        .unwrap();
    assert!(!off.late_adopt_capable());
    assert!(on.late_adopt_capable());
    assert_eq!(off, on, "live OFF/ON must not reinstall the immutable BOOT");
}

#[test]
fn restart_warning_sticks_to_installed_boot_capability_across_reload() {
    const CHILD: &str = "ADK_TEST_LATE_RELOAD_CAPABILITY";
    let Some(role) = std::env::var_os(CHILD) else {
        for role in ["off", "on"] {
            let root = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "config_live_reload::late_live_reload_tests::restart_warning_sticks_to_installed_boot_capability_across_reload", "--nocapture"])
                .env(CHILD, role)
                .env("AGENTDESK_ROOT_DIR", root.path()).output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "{role}: {stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
        }
        return;
    };
    let boot_on = role == "on";
    let boot_config = config(boot_on);
    crate::services::tui_o::channel_policy::install(&boot_config).unwrap();
    install(boot_config);
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("fixture.yaml");
    for requested in [true, true, false, true] {
        std::fs::write(&path, serde_yaml::to_string(&config(requested)).unwrap()).unwrap();
        let ReloadOutcome::Applied { restart_required } = reload_from_path(&path) else {
            panic!("valid reload rejected");
        };
        assert_eq!(
            restart_required.contains(&"tui_o.writer.late_adopt"),
            requested && !boot_on
        );
        assert_eq!(
            crate::services::tui_o::channel_policy::boot()
                .unwrap()
                .late_adopt_capable(),
            boot_on
        );
    }
}
