use super::*;
use crate::domain::provider_auth_profile::ProfileOverrides;

#[test]
fn normalized_selection_inputs_preserve_launch_and_fallback_results() {
    let runtime = crate::config::TestRuntimeRootGuard::new();
    let home = tempfile::tempdir().unwrap();
    let home_path = std::fs::canonicalize(home.path()).unwrap();
    let home_guard =
        crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock("HOME", &home_path);
    let mut schema = parse_org_schema(
        "version: 1\nagents:\n  coder:\n    display_name: Coder\n    provider: codex\nchannels:\n  by_id:\n    '61018514':\n      agent: coder\n",
    ).unwrap();
    let catalog: HashMap<_, _> = ["a", "b", "c"]
        .into_iter()
        .map(|id| {
            let path = home_path.join(".adk/profiles/codex").join(id);
            std::fs::create_dir_all(&path).unwrap();
            (
                id.to_string(),
                ProviderAuthProfileDef {
                    provider: "codex".into(),
                    home: Some(path.display().to_string()),
                    ..Default::default()
                },
            )
        })
        .collect();
    let policies = [
        FallbackPolicy::default(),
        FallbackPolicy {
            enabled: false,
            ..Default::default()
        },
        FallbackPolicy {
            fallback_profile: Some("c".into()),
            priority: vec!["b".into(), "default".into(), "c".into()],
            include_remaining: false,
            ..Default::default()
        },
    ];
    let overrides = [
        None,
        Some(""),
        Some(" \t"),
        Some(" a "),
        Some("b"),
        Some(" default "),
    ];
    let normalized = [None, None, None, Some("a"), Some("b"), Some("default")];
    for primary in ["default", "c"] {
        schema.provider_auth_primary_profiles =
            Some(HashMap::from([("codex".into(), primary.into())]));
        for (channel_index, channel) in overrides.into_iter().enumerate() {
            for (agent_index, agent) in overrides.into_iter().enumerate() {
                schema
                    .channels
                    .as_mut()
                    .unwrap()
                    .by_id
                    .as_mut()
                    .unwrap()
                    .get_mut("61018514")
                    .unwrap()
                    .auth_profile = channel.map(str::to_string);
                schema.agents.get_mut("coder").unwrap().auth_profile = agent.map(str::to_string);
                let inputs = ProfileOverrides::new(channel, agent);
                assert_eq!(
                    (inputs.channel, inputs.agent),
                    (normalized[channel_index], normalized[agent_index])
                );
                let expected = normalized[channel_index]
                    .or(normalized[agent_index])
                    .unwrap_or(primary);
                for (policy_index, policy) in policies.iter().enumerate() {
                    schema
                        .provider_auth_fallbacks
                        .insert("codex".into(), policy.clone());
                    let (overlay, route) = launch_overlay_parts(
                        Some(&crate::services::org_catalog::AuthCatalogSnapshot::from(
                            &schema,
                        )),
                        &ProviderKind::Codex,
                        Some(61018514),
                        Some("coder"),
                        &catalog,
                    )
                    .unwrap();
                    assert_eq!(overlay.profile_id, expected);
                    let (channel_id, actual_policy) = route.unwrap();
                    assert_eq!(channel_id, 61018514);
                    let expected_candidates: Vec<&str> = match (policy_index, expected) {
                        (0, "default") => vec!["default", "a", "b", "c"],
                        (0, "a") => vec!["a", "b", "c"],
                        (0, "b") => vec!["b", "a", "c"],
                        (0, "c") => vec!["c", "a", "b"],
                        (1, value) => vec![value],
                        (2, "default") => vec!["default", "c", "b"],
                        (2, "a") => vec!["a", "c", "b", "default"],
                        (2, "b") => vec!["b", "c", "default"],
                        (2, "c") => vec!["c", "b", "default"],
                        _ => unreachable!(),
                    };
                    assert_eq!(
                        actual_policy.candidates(
                            &ProviderKind::Codex,
                            &overlay.profile_id,
                            &catalog
                        ),
                        expected_candidates
                    );
                }
            }
        }
    }
    // A stale channel binding cannot override the agent selected for this launch.
    schema.agents.insert(
        "other".into(),
        serde_yaml::from_str("display_name: Other\nauth_profile: a").unwrap(),
    );
    let (profile, pinned) = configured_launch_profile(
        Some(&crate::services::org_catalog::AuthCatalogSnapshot::from(
            &schema,
        )),
        &ProviderKind::Codex,
        Some(61018514),
        Some("other"),
    )
    .unwrap();
    assert_eq!(profile.as_deref(), Some("a"));
    assert!(!pinned);
    // Named typos must fail before any fallback can make the launch look successful.
    schema.agents.get_mut("other").unwrap().auth_profile = Some("missing".into());
    assert!(
        launch_overlay_parts(
            Some(&crate::services::org_catalog::AuthCatalogSnapshot::from(
                &schema
            )),
            &ProviderKind::Codex,
            Some(61018514),
            Some("other"),
            &catalog
        )
        .unwrap_err()
        .contains("unknown auth_profile")
    );
    assert!(
        launch_overlay_parts(
            Some(&crate::services::org_catalog::AuthCatalogSnapshot::from(
                &schema
            )),
            &ProviderKind::Codex,
            None,
            Some("other"),
            &HashMap::new()
        )
        .is_err()
    );
    assert_eq!(
        launch_overlay_parts(None, &ProviderKind::Codex, None, None, &HashMap::new())
            .unwrap()
            .0
            .profile_id,
        "default"
    );
    drop(home_guard);
    drop(runtime);
}

#[test]
fn legacy_auth_paths_reexport_the_same_items() {
    use crate::services::discord::org_writer;
    use crate::services::provider_auth_profile::repository;
    let write: fn(&std::path::Path, &str, &str) -> Result<(), String> =
        repository::set_provider_primary_profile_at;
    assert!(std::ptr::fn_addr_eq(
        write,
        org_writer::set_provider_primary_profile_at as fn(_, _, _) -> _
    ));
    let channel_write: fn(&std::path::Path, &str, Option<&str>) -> Result<(), String> =
        repository::set_channel_auth_profile_at;
    assert!(std::ptr::fn_addr_eq(
        channel_write,
        org_writer::set_channel_auth_profile_at as fn(_, _, _) -> _
    ));
    let read: fn() -> HashMap<String, ProviderAuthProfileDef> =
        crate::services::provider_auth_profile::queries::provider_auth_catalog;
    assert!(std::ptr::fn_addr_eq(
        read,
        provider_auth_catalog as fn() -> _
    ));
    let value: crate::domain::provider_auth_profile::ProviderAuthOverlay =
        crate::services::provider_auth_profile::ProviderAuthOverlay::default_for(
            ProviderKind::Codex,
        );
    assert!(value.is_default());
    let policy: crate::domain::provider_auth_profile::FallbackPolicy = FallbackPolicy::default();
    assert!(policy.enabled && policy.include_remaining);
}
