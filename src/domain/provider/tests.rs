use super::{
    ProviderCatalogEntry, ProviderKind, derived_counterpart_ids, provider_catalog,
    public_provider_catalog,
};

#[test]
fn provider_kind_value_contract() {
    let known = [
        (ProviderKind::Claude, "claude", "provider_claude"),
        (ProviderKind::Codex, "codex", "provider_codex"),
        (ProviderKind::Gemini, "gemini", "provider_gemini"),
        (ProviderKind::OpenCode, "opencode", "provider_opencode"),
        (ProviderKind::Qwen, "qwen", "provider_qwen"),
        (ProviderKind::Grok, "grok", "provider_grok"),
        (
            ProviderKind::Antigravity,
            "antigravity",
            "provider_antigravity",
        ),
    ];
    for (kind, id, check_id) in known {
        assert_eq!(kind.as_str(), id);
        assert_eq!(kind.doctor_check_id(), check_id);
        assert!(kind.is_supported());
    }
    let unknown = ProviderKind::Unsupported(" Future-CLI ".into());
    assert_eq!(unknown.as_str(), " Future-CLI ");
    assert_eq!(unknown.doctor_check_id(), "provider_unsupported");
    assert!(!unknown.is_supported());
}

#[test]
fn provider_catalog_names_aliases_and_cli_order_contract() {
    let expected = [
        ("claude", "claude (Anthropic)", &[][..]),
        ("codex", "codex (OpenAI)", &[][..]),
        ("gemini", "gemini (Google)", &[][..]),
        ("opencode", "opencode (OpenCode)", &[][..]),
        ("qwen", "qwen (Alibaba)", &[][..]),
        ("grok", "grok (xAI)", &[][..]),
        ("antigravity", "antigravity (agy)", &["agy"][..]),
    ];
    let actual = provider_catalog()
        .iter()
        .map(|entry| (entry.id, entry.cli_init_label, entry.aliases))
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
}

#[test]
fn provider_counterpart_order_contract() {
    let expected = [
        (
            "claude",
            ["codex", "antigravity", "gemini", "grok", "opencode", "qwen"],
        ),
        (
            "codex",
            [
                "claude",
                "antigravity",
                "gemini",
                "grok",
                "opencode",
                "qwen",
            ],
        ),
        (
            "gemini",
            ["codex", "antigravity", "claude", "grok", "opencode", "qwen"],
        ),
        (
            "opencode",
            ["codex", "antigravity", "claude", "gemini", "grok", "qwen"],
        ),
        (
            "qwen",
            [
                "codex",
                "antigravity",
                "claude",
                "gemini",
                "grok",
                "opencode",
            ],
        ),
        (
            "grok",
            [
                "codex",
                "antigravity",
                "claude",
                "gemini",
                "opencode",
                "qwen",
            ],
        ),
        (
            "antigravity",
            ["codex", "claude", "gemini", "grok", "opencode", "qwen"],
        ),
    ];
    for (id, counterparts) in expected {
        assert_eq!(derived_counterpart_ids(id), counterparts, "{id}");
    }
}

#[test]
fn public_catalog_serde_golden_contract() {
    let expected = include_str!("../../../tests/fixtures/provider/public_catalog.json").trim();
    let catalog = public_provider_catalog();
    assert_eq!(serde_json::to_string(&catalog).unwrap(), expected);
    let decoded: Vec<ProviderCatalogEntry> = serde_json::from_str(expected).unwrap();
    assert_eq!(decoded, catalog);
}
