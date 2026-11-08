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
fn retired_hook_registry_and_rollout_cache_switches_are_ignored_on_load() {
    for value in ["false", "true"] {
        let yaml = format!(
            "tui_hook_registry_enabled: {value}\ncodex_rollout_index_cache_enabled: {value}\n"
        );
        let runtime: RuntimeSettingsConfig = serde_yaml::from_str(&yaml).unwrap();
        assert!(runtime.is_empty());
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retired-runtime-switches.yaml");
        save_to_path(&path, &Config::default()).unwrap();
        let mut document = std::fs::read_to_string(&path).unwrap();
        document.push_str("\nruntime:\n");
        for line in yaml.lines() {
            document.push_str(&format!("  {line}\n"));
        }
        std::fs::write(&path, document).unwrap();
        let loaded =
            load_from_path(&path).expect("retired runtime switches must not fail config load");
        assert!(loaded.runtime.is_empty());
        save_to_path(&path, &loaded).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("tui_hook_registry_enabled"));
        assert!(!saved.contains("codex_rollout_index_cache_enabled"));
    }
}

#[test]
fn retired_placeholder_panel_switches_are_ignored_on_load() {
    for value in ["false", "true"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("retired-placeholder-switches.yaml");
        save_to_path(&path, &Config::default()).unwrap();
        let mut document = std::fs::read_to_string(&path).unwrap();
        document.push_str(&format!(
            "\nplaceholder:\n  live_events_enabled: {value}\n  status_panel_v2_enabled: {value}\n  tool_activity_display: class\n"
        ));
        std::fs::write(&path, document).unwrap();
        let loaded =
            load_from_path(&path).expect("retired placeholder switches must not fail config load");
        assert_eq!(
            loaded.placeholder.tool_activity_display,
            ToolActivityDisplay::Class
        );
        save_to_path(&path, &loaded).unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(!saved.contains("live_events_enabled"), "{saved}");
        assert!(!saved.contains("status_panel_v2_enabled"), "{saved}");
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
