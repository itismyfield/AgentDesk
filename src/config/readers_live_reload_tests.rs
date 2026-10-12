use super::*;
use crate::services::tui_o::channel_policy;

#[test]
fn readers_s1b_validated_reload_compares_installed_boot_in_both_directions() {
    const CHILD: &str = "ADK_TEST_READERS_S1B_RELOAD_CHILD";
    let Some(initial) = std::env::var_os(CHILD) else {
        for initial in ["off", "on"] {
            let root = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "config_live_reload::readers_tests::readers_s1b_validated_reload_compares_installed_boot_in_both_directions",
                    "--nocapture",
                ])
                .env(CHILD, initial)
                .env("AGENTDESK_ROOT_DIR", root.path())
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success(),
                "{initial}: {stdout}\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
        }
        return;
    };
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("agentdesk.yaml");
    let write = |dormant: &str| {
        std::fs::write(
            &path,
            format!(
                "server: {{}}\nagents:\n  - id: fixture\n    name: Fixture\n    channels:\n      claude: {{id: '41', runtime: tui}}\n      codex: {{id: '42', runtime: tui}}\ntui_o:\n  writer:\n    channels: [41]\n    dormant_channels: {dormant}\n"
            ),
        )
        .unwrap();
    };
    let on = initial == "on";
    let original_list = if on { "[41]" } else { "[]" };
    let changed_list = if on { "[]" } else { "[41]" };
    write(original_list);
    let original = crate::config::load_from_path(&path).unwrap();
    channel_policy::install(&original).unwrap();
    let installed = channel_policy::BootChannels::validate(&original).unwrap();
    install(original);
    write(changed_list);
    for _ in 0..2 {
        let outcome = reload_from_path(&path);
        assert!(
            matches!(outcome, ReloadOutcome::Applied { restart_required } if restart_required.contains(&"tui_o.writer.dormant_channels")),
            "the same edit still needs restart while BOOT has the old policy"
        );
        let live = serde_json::to_value(&*current().unwrap()).unwrap();
        let expected = if on {
            None
        } else {
            Some(serde_json::json!([41]))
        };
        assert_eq!(
            live.pointer("/tui_o/writer/dormant_channels"),
            expected.as_ref(),
            "valid reload publishes the new config, without changing actor BOOT"
        );
        assert_eq!(channel_policy::boot().unwrap(), &installed);
    }
    write(original_list);
    assert!(
        matches!(reload_from_path(&path), ReloadOutcome::Applied { restart_required } if !restart_required.contains(&"tui_o.writer.dormant_channels")),
        "returning to installed policy clears restart-required"
    );
    for invalid in ["[0]", "[42]", "[-1]"] {
        let before = current().unwrap();
        write(invalid);
        assert!(matches!(
            reload_from_path(&path),
            ReloadOutcome::Rejected { .. }
        ));
        assert!(
            Arc::ptr_eq(&before, &current().unwrap()),
            "invalid dormant membership must preserve the validated live snapshot"
        );
        assert_eq!(channel_policy::boot().unwrap(), &installed);
    }
}
