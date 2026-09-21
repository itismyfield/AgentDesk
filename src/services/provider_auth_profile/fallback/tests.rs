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
        router
            .select("codex", 1, &candidates, 100, |id| id != "a")
            .as_deref(),
        Some("b")
    );
    // Healthy sticky account avoids crossing homes on each new request.
    assert_eq!(
        router
            .select("codex", 1, &candidates, 101, |_| true)
            .as_deref(),
        Some("b")
    );
    assert_eq!(
        router
            .select("claude", 1, &candidates, 101, |_| true)
            .as_deref(),
        Some("a")
    );
    assert_eq!(
        router
            .select("codex", 2, &candidates, 101, |_| true)
            .as_deref(),
        Some("a")
    );
}

#[test]
fn retry_chain_is_bounded_even_after_cooldown_expires() {
    let mut router = Router::default();
    let candidates = vec!["a".into(), "b".into(), "c".into()];
    assert_eq!(
        router
            .select("qwen", 1, &candidates, 100, |_| true)
            .as_deref(),
        Some("a")
    );
    assert_eq!(
        router.fail("qwen", 1, 42, 1, 100, |_| true),
        Some(("a".into(), "b".into()))
    );
    assert_eq!(
        router
            .select("qwen", 1, &candidates, 102, |_| true)
            .as_deref(),
        Some("b")
    );
    assert_eq!(
        router.fail("qwen", 1, 42, 1, 102, |_| true),
        Some(("b".into(), "c".into()))
    );
    assert_eq!(
        router
            .select("qwen", 1, &candidates, 104, |_| true)
            .as_deref(),
        Some("c")
    );
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
    assert_eq!(
        router
            .select("grok", 2, &candidates, 101, |_| true)
            .as_deref(),
        Some("b")
    );
    assert_eq!(
        router
            .select("claude", 2, &candidates, 101, |_| true)
            .as_deref(),
        Some("a")
    );
    assert_eq!(
        router
            .select("grok", 3, &candidates, 401, |_| true)
            .as_deref(),
        Some("a")
    );
}

#[test]
fn exhausted_or_invalid_backups_stop_and_policy_removal_resets_selection() {
    let mut router = Router::default();
    let candidates = vec!["a".into(), "b".into()];
    router.select("opencode", 1, &candidates, 100, |_| true);
    assert_eq!(router.fail("opencode", 1, 42, 300, 100, |_| false), None);
    assert_eq!(
        router
            .select("opencode", 1, &["b".into()], 101, |_| true)
            .as_deref(),
        Some("b")
    );
    assert_eq!(router.fail("opencode", 1, 42, 300, 101, |_| true), None);
}

#[test]
fn no_eligible_account_never_falls_back_to_a_pressured_or_cooled_primary() {
    let mut router = Router::default();
    let candidates = vec!["a".into(), "b".into()];
    assert_eq!(
        router.select("codex", 71, &candidates, 100, |_| false),
        None
    );
    assert_eq!(router.select("codex", 71, &[], 100, |_| true), None);
    router.select("codex", 71, &candidates, 101, |_| true);
    router.fail("codex", 71, 42, 300, 102, |_| true);
    router.fail("codex", 71, 42, 300, 103, |_| true);
    assert_eq!(router.select("codex", 71, &candidates, 104, |_| true), None);
    assert_eq!(
        router
            .select("codex", 71, &candidates, 403, |_| true)
            .as_deref(),
        Some("b")
    );
}

#[test]
fn session_launch_uses_configured_primary_when_all_accounts_are_pressured() {
    let mut router = Router::default();
    let candidates = vec!["work".into(), "backup".into()];
    assert_eq!(
        router.select_for_launch("claude", 72, &candidates, 100, |id| id == "backup"),
        Some("backup".into())
    );
    assert_eq!(
        router.select_for_launch("claude", 72, &candidates, 101, |_| false),
        Some("work".into())
    );
    // Failure must refer to the account actually launched, not the old sticky backup.
    assert_eq!(
        router.fail("claude", 72, 42, 300, 102, |id| id == "backup"),
        Some(("work".into(), "backup".into()))
    );
    assert_eq!(
        router.select_for_launch("claude", 73, &["default".into()], 101, |_| false),
        Some("default".into())
    );
    assert_eq!(
        router.select_for_launch("claude", 74, &[], 101, |_| true),
        None
    );
}

#[test]
fn session_launch_during_cooldown_preserves_bounded_retries() {
    let mut router = Router::default();
    let candidates = vec!["a".into(), "b".into()];
    router.select_for_launch("claude", 75, &candidates, 100, |_| true);
    assert_eq!(
        router.fail("claude", 75, 42, 300, 101, |_| true),
        Some(("a".into(), "b".into()))
    );
    assert_eq!(router.fail("claude", 75, 42, 300, 102, |_| true), None);
    assert_eq!(
        router.select_for_launch("claude", 75, &candidates, 103, |_| true),
        Some("a".into())
    );
    assert_eq!(router.fail("claude", 75, 42, 300, 104, |_| true), None);
    // Neither opening the session nor expiry of cooldown permits replaying this request.
    assert_eq!(router.fail("claude", 75, 42, 300, 405, |_| true), None);
    assert_eq!(
        router.fail("claude", 75, 43, 300, 406, |_| true),
        Some(("a".into(), "b".into()))
    );
}
