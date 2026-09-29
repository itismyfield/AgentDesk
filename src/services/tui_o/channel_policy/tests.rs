use super::*;

#[test]
fn ownership_uses_flag_membership_and_tui_kind_together() {
    let channels = BTreeSet::from([41]);
    for enabled in [false, true] {
        for (channel, selected) in [(41, true), (42, false)] {
            for (kind, tui) in [
                (Some(RuntimeHandoffKind::ClaudeTui), true),
                (Some(RuntimeHandoffKind::CodexTui), true),
                (Some(RuntimeHandoffKind::LegacyTmuxWrapper), false),
                (Some(RuntimeHandoffKind::ProcessBackend), false),
                (Some(RuntimeHandoffKind::ClaudeEAdapter), false),
                (None, false),
            ] {
                assert_eq!(
                    owns_output(enabled, &channels, channel, kind),
                    enabled && selected && tui
                );
                assert!(!owns_output(enabled, &BTreeSet::new(), channel, kind));
            }
        }
    }
}

#[test]
fn boot_membership_survives_reload_until_restart() {
    const CHILD: &str = "ADK_TEST_WRITER_BOOT_SNAPSHOT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "services::tui_o::channel_policy::tests::boot_membership_survives_reload_until_restart", "--nocapture"])
            .env(CHILD, "1")
            .output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        // A filter that matches nothing also exits 0; require the child to have run this test.
        assert!(stdout.contains("1 passed; 0 failed; 0 ignored"), "{stdout}");
        return;
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("agentdesk.yaml");
    let write = |channels: &str| {
        std::fs::write(&path, format!(
            "server: {{}}\nagents:\n  - id: fixture\n    name: Fixture\n    channels:\n      claude: {{id: '41', runtime: tui}}\n      codex: {{id: '42', runtime: tui}}\ntui_o:\n  writer:\n    channels: {channels}\n"
        )).unwrap();
    };
    write("[41]");
    let original = crate::config::load_from_path(&path).unwrap();
    install(&original).unwrap();
    crate::config_live_reload::install(original);
    write("[42]");
    for _ in 0..2 {
        let outcome = crate::config_live_reload::reload_from_path(&path);
        assert!(
            matches!(outcome, crate::config_live_reload::ReloadOutcome::Applied { restart_required } if restart_required.contains(&"tui_o.writer.channels"))
        );
        let snapshot = boot().unwrap();
        assert!(owns_output(
            true,
            snapshot.channels(),
            41,
            Some(RuntimeHandoffKind::ClaudeTui)
        ));
        assert!(!owns_output(
            true,
            snapshot.channels(),
            42,
            Some(RuntimeHandoffKind::CodexTui)
        ));
    }
    assert!(install(&crate::config::load_from_path(&path).unwrap()).is_err());
    write("[41]");
    assert!(
        matches!(crate::config_live_reload::reload_from_path(&path), crate::config_live_reload::ReloadOutcome::Applied { restart_required } if !restart_required.contains(&"tui_o.writer.channels"))
    );
    write("[0]");
    assert!(matches!(
        crate::config_live_reload::reload_from_path(&path),
        crate::config_live_reload::ReloadOutcome::Rejected { .. }
    ));
    assert!(owns_output(
        true,
        boot().unwrap().channels(),
        41,
        Some(RuntimeHandoffKind::ClaudeTui)
    ));
}
