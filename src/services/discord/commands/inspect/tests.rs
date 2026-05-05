use chrono::{DateTime, Utc};

use super::formatting::{format_context_usage, no_data_report};
use super::model::{InspectContextConfig, LatestTurn, LifecycleEventRow};
use super::render_context::render_context_report;
use super::render_prompt::render_prompt_manifest_report;
use crate::db::prompt_manifests::{PromptContentVisibility, PromptManifest, PromptManifestLayer};
use crate::services::provider::ProviderKind;

fn test_manifest() -> PromptManifest {
    PromptManifest {
        id: Some(42),
        created_at: None,
        turn_id: "discord:1:2".to_string(),
        channel_id: "1".to_string(),
        dispatch_id: None,
        profile: Some("Full".to_string()),
        total_input_tokens_est: 1_600,
        layer_count: 2,
        layers: vec![
            PromptManifestLayer {
                id: None,
                manifest_id: None,
                layer_name: "role_prompt".to_string(),
                enabled: true,
                source: Some("agents/project-agentdesk.prompt.md".to_string()),
                reason: None,
                chars: 400,
                tokens_est: 100,
                content_sha256: "a".repeat(64),
                content_visibility: PromptContentVisibility::AdkProvided,
                full_content: Some("ADK full body with ``` fence".to_string()),
                redacted_preview: None,
                is_truncated: false,
                original_bytes: Some(28),
            },
            PromptManifestLayer {
                id: None,
                manifest_id: None,
                layer_name: "user_message".to_string(),
                enabled: true,
                source: Some("discord".to_string()),
                reason: None,
                chars: 800,
                tokens_est: 200,
                content_sha256: "b".repeat(64),
                content_visibility: PromptContentVisibility::UserDerived,
                full_content: Some("SECRET USER BODY MUST NOT LEAK".to_string()),
                redacted_preview: Some("redacted user preview".to_string()),
                is_truncated: false,
                original_bytes: Some(30),
            },
        ],
    }
}

fn test_turn(turn_id: &str) -> LatestTurn {
    LatestTurn {
        turn_id: turn_id.to_string(),
        channel_id: "1".to_string(),
        provider: Some("codex".to_string()),
        session_key: Some("channel:1".to_string()),
        session_id: Some("codex-session".to_string()),
        dispatch_id: None,
        finished_at: DateTime::parse_from_rfc3339("2026-05-01T00:01:00Z")
            .unwrap()
            .with_timezone(&Utc),
        duration_ms: Some(60_000),
        input_tokens: 500,
        cache_create_tokens: 250,
        cache_read_tokens: 250,
    }
}

fn test_context() -> InspectContextConfig {
    InspectContextConfig {
        provider: ProviderKind::Codex,
        model: Some("gpt-test".to_string()),
        context_window_tokens: 2_000,
        compact_percent: 85,
    }
}

mod prompt {
    use super::*;

    #[test]
    fn prompt_report_uses_full_adk_body_but_only_redacted_user_preview() {
        let report = render_prompt_manifest_report(&test_manifest());

        assert!(report.contains("ADK full body"));
        assert!(report.contains("redacted user preview"));
        assert!(!report.contains("SECRET USER BODY"));
        assert!(report.contains("``\u{200B}` fence"));
    }
}

mod context {
    use super::*;

    #[test]
    fn context_report_orders_largest_layers_and_formats_compaction() {
        let turn = test_turn("discord:1:2");
        let mut manifest = test_manifest();
        manifest.layers[0].tokens_est = 100;
        manifest.layers[1].tokens_est = 900;
        let compaction = LifecycleEventRow {
            kind: "context_compacted".to_string(),
            severity: "info".to_string(),
            summary: "context compacted".to_string(),
            details_json: serde_json::json!({"before_pct": 88, "after_pct": 41}),
            created_at: turn.finished_at,
        };
        let context = test_context();

        let report = render_context_report(&turn, Some(&manifest), Some(&compaction), &context);
        let user_idx = report.find("- user_message").unwrap();
        let role_idx = report.find("- role_prompt").unwrap();

        assert!(report.contains("usage: 50%"));
        assert!(report.contains("before 88% -> after 41%"));
        assert!(user_idx < role_idx);
    }

    #[test]
    fn context_report_omits_other_turns_manifest_when_none() {
        // Regression for #1691: when the latest turn has no manifest yet,
        // we must NOT pull layer data from a different turn's manifest.
        let turn = test_turn("discord:1:99");
        let other_turn_manifest = test_manifest();
        let context = test_context();

        let report = render_context_report(&turn, None, None, &context);

        // Identifies that this report is bound to the current turn.
        assert!(report.contains("turn_id: discord:1:99"));
        // Manifest-pending indicator is shown.
        assert!(report.contains("manifest pending for this turn"));
        // Layer details from a different turn must not appear.
        for layer in &other_turn_manifest.layers {
            assert!(
                !report.contains(&layer.layer_name),
                "report leaked layer name {} from a different turn",
                layer.layer_name
            );
        }
        // Token usage from the latest turn is still rendered.
        assert!(report.contains("usage: 50%"));
    }

    #[test]
    fn context_report_clamps_context_usage_to_window() {
        let mut turn = test_turn("discord:1:3");
        turn.input_tokens = 50;
        turn.cache_create_tokens = 0;
        turn.cache_read_tokens = 400_000;
        let mut context = test_context();
        context.context_window_tokens = 200_000;

        let rendered = format_context_usage(&turn, &context);

        assert!(rendered.starts_with("100% "));
        assert!(!rendered.starts_with("200% "));
    }
}

mod formatting {
    use super::*;

    #[test]
    fn no_data_report_uses_required_phrase_inside_code_fence() {
        assert_eq!(no_data_report(), "```text\n최근 턴 데이터 없음\n```");
    }
}
