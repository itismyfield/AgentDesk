use super::*;
use crate::services::discord::footer_retirement::{legacy_for_test, retired};

#[test]
fn retirement_is_default_and_legacy_override_is_scoped() {
    assert!(retired());
    {
        let _legacy = legacy_for_test();
        assert!(!retired());
        {
            let _nested = legacy_for_test();
            assert!(!retired());
        }
        assert!(!retired());
    }
    assert!(retired());
    assert!(std::thread::spawn(retired).join().unwrap());
}

#[test]
fn retired_snapshots_preserve_legacy_body_and_rollover_bytes() {
    // Baseline 92e4b674: 2000 - units("\n\n" + 600 ASCII bytes) - margin 10 = 1388.
    let fixed = plan_streaming_rollover(&"x".repeat(6000), &"s".repeat(600)).unwrap();
    assert_eq!(fixed.split_at, 1388);
    assert_eq!(fixed.frozen_chunk.as_bytes(), "x".repeat(1388).as_bytes());
    // The oracle runs the unchanged legacy branch in this binary, rather than a copied planner.
    for unit in ["x", "한", "🙂", "```rust\nlet x = 1;\n```\n\n"] {
        for len in (0..=6000).step_by(137) {
            let body = unit.repeat(len);
            for status in [
                "".to_string(),
                "⠸ 계속 처리 중".into(),
                "🔧 긴 도구 줄 ".repeat(60),
                "s".repeat(600),
            ] {
                let (old_text, old_plan) = {
                    let _legacy = legacy_for_test();
                    (
                        build_streaming_placeholder_text(&body, &status),
                        plan_streaming_rollover(&body, &status),
                    )
                };
                let new_text = build_streaming_placeholder_text(&body, &status);
                if body.is_empty() {
                    assert_eq!(new_text, "⠋ 계속 처리 중");
                } else {
                    let footer = format!("\n\n{}", clamp_placeholder_status_block(&status));
                    assert_eq!(new_text, old_text.strip_suffix(&footer).unwrap());
                }
                let new_plan = plan_streaming_rollover(&body, &status);
                assert_eq!(
                    new_plan.as_ref().map(|p| (&p.frozen_chunk, p.split_at)),
                    old_plan.as_ref().map(|p| (&p.frozen_chunk, p.split_at))
                );
                if let Some(plan) = new_plan {
                    assert_eq!(plan.display_snapshot, new_text);
                    assert_eq!(
                        plan.frozen_chunk.as_bytes(),
                        &body.as_bytes()[..plan.split_at]
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
        let old = {
            let _legacy = legacy_for_test();
            build_streaming_placeholder_text(body, &status)
        };
        let current = build_streaming_placeholder_text(body, &status);
        assert_ne!(
            current, old,
            "the old rich budget is never a visible empty anchor"
        );
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
        let old = {
            let _legacy = legacy_for_test();
            build_status_panel_streaming_edit_text(
                "본문\n\n```rust\nfn main() {}\n```",
                status,
                &provider,
            )
        };
        let current = build_status_panel_streaming_edit_text(
            "본문\n\n```rust\nfn main() {}\n```",
            status,
            &provider,
        );
        assert_eq!(current, old.strip_suffix(&format!("\n\n{status}")).unwrap());
        assert!(!current.contains("계속 처리 중"));
        assert_eq!(
            build_status_panel_streaming_edit_text("", status, &provider),
            "⠋ 계속 처리 중"
        );
    }
}
