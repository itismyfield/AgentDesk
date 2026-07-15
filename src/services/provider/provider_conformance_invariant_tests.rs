use std::collections::HashSet;

use super::{ProviderCompactionAdapter, ProviderKind, provider_registry};

const READY_CAPTURE: &str = "Ready for input (type message + Enter)\n> ";
const BUSY_CAPTURE: &str = "working\nstill waiting for tool output";

#[test]
fn provider_exec_registry_conformance_invariant() {
    let mut provider_ids = HashSet::new();

    for entry in provider_registry() {
        assert!(
            provider_ids.insert(entry.id),
            "duplicate provider registry id: {}",
            entry.id
        );

        let provider = ProviderKind::from_str(entry.id).unwrap_or_else(|| {
            panic!("registry provider {} has no ProviderKind mapping", entry.id)
        });
        let execution_adapter = provider
            .execution_adapter()
            .unwrap_or_else(|| panic!("registry provider {} has no execution adapter", entry.id));

        assert_eq!(
            execution_adapter.provider_id(),
            entry.id,
            "{} execution adapter is wired to another provider",
            entry.id
        );
        assert_eq!(
            execution_adapter.supported_capabilities(),
            entry.capabilities,
            "{} declared capabilities do not match its execution adapter",
            entry.id
        );

        let compaction_adapter = provider
            .compaction_adapter()
            .unwrap_or_else(|| panic!("registry provider {} has no compaction adapter", entry.id));
        assert_eq!(
            compaction_adapter.provider_id(),
            entry.id,
            "{} compaction adapter is not provider-specific",
            entry.id
        );
        match compaction_adapter {
            ProviderCompactionAdapter::ClaudeEnvironment => {
                assert!(!provider.compact_env_vars(80).is_empty());
                assert!(provider.compact_cli_config(80, 100_000).is_empty());
            }
            ProviderCompactionAdapter::CodexCli => {
                assert!(provider.compact_env_vars(80).is_empty());
                assert!(!provider.compact_cli_config(80, 100_000).is_empty());
            }
            ProviderCompactionAdapter::GeminiDisabled
            | ProviderCompactionAdapter::OpenCodeDisabled
            | ProviderCompactionAdapter::QwenDisabled => {
                assert!(provider.compact_env_vars(80).is_empty());
                assert!(provider.compact_cli_config(80, 100_000).is_empty());
            }
        }

        let readiness_adapter = provider
            .readiness_adapter()
            .unwrap_or_else(|| panic!("registry provider {} has no readiness adapter", entry.id));
        assert_eq!(
            readiness_adapter.provider_id(),
            entry.id,
            "{} readiness must use a concrete provider adapter",
            entry.id
        );
        assert!(
            super::tmux_capture_indicates_ready_for_input(READY_CAPTURE, &provider),
            "{} readiness adapter rejected its ready banner",
            entry.id
        );
        assert!(
            !super::tmux_capture_indicates_ready_for_input(BUSY_CAPTURE, &provider),
            "{} readiness adapter accepted a generic busy capture",
            entry.id
        );
    }

    assert!(
        !provider_ids.is_empty(),
        "provider registry must not be empty"
    );
    assert_scoped_dispatches_have_no_wildcard_arms();
}

#[test]
fn unsupported_provider_preserves_generic_readiness_fallback() {
    let provider = ProviderKind::Unsupported("future-provider".to_string());
    let wrapper_marker =
        r#"{"type":"ready_for_input","provider":"future-provider","ts":"2026-07-14T00:00:00Z"}"#;

    assert!(super::tmux_capture_indicates_ready_for_input(
        READY_CAPTURE,
        &provider
    ));
    assert!(super::tmux_capture_indicates_ready_for_input(
        wrapper_marker,
        &provider
    ));
    assert!(!super::tmux_capture_indicates_ready_for_input(
        BUSY_CAPTURE,
        &provider
    ));
}

fn assert_scoped_dispatches_have_no_wildcard_arms() {
    let provider_source = include_str!("../provider.rs");
    let provider_exec_source = include_str!("../provider_exec.rs");

    for function_name in [
        "compact_env_vars",
        "compact_cli_config",
        "tmux_capture_indicates_ready_for_input",
    ] {
        assert_function_has_no_wildcard_arm(provider_source, function_name);
    }
    for function_name in [
        "execute_simple_blocking_inner",
        "execute_structured_with_context",
    ] {
        assert_function_has_no_wildcard_arm(provider_exec_source, function_name);
    }
}

fn assert_function_has_no_wildcard_arm(source: &str, function_name: &str) {
    let body = function_source(source, function_name);
    let wildcard_line = body.lines().find(|line| {
        line.split_once("=>")
            .is_some_and(|(pattern, _)| pattern.trim() == "_")
    });
    assert!(
        wildcard_line.is_none(),
        "{function_name} contains a wildcard match arm; every registered provider needs an explicit adapter: {}",
        wildcard_line.unwrap_or_default().trim()
    );
}

fn function_source<'a>(source: &'a str, function_name: &str) -> &'a str {
    let signature = format!("fn {function_name}(");
    let function_start = source
        .find(&signature)
        .unwrap_or_else(|| panic!("missing scoped function {function_name}"));
    let body_start = source[function_start..]
        .find('{')
        .map(|offset| function_start + offset)
        .unwrap_or_else(|| panic!("missing body for scoped function {function_name}"));

    let mut depth = 0_u32;
    for (offset, character) in source[body_start..].char_indices() {
        match character {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return &source[function_start..=body_start + offset];
                }
            }
            _ => {}
        }
    }
    panic!("unterminated body for scoped function {function_name}");
}
