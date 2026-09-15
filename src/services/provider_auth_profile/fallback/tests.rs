use super::*;

fn catalog(provider: &str) -> HashMap<String, ProviderAuthProfileDef> {
    ["a", "b", "c"]
        .into_iter()
        .map(|id| {
            (
                id.into(),
                ProviderAuthProfileDef {
                    provider: provider.into(),
                    ..ProviderAuthProfileDef::default()
                },
            )
        })
        .collect()
}

#[test]
fn every_isolated_provider_works_without_configuration() {
    for id in super::super::EXTRA_ACCOUNT_PROVIDER_IDS {
        let provider = intern_provider(id).unwrap();
        let mut catalog = catalog(id);
        catalog.insert(
            "foreign".into(),
            ProviderAuthProfileDef {
                provider: "gemini".into(),
                ..Default::default()
            },
        );
        assert_eq!(
            FallbackPolicy::default().candidates(&provider, "b", &catalog),
            ["b", "a", "c"]
        );
        assert_eq!(
            FallbackPolicy::default().candidates(&provider, "default", &catalog),
            ["default", "a", "b", "c"]
        );
    }
}

#[test]
fn explicit_fallback_then_priority_then_remainder_are_deduplicated() {
    let mut policy = FallbackPolicy {
        fallback_profile: Some("c".into()),
        priority: vec!["c".into(), "b".into(), "default".into()],
        ..Default::default()
    };
    let catalog = catalog("codex");
    assert_eq!(
        policy.candidates(&ProviderKind::Codex, "a", &catalog),
        ["a", "c", "b", "default"]
    );
    policy.include_remaining = false;
    policy.priority.clear();
    assert_eq!(
        policy.candidates(&ProviderKind::Codex, "a", &catalog),
        ["a", "c"]
    );
    policy.enabled = false;
    assert_eq!(
        policy.candidates(&ProviderKind::Codex, "a", &catalog),
        ["a"]
    );
}

#[test]
fn invalid_or_cross_provider_policy_fails_closed() {
    let catalog = catalog("codex");
    let mut policy = FallbackPolicy {
        fallback_profile: Some("typo".into()),
        ..Default::default()
    };
    assert!(policy.validate(&ProviderKind::Codex, &catalog).is_err());
    policy.fallback_profile = Some("a".into());
    assert!(policy.validate(&ProviderKind::Claude, &catalog).is_err());
    assert!(policy.validate(&ProviderKind::Gemini, &catalog).is_err());
    policy.cooldown_secs = 0;
    assert!(policy.validate(&ProviderKind::Codex, &catalog).is_err());
    assert!(serde_yaml::from_str::<FallbackPolicy>("prioritty: [a]").is_err());
}

#[test]
fn exhaustion_switches_before_launch_and_unknown_usage_is_eligible() {
    let mut router = Router::default();
    let candidates = vec!["a".into(), "b".into(), "c".into()];
    assert_eq!(
        router.select("codex", 1, &candidates, 100, |id| id != "a"),
        "b"
    );
    // Healthy sticky account avoids crossing homes on each new request.
    assert_eq!(router.select("codex", 1, &candidates, 101, |_| true), "b");
    assert_eq!(router.select("claude", 1, &candidates, 101, |_| true), "a");
    assert_eq!(router.select("codex", 2, &candidates, 101, |_| true), "a");
}

#[test]
fn retry_chain_is_bounded_even_after_cooldown_expires() {
    let mut router = Router::default();
    let candidates = vec!["a".into(), "b".into(), "c".into()];
    assert_eq!(router.select("qwen", 1, &candidates, 100, |_| true), "a");
    assert_eq!(
        router.fail("qwen", 1, 42, 1, 100, |_| true),
        Some(("a".into(), "b".into()))
    );
    assert_eq!(router.select("qwen", 1, &candidates, 102, |_| true), "b");
    assert_eq!(
        router.fail("qwen", 1, 42, 1, 102, |_| true),
        Some(("b".into(), "c".into()))
    );
    assert_eq!(router.select("qwen", 1, &candidates, 104, |_| true), "c");
    assert_eq!(router.fail("qwen", 1, 42, 1, 104, |_| true), None);
    // A different user request can use the now recovered account.
    assert_eq!(
        router.fail("qwen", 1, 43, 1, 106, |_| true),
        Some(("c".into(), "a".into()))
    );
}

#[test]
fn failed_account_is_cooled_down_across_channels_but_not_providers() {
    let mut router = Router::default();
    let candidates = vec!["a".into(), "b".into()];
    router.select("grok", 1, &candidates, 100, |_| true);
    router.fail("grok", 1, 42, 300, 100, |_| true);
    assert_eq!(router.select("grok", 2, &candidates, 101, |_| true), "b");
    assert_eq!(router.select("claude", 2, &candidates, 101, |_| true), "a");
    assert_eq!(router.select("grok", 3, &candidates, 401, |_| true), "a");
}

#[test]
fn exhausted_or_invalid_backups_stop_and_policy_removal_resets_selection() {
    let mut router = Router::default();
    let candidates = vec!["a".into(), "b".into()];
    router.select("opencode", 1, &candidates, 100, |_| true);
    assert_eq!(router.fail("opencode", 1, 42, 300, 100, |_| false), None);
    assert_eq!(
        router.select("opencode", 1, &["b".into()], 101, |_| true),
        "b"
    );
    assert_eq!(router.fail("opencode", 1, 42, 300, 101, |_| true), None);
}
