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
    }
}
