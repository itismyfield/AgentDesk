use super::*;

#[test]
fn retired_snapshots_preserve_legacy_body_and_rollover_bytes() {
    // Baseline 92e4b674: 2000 - units("\n\n" + 600 ASCII bytes) - margin 10 = 1388.
    let fixed = plan_streaming_rollover(&"x".repeat(6000), &"s".repeat(600)).unwrap();
    assert_eq!(fixed.split_at, 1388);
    assert_eq!(fixed.frozen_chunk.as_bytes(), "x".repeat(1388).as_bytes());
    for unit in ["x", "한", "🙂", "```rust\nlet x = 1;\n```\n\n"] {
        for len in (0..=6000).step_by(137) {
            let body = unit.repeat(len);
            for status in [
                "".to_string(),
                "⠸ 계속 처리 중".into(),
                "🔧 긴 도구 줄 ".repeat(60),
                "s".repeat(600),
            ] {
                let footer = format!("\n\n{}", clamp_placeholder_status_block(&status));
                // The rich block keeps its old reservation even though it is no longer shown.
                let reserved = discord_message_units(&footer) + STREAMING_PLACEHOLDER_MARGIN;
                let text = build_streaming_placeholder_text(&body, &status);
                if body.is_empty() {
                    assert_eq!(text, "⠋ 계속 처리 중");
                } else {
                    assert!(!text.ends_with(&footer) || status.is_empty());
                    assert!(discord_message_units(&text) + reserved <= DISCORD_MSG_LIMIT);
                    let normalized = normalize_empty_lines(&body);
                    if discord_message_units(&normalized) + reserved <= DISCORD_MSG_LIMIT {
                        assert_eq!(text, normalized, "a body within budget is shown whole");
                    }
                }
                if let Some(plan) = plan_streaming_rollover(&body, &status) {
                    assert_eq!(plan.display_snapshot, text);
                    assert_eq!(
                        plan.frozen_chunk.as_bytes(),
                        &body.as_bytes()[..plan.split_at]
                    );
                    assert!(
                        discord_message_units(&plan.frozen_chunk) + reserved <= DISCORD_MSG_LIMIT
                    );
                }
            }
        }
    }
}

#[test]
fn retired_empty_anchors_and_exhausted_budget_use_only_minimal_status() {
    for body in ["", "본문"] {
        let status = "🙂".repeat(1500);
        let current = build_streaming_placeholder_text(body, &status);
        // The old rich budget is never a visible empty anchor.
        assert_eq!(current, "⠋ 계속 처리 중");
        assert!(discord_message_units(&current) <= DISCORD_MSG_LIMIT);
    }
}

#[test]
fn retired_status_panel_edits_omit_footer_for_both_providers() {
    for provider in [
        crate::services::provider::ProviderKind::Claude,
        crate::services::provider::ProviderKind::Codex,
    ] {
        let status = "⠸ 계속 처리 중\n턴 시작 : <t:1700000000:R>";
        let body = "본문\n\n```rust\nfn main() {}\n```";
        let current = build_status_panel_streaming_edit_text(body, status, &provider);
        assert_eq!(
            current,
            build_status_panel_streaming_edit_text(body, "", &provider),
            "the status block only reserves budget"
        );
        assert!(!current.contains("계속 처리 중"));
        assert_eq!(
            build_status_panel_streaming_edit_text("", status, &provider),
            "⠋ 계속 처리 중"
        );
    }
}
