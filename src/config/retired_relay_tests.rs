use super::*;

#[test]
fn retired_delivery_journal_keys_are_ignored_without_enabling_runtime_work() {
    for mode in ["legacy", "shadow", "authority", "future_mode"] {
        let yaml = format!(
            "delivery_journal_mode: {mode}\ndelivery_journal_cohort_percent: 100\ndelivery_journal_internal_channel_ids: [\"123\"]\n"
        );
        let runtime: RuntimeSettingsConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(runtime, RuntimeSettingsConfig::default());
        assert!(runtime.is_empty());
        let saved = serde_yaml::to_string(&runtime).unwrap();
        assert!(!saved.contains("delivery_journal"));
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retired-relay.yaml");
        save_to_path(&path, &Config::default()).unwrap();
        let mut document = std::fs::read_to_string(&path).unwrap();
        document.push_str(&format!(
            "\nruntime:\n{}",
            yaml.lines()
                .map(|line| format!("  {line}\n"))
                .collect::<String>()
        ));
        std::fs::write(&path, document).unwrap();
        let loaded = load_from_path(&path).expect("retired journal keys must not fail config load");
        assert!(loaded.runtime.is_empty());
    }
}

#[test]
fn retired_intake_settlement_key_is_ignored_without_enabling_runtime_work() {
    let declaration = concat!("intake_delivery", "_settlement:");
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut pending: Vec<_> = ["src", "policies", "dashboard/src", "agentdesk.example.yaml"]
        .iter()
        .map(|path| root.join(path))
        .collect();
    let mut declarations = Vec::new();
    while let Some(path) = pending.pop() {
        if path.is_dir() {
            pending.extend(
                std::fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path()),
            );
        } else if path != root.join("src/config/retired_relay_tests.rs")
            && std::fs::read_to_string(&path).is_ok_and(|text| text.contains(declaration))
        {
            declarations.push(path.strip_prefix(root).unwrap().to_owned());
        }
    }
    assert!(
        declarations.is_empty(),
        "retired intake key declared again: {declarations:?}"
    );
    for stage in ["off", "observe", "settle", "enforce", "future_stage"] {
        let yaml = format!("intake_delivery_settlement: {stage}\n");
        let runtime: RuntimeSettingsConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(runtime, RuntimeSettingsConfig::default());
        assert!(runtime.is_empty());
        assert!(
            !serde_yaml::to_string(&runtime)
                .unwrap()
                .contains("intake_delivery_settlement")
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retired-intake.yaml");
        save_to_path(&path, &Config::default()).unwrap();
        let mut document = std::fs::read_to_string(&path).unwrap();
        document.push_str(&format!("\nruntime:\n  {yaml}"));
        std::fs::write(&path, document).unwrap();
        let loaded = load_from_path(&path).expect("retired intake key must not fail config load");
        assert!(loaded.runtime.is_empty());
    }
}

#[test]
fn retired_session_audit_keys_are_ignored_without_enabling_runtime_work() {
    for enabled in ["true", "false", "future_value"] {
        let yaml = format!(
            "active_session_audit_enabled: {enabled}\nactive_session_audit_stale_secs: invalid\nactive_session_audit_max_candidates: invalid\n"
        );
        let runtime: RuntimeSettingsConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(runtime, RuntimeSettingsConfig::default());
        assert!(runtime.is_empty());
        assert!(
            !serde_yaml::to_string(&runtime)
                .unwrap()
                .contains("active_session_audit")
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retired-audit.yaml");
        save_to_path(&path, &Config::default()).unwrap();
        let mut document = std::fs::read_to_string(&path).unwrap();
        document.push_str("\nruntime:\n");
        for line in yaml.lines() {
            document.push_str(&format!("  {line}\n"));
        }
        std::fs::write(&path, document).unwrap();
        let loaded = load_from_path(&path).expect("retired audit keys must not fail config load");
        assert!(loaded.runtime.is_empty());
    }
}
