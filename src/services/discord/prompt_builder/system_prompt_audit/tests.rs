use super::*;

fn joined(blocks: &[String]) -> String {
    blocks
        .iter()
        .map(|b| {
            b.strip_prefix("```\n")
                .unwrap()
                .strip_suffix("\n```")
                .unwrap()
        })
        .collect()
}

#[test]
fn system_prompt_utf16_boundaries_and_roundtrip() {
    for length in [1991, 1992, 1993, 1999, 2000, 2001, 8001] {
        let text = "가".repeat(length);
        let blocks = code_blocks(&text);
        assert_eq!(joined(&blocks), text);
        assert!(blocks.iter().all(|b| b.encode_utf16().count() <= 2000));
        assert_eq!(blocks.len(), length.div_ceil(BODY_LIMIT));
    }
    for (length, expected) in [(1991, 1999), (1992, 2000), (1993, 2000)] {
        assert_eq!(
            code_blocks(&"x".repeat(length))[0].encode_utf16().count(),
            expected
        );
    }
}

#[test]
fn system_prompt_lines_and_emoji_keep_every_character() {
    let text = format!(
        "{}\n{}\n{}",
        "a".repeat(1500),
        "b".repeat(800),
        "🧾😀".repeat(1300)
    );
    let blocks = code_blocks(&text);
    assert_eq!(joined(&blocks), text);
    assert!(blocks[0].ends_with("\n\n```"));
    assert!(blocks.iter().all(|b| b.encode_utf16().count() <= 2000));
    assert_eq!(code_blocks("").len(), 1);
}

#[test]
fn system_prompt_fences_cannot_close_the_wrapper() {
    for ticks in [3, 4, 6, 18, 1994] {
        let text = format!("before\n{}\nafter", "`".repeat(ticks));
        let neutral = neutralize_fences(&text);
        assert!(!neutral.contains("```"));
        let blocks = code_blocks(&neutral);
        assert_eq!(joined(&blocks), neutral);
        assert_eq!(neutral.replace('\u{200b}', ""), text);
    }
}

#[test]
fn system_prompt_masks_keys_and_token_shapes_and_counts_values_once() {
    let values = [
        "private-key",
        "a spaced value",
        "another spaced value",
        "sk-proj-Sensitive123",
        "ghp_sensitive123",
        "xoxb-123-sensitive",
        &"a".repeat(32),
        &"Ab9+/Z".repeat(12),
    ];
    let text = format!(
        "--key {}\n--key=\"{}\"\n--key '{}'\n{}\n{}\n{}\n{}\n{}",
        values[0], values[1], values[2], values[3], values[4], values[5], values[6], values[7]
    );
    let (masked, count) = redact(&text);
    assert_eq!(count, values.len());
    for value in values {
        assert!(!masked.contains(value), "unmasked value");
    }
    assert_eq!(
        redact("ordinary words and short_hex_123 task-specific ask-user"),
        (
            "ordinary words and short_hex_123 task-specific ask-user".into(),
            0
        )
    );
    assert_eq!(redact("--key sk-keyToken123"), ("--key ***".into(), 1));
    assert_eq!(redact("xoxe.xoxb-sensitive_123-token"), ("***".into(), 1));
    assert_eq!(
        redact(&format!("{}ZmixedSuffix123", "a".repeat(40))),
        ("***".into(), 1)
    );
}

#[test]
fn system_prompt_header_distinguishes_origins_and_anchors() {
    let original = render(
        "safe\nbody",
        "12345678-session",
        Some("2026-10-25 UTC"),
        3,
        true,
    );
    assert!(original[0].contains("세션 12345678 시작 2026-10-25 UTC"));
    assert!(original[0].ends_with("가림 0 · 원본"));
    assert!(original[0].contains("앵커 3개"));
    let rebuilt = render("safe\nbody", "12345678-session", None, 0, false);
    assert!(rebuilt[0].contains("시작 알 수 없음"));
    assert!(rebuilt[0].contains("앵커 없음"));
    assert!(rebuilt[0].ends_with("재구성"));
    assert_eq!(joined(&rebuilt[1..]), "safe\nbody");
}
