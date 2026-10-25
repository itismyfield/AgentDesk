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
        &"a9".repeat(16),
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

#[test]
fn system_prompt_preserves_paths_urls_and_nonsecret_words() {
    let opaque = "Ab9Z".repeat(12);
    for text in [
        "/Users/itismyfield/ObsidianVault/RemoteVault/99_Skills/agentdesk-issue-pipeline/SKILL.md"
            .to_owned(),
        "/home/adk/Project1234567890/agentdeskSkills42/knowledge_notes".to_owned(),
        "https://github.com/itismyfield/AgentDesk/issues/7378/comments".to_owned(),
        format!("/home/{opaque}/notes"),
        format!("prefix.{opaque}"),
        format!("{opaque}.md"),
        "a".repeat(64),
        "the sk-learn library and task-specific ask-user guidance".to_owned(),
    ] {
        assert_eq!(redact(&text), (text.clone(), 0), "nonsecret text changed");
    }
}

#[test]
fn system_prompt_masks_bounded_opaque_keys_and_labeled_values() {
    for text in [
        "Ab9+Z0".repeat(10),
        format!("sk-{}", "Ab9X".repeat(4)),
        "ghp_sensitive123".into(),
    ] {
        assert_eq!(
            redact(&format!("before ({text}) after")),
            ("before (***) after".into(), 1)
        );
    }
    let text =
        "token: short-value\napi_key='spaced value'\nsecret=bare\npassword: \"quoted value\"";
    assert_eq!(
        redact(text),
        (
            "token: ***\napi_key=***\nsecret=***\npassword: ***".into(),
            4
        )
    );
}

#[test]
fn system_prompt_masks_env_assignments_and_standard_base64() {
    for assignment in [
        "AGENTDESK_TOKEN=short-value",
        "export OPENAI_API_KEY=short-value",
    ] {
        let (masked, count) = redact(assignment);
        assert_eq!(masked, assignment.replace("short-value", "***"));
        assert_eq!(count, 1);
    }
    assert_eq!(
        redact("Cg47XywrFTAOow60OMNG+FhCCf/mIIMebVLeX1qw"),
        ("***".into(), 1)
    );
    let prompt = format!(
        "## Secret:\r\nNext line\n{}",
        "export OPENAI_API_KEY='spaced value'\n".repeat(200)
    );
    let (masked, count) = redact(&prompt);
    assert_eq!(count, 200);
    assert!(masked.starts_with("## Secret:\r\nNext line\n"));
    assert_eq!(masked.matches('\n').count(), prompt.matches('\n').count());
    let rendered = render(&prompt, "session", None, 0, true);
    assert_eq!(joined(&rendered[1..]), masked);
    assert!(
        rendered
            .iter()
            .all(|message| message.encode_utf16().count() <= 2000)
    );
}

#[test]
fn system_prompt_preserves_multiline_labels_identifiers_and_hash_paths() {
    let hash = "a9".repeat(32);
    for text in [
        "## Secret:\nNext line".into(),
        "`build_codex_memento_prompt_for_issue_4309`".into(),
        "sk-learn-compatible-estimators".into(),
        format!("/tmp/{hash}/notes"),
        format!("~/{hash}/notes"),
        format!("./{hash}/notes"),
        format!("https://example.com/{hash}"),
    ] {
        assert_eq!(redact(&text), (text.clone(), 0), "changed {text}");
    }
    assert_eq!(redact(&format!("--key {hash}")), ("--key ***".into(), 1));
}
