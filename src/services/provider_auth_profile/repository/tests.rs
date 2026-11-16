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
    fs::write(&path, "version: 1\ncustom_root: keep\nagents:\n  coder:\n    display_name: Coder\n    provider: codex\n    custom_agent: keep\n    memory: {backend: file, recall_timeout_ms: 123}\n    recovery: {enabled: false, future_recovery: keep}\nchannels:\n  by_id:\n    '42':\n      agent: coder\n      custom_channel: keep\n  by_name:\n    enabled: true\n    mappings: {}\nprovider_auth_fallbacks:\n  codex:\n    fallback_profile: default\n").unwrap();
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
        loaded
            .bindings
            .iter()
            .find(|binding| binding.channel_id.as_deref() == Some("42"))
            .unwrap()
            .channel_profile
            .as_deref(),
        Some("work")
    );
    assert_eq!(loaded.primary_profiles["codex"], "work");
    let content = fs::read(&path).unwrap();
    let yaml: serde_yaml::Value = serde_yaml::from_slice(&content).unwrap();
    assert_eq!(yaml["custom_root"], "keep");
    assert_eq!(yaml["agents"]["coder"]["custom_agent"], "keep");
    assert_eq!(yaml["agents"]["coder"]["memory"]["recall_timeout_ms"], 123);
    assert_eq!(
        yaml["agents"]["coder"]["recovery"]["future_recovery"],
        "keep"
    );
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
    assert!(loaded.profiles.is_empty());
    assert_eq!(
        loaded
            .bindings
            .iter()
            .find(|binding| binding.channel_id.as_deref() == Some("42"))
            .unwrap()
            .channel_profile
            .as_deref(),
        Some("default")
    );
    fs::write(&path, "agents: [broken").unwrap();
    assert!(read_org_schema(&path).is_err());
    drop(home_guard);
    drop(runtime);
}

#[test]
fn auth_projection_has_no_document_escape_or_recovery_publication() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = |path| std::fs::read_to_string(root.join(path)).unwrap();
    let catalog = source("src/services/org_catalog.rs");
    let auth_reader = catalog
        .split("fn read_auth_catalog_at(")
        .nth(1)
        .unwrap()
        .split("pub(crate) fn load_runtime_auth_catalog")
        .next()
        .unwrap();
    assert!(auth_reader.contains("document::read_document_at(path)"));
    for forbidden in ["load_runtime", "install_catalog", "clear_catalog"] {
        assert!(
            !auth_reader.contains(forbidden),
            "auth reader invokes {forbidden}"
        );
    }
    let document = source("src/services/org_catalog/document.rs");
    let decoder = document
        .split("fn read_document_at(")
        .nth(1)
        .unwrap()
        .split("trait RecoveryEffects")
        .next()
        .unwrap();
    for forbidden in ["load_runtime", "install_catalog", "clear_catalog"] {
        assert!(!decoder.contains(forbidden), "decoder invokes {forbidden}");
    }
    let projection = source("src/services/org_catalog/auth_projection.rs");
    for name in ["AuthCatalogSnapshot", "ProfileBindingInput"] {
        let fields = projection
            .split(&format!("struct {name} {{"))
            .nth(1)
            .unwrap()
            .split('}')
            .next()
            .unwrap();
        assert!(
            !fields.contains("OrgSchema") && !fields.contains("OrgDocument"),
            "{name} exposes the document"
        );
    }
    for path in [
        "src/services/org_catalog.rs",
        "src/services/org_catalog/document.rs",
        "src/services/org_catalog/auth_projection.rs",
        "src/services/provider_auth_profile/queries.rs",
    ] {
        assert!(
            !source(path).contains("services::discord"),
            "{path} delegates back to Discord"
        );
    }
    let queries = source("src/services/provider_auth_profile/queries.rs");
    assert_eq!(queries.matches("load_runtime_auth_catalog()").count(), 4);
}

#[test]
fn projected_bindings_preserve_primary_overrides_unknown_providers_and_dangling_channels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("org.yaml");
    fs::write(&path, "version: 1\nagents:\n  future:\n    display_name: Future\n    provider: future-provider\n    auth_profile: '  default  '\n  coder:\n    display_name: Coder\n    provider: CODEX\nchannels:\n  by_id:\n    '1': {agent: future, auth_profile: '   '}\n    '2': {agent: absent, auth_profile: ' default '}\n    '3': {agent: coder}\nprovider_auth_primary_profiles: {codex: default}\n").unwrap();
    let snapshot = read_org_schema(&path).unwrap().unwrap();
    let bindings =
        crate::services::provider_auth_profile::queries::list_profile_bindings_from_snapshot(
            snapshot,
        );
    assert_eq!(bindings.len(), 5);
    let agent = bindings
        .iter()
        .find(|b| b.agent_id == "future" && b.channel_id.is_none())
        .unwrap();
    assert_eq!(agent.provider, "future-provider");
    assert_eq!(agent.profile_id, "default");
    assert!(agent.is_explicit);
    let channel = bindings
        .iter()
        .find(|b| b.channel_id.as_deref() == Some("1"))
        .unwrap();
    assert_eq!(channel.provider, "unknown");
    assert!(channel.is_explicit);
    let dangling = bindings
        .iter()
        .find(|b| b.channel_id.as_deref() == Some("2"))
        .unwrap();
    assert_eq!(dangling.provider, "unknown");
    assert!(dangling.is_explicit);
    let inherited = bindings
        .iter()
        .find(|b| b.channel_id.as_deref() == Some("3"))
        .unwrap();
    assert_eq!(inherited.provider, "codex");
    assert!(!inherited.is_explicit);
}

#[test]
fn runtime_queries_reject_recovery_invalid_while_auth_read_keeps_the_projection() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let path = crate::runtime_layout::org_schema_path(runtime.path());
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    let content = "version: 1\nagents:\n  coder:\n    display_name: Coder\n    provider: codex\n    recovery: {enabled: true, fallback_agent_id: absent}\nchannels:\n  by_id:\n    '42': {agent: coder}\nprovider_auth_primary_profiles: {codex: default}\nprovider_auth_fallbacks:\n  codex: {enabled: false}\n";
    fs::write(&path, content).unwrap();
    let auth = read_org_schema(&path).unwrap().unwrap();
    assert_eq!(auth.bindings.len(), 2);
    assert_eq!(auth.primary_profiles["codex"], "default");
    assert!(!auth.fallback_policies["codex"].enabled);
    use crate::services::provider_auth_profile::queries;
    assert!(queries::provider_auth_catalog().is_empty());
    assert!(queries::provider_auth_primary_profiles().is_empty());
    assert!(queries::provider_auth_fallback_policies().is_empty());
    assert!(queries::list_profile_bindings().is_empty());
    fs::write(
        &path,
        content.replace("enabled: true, fallback_agent_id: absent", "enabled: false"),
    )
    .unwrap();
    assert_eq!(
        queries::provider_auth_primary_profiles()["codex"],
        "default"
    );
    assert!(!queries::provider_auth_fallback_policies()["codex"].enabled);
    assert_eq!(queries::list_profile_bindings().len(), 2);
}
