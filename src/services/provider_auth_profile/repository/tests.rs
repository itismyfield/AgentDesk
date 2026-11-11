use super::*;

#[test]
fn repository_preserves_org_extensions_and_rejects_unknown_profiles_without_writing() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let directory = tempfile::tempdir().unwrap();
    let home_guard = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
        "HOME",
        directory.path(),
    );
    let path = directory.path().join("org.yaml");
    assert!(read_org_schema(&path).unwrap().is_none());
    fs::write(&path, "version: 1\ncustom_root: keep\nagents:\n  coder:\n    display_name: Coder\n    provider: codex\n    custom_agent: keep\nchannels:\n  by_id:\n    '42':\n      agent: coder\n      custom_channel: keep\n  by_name:\n    enabled: true\n    mappings: {}\nprovider_auth_fallbacks:\n  codex:\n    fallback_profile: default\n").unwrap();
    let profile = ProviderAuthProfileDef {
        provider: "codex".into(),
        home: Some(
            directory
                .path()
                .join(".adk/profiles/codex/work")
                .display()
                .to_string(),
        ),
        ..Default::default()
    };
    append_provider_auth_profile_at(&path, "work", profile).unwrap();
    set_channel_auth_profile_at(&path, "42", Some(" work ")).unwrap();
    set_provider_primary_profile_at(&path, "codex", "work").unwrap();
    let loaded = read_org_schema(&path).unwrap().unwrap();
    assert_eq!(
        loaded.channels.as_ref().unwrap().by_id.as_ref().unwrap()["42"]
            .auth_profile
            .as_deref(),
        Some("work")
    );
    assert_eq!(
        loaded.provider_auth_primary_profiles.as_ref().unwrap()["codex"],
        "work"
    );
    let content = fs::read(&path).unwrap();
    let yaml: serde_yaml::Value = serde_yaml::from_slice(&content).unwrap();
    assert_eq!(yaml["custom_root"], "keep");
    assert_eq!(yaml["agents"]["coder"]["custom_agent"], "keep");
    assert_eq!(yaml["channels"]["by_id"]["42"]["custom_channel"], "keep");
    assert_eq!(yaml["channels"]["by_name"]["enabled"], true);
    assert_eq!(
        yaml["provider_auth_fallbacks"]["codex"]["fallback_profile"],
        "default"
    );
    assert!(
        set_channel_auth_profile_at(&path, "42", Some("missing"))
            .unwrap_err()
            .contains("unknown auth_profile")
    );
    assert_eq!(fs::read(&path).unwrap(), content);
    assert!(remove_provider_auth_profile_at(&path, "work", "codex").is_err());
    assert_eq!(fs::read(&path).unwrap(), content);
    set_channel_auth_profile_at(&path, "42", Some(" default ")).unwrap();
    set_provider_primary_profile_at(&path, "codex", "default").unwrap();
    remove_provider_auth_profile_at(&path, "work", "codex").unwrap();
    let loaded = read_org_schema(&path).unwrap().unwrap();
    assert!(loaded.provider_auth_profiles.unwrap_or_default().is_empty());
    assert_eq!(
        loaded.channels.unwrap().by_id.unwrap()["42"]
            .auth_profile
            .as_deref(),
        Some("default")
    );
    fs::write(&path, "agents: [broken").unwrap();
    assert!(read_org_schema(&path).is_err());
    drop(home_guard);
    drop(runtime);
}
