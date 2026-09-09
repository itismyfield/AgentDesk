use super::*;

#[test]
fn streaming_status_and_tmux_placeholder_suppression_unicode_units() {
    let limit = discord::DISCORD_MSG_LIMIT;
    // Body budget zero must not reserve even an ellipsis.
    for size in [1998, 1988] {
        let status = "한".repeat(size);
        let rendered = build_streaming_placeholder_text("본문", &status);
        assert!(discord_message_units(&rendered) <= limit);
        assert_eq!(rendered, format!("\n\n{status}"));
    }
    for status in ["한".repeat(2001), "\u{1f680}".repeat(1001)] {
        let rendered = build_streaming_placeholder_text("본문", &status);
        assert!(discord_message_units(&rendered) <= limit);
        assert!(!rendered.contains("본문"));
    }
    let status = "한\u{1f680}".repeat(100);
    let body = "가".repeat(2000);
    let plan = discord::formatting::plan_streaming_rollover(&body, &status).unwrap();
    // 300 footer units + 2 separator units + 10 margin, not 200 scalars.
    assert_eq!(plan.frozen_chunk, "가".repeat(1688));
    assert_eq!(plan.split_at, "가".repeat(1688).len());
    assert!(discord_message_units(&plan.display_snapshot) <= limit);

    let rewrite = |text: &str, label: &str| {
        rewrite_placeholder_as_terminal_suppressed(text, label, &ProviderKind::Claude)
    };
    for label in [
        "한".repeat(1500),
        "한".repeat(2000),
        "\u{1f680}".repeat(1000),
    ] {
        assert_eq!(rewrite("", &label), label);
    }
    for body in ["한".repeat(1997), "\u{1f680}".repeat(998) + "한"] {
        let rendered = rewrite(&body, "끝");
        assert_eq!(rendered, format!("{body}\n\n끝"));
        assert_eq!(discord_message_units(&rendered), limit);
        assert_eq!(rewrite(&rendered, "끝"), rendered);
    }
    let clipped = rewrite(&"한".repeat(2100), "끝");
    assert_eq!(clipped, format!("{}\n\n끝", "한".repeat(1997)));
    let clipped = rewrite(&"\u{1f680}".repeat(1000), "끝");
    assert_eq!(clipped, format!("{}\n\n끝", "\u{1f680}".repeat(998)));
    for label in ["한".repeat(2100), "\u{1f680}".repeat(1100)] {
        for body in ["", "본문", label.as_str()] {
            assert!(discord_message_units(&rewrite(body, &label)) <= limit);
        }
    }
}
