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

#[test]
fn provider_kind_catalog_helpers_preserve_known_alias_and_unknown_contract() {
    let known = [
        (
            ProviderKind::Claude,
            "Claude",
            "claude (Anthropic)",
            ["codex", "antigravity", "gemini", "grok", "opencode", "qwen"],
        ),
        (
            ProviderKind::Codex,
            "Codex",
            "codex (OpenAI)",
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
            ProviderKind::Gemini,
            "Gemini",
            "gemini (Google)",
            ["codex", "antigravity", "claude", "grok", "opencode", "qwen"],
        ),
        (
            ProviderKind::OpenCode,
            "OpenCode",
            "opencode (OpenCode)",
            ["codex", "antigravity", "claude", "gemini", "grok", "qwen"],
        ),
        (
            ProviderKind::Qwen,
            "Qwen Code",
            "qwen (Alibaba)",
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
            ProviderKind::Grok,
            "Grok",
            "grok (xAI)",
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
            ProviderKind::Antigravity,
            "Antigravity",
            "antigravity (agy)",
            ["codex", "claude", "gemini", "grok", "opencode", "qwen"],
        ),
    ];
    assert_eq!(
        ProviderKind::cli_init_labels(),
        known
            .iter()
            .map(|(_, _, label, _)| *label)
            .collect::<Vec<_>>()
    );
    for (index, (kind, display_name, label, counterparts)) in known.iter().enumerate() {
        let input = format!(" {} ", kind.as_str().to_ascii_uppercase());
        assert_eq!(ProviderKind::from_str(&input), Some(kind.clone()));
        assert_eq!(ProviderKind::from_str_or_unsupported(&input), *kind);
        assert_eq!(kind.display_name(), *display_name);
        assert_eq!(kind.cli_init_label(), *label);
        assert_eq!(
            ProviderKind::provider_for_cli_init_index(index),
            Some(kind.clone())
        );
        let preferred = kind.preferred_counterparts();
        assert_eq!(
            preferred
                .iter()
                .map(ProviderKind::as_str)
                .collect::<Vec<_>>(),
            *counterparts
        );
        assert_eq!(kind.counterpart().as_str(), counterparts[0]);
    }
    assert_eq!(
        ProviderKind::from_str(" AGY "),
        Some(ProviderKind::Antigravity)
    );
    assert_eq!(
        ProviderKind::Grok.stream_json_dialect(),
        Some(super::StreamJsonDialectId::Grok)
    );
    assert_eq!(
        ProviderKind::Antigravity.stream_json_dialect(),
        Some(super::StreamJsonDialectId::Agy)
    );
    assert_eq!(ProviderKind::Claude.stream_json_dialect(), None);
    assert_eq!(ProviderKind::provider_for_cli_init_index(7), None);
    assert_eq!(ProviderKind::provider_for_cli_init_index(usize::MAX), None);
    for raw in ["", " Future-CLI ", "claude-extra"] {
        assert_eq!(ProviderKind::from_str(raw), None);
        let unknown = ProviderKind::Unsupported(raw.trim().to_string());
        assert_eq!(ProviderKind::from_str_or_unsupported(raw), unknown);
        assert_eq!(unknown.display_name(), raw.trim());
        assert_eq!(unknown.cli_init_label(), "claude");
        assert!(unknown.preferred_counterparts().is_empty());
        assert_eq!(unknown.counterpart(), unknown);
    }
    let unsupported_known_id = ProviderKind::Unsupported("claude".into());
    assert_eq!(unsupported_known_id.display_name(), "claude");
    assert_eq!(unsupported_known_id.cli_init_label(), "claude");
    assert!(unsupported_known_id.preferred_counterparts().is_empty());
    assert_eq!(unsupported_known_id.counterpart(), unsupported_known_id);
}

#[test]
fn provider_tmux_name_preserves_sanitization_byte_budget_and_thread_suffix() {
    let cases = [
        ("", "AgentDesk-claude-"),
        ("Room:#1/a.b c_가", "AgentDesk-claude-Room--1-a-b-c_가"),
        (
            "01234567890123456789012345678901234567890123456789",
            "AgentDesk-claude-01234567890123456789012345678901234567890123",
        ),
        (
            "01234567890123456789012345678901234567890123456789-t1234567890123456789",
            "AgentDesk-claude-01234567890123456789012-t1234567890123456789",
        ),
        (
            "가나다라마바사아자차카타파하거",
            "AgentDesk-claude-가나다라마바사아자차카타파하",
        ),
        (
            "가나다라마바사아자차카타파하거-t1234567890123456789",
            "AgentDesk-claude-가나다라마바사-t1234567890123456789",
        ),
        (
            "01234567890123456789012345678901234567890123456789-tthread",
            "AgentDesk-claude-01234567890123456789012345678901234567890123",
        ),
    ];
    for (channel, expected) in cases {
        assert_eq!(
            ProviderKind::Claude.build_tmux_session_name(channel),
            expected
        );
    }
    assert_eq!(
        ProviderKind::Codex.build_tmux_session_name("dev-cdx"),
        "AgentDesk-codex-dev-cdx"
    );
    assert_eq!(
        ProviderKind::Antigravity.build_tmux_session_name("dev-ag"),
        "AgentDesk-antigravity-dev-ag"
    );
}
