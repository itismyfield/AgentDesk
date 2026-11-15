use super::*;

fn shown(text: &str) -> String {
    notice_line(Some("imessage"), text).unwrap()
}

#[test]
fn imessage_notice_accepts_three_headers_and_strips_only_tail_commands() {
    for header in [
        "[아이메시지 요청 · 시험]",
        "[아이메시지 요청 · 시험 · 답장]",
        "[아이메시지 요청 · 시험 · 결정 알림 123 답장]",
    ] {
        let reference = if header.ends_with("답장]") {
            "\n답장 대상 원문: python3 /tmp/imsg_reply.py --show 123"
        } else {
            ""
        };
        let text = format!(
            "{header}\n사용자 본문{reference}\n회신: python3 /tmp/imsg_reply.py --origin guid\n\n"
        );
        assert_eq!(
            shown(&text),
            "-# ⌨️ tui direct input\n```\n시험: 사용자 본문\n```",
            "{header}"
        );
    }
    let text = "[아이메시지 요청 · 시험]\n첫 줄\n회신: 본문 인용\n답장 대상 원문: 일반 인용\n마지막 본문\n회신: python3 /tmp/imsg_reply.py --origin guid";
    assert_eq!(
        shown(text),
        "-# ⌨️ tui direct input\n```\n시험: 첫 줄 회신: 본문 인용 답장 대상 원문: 일반 인용 마지막 본문\n```"
    );
}

#[test]
fn imessage_notice_unknown_boundaries_keep_the_whole_prompt() {
    for text in [
        "아무 헤더 없는 본문",
        "[아이메시지 요청 · ]\n본문",
        "[다른 요청 · 시험]\n본문",
        "[아이메시지 요청 · 시험]\n본문\n회신: 확인할 수 없는 지시",
        "[아이메시지 요청 · 시험 · 답장]\n본문\n답장 대상 원문: 지시가 아닌 인용\n회신: python3 /tmp/imsg_reply.py --origin guid",
    ] {
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            shown(text),
            crate::services::discord::input_notice_line(&format!("iMessage: {flat}"))
        );
    }
}

#[test]
fn imessage_notice_source_is_trimmed_exact_and_case_sensitive() {
    for source in [None, Some("external"), Some("iMessage"), Some("imessage-x")] {
        assert!(notice_line(source, "/compact").is_none());
    }
    assert_eq!(
        notice_line(Some(" imessage "), "/compact"),
        Some("-# ⌨️ tui direct input\n```\niMessage: /compact\n```".into())
    );
}

#[test]
fn imessage_notice_slash_fallback_is_not_filtered_as_nonhuman() {
    for text in [
        "/compact",
        "/loop 5m inspect",
        "<local-command-stdout>result</local-command-stdout>",
    ] {
        let line = shown(text);
        assert!(line.starts_with("-# ⌨️ tui direct input\n```\niMessage: "));
        assert!(line.ends_with("\n```"));
    }
}

#[test]
fn imessage_notice_flattens_then_uses_existing_controls_mentions_and_markdown_renderer() {
    let text = "[아이메시지 요청 · 시험]\n@everyone <@123> **강조**\t\u{1b}[31m빨강\u{1b}[0m\u{7}\n회신: python3 /tmp/imsg_reply.py --origin guid";
    let line = shown(text);
    assert!(line.contains("@everyone"));
    assert!(line.contains("<@123>"));
    assert!(line.contains("**강조**"));
    assert!(line.contains("빨강"));
    assert!(!line.contains(['\r', '\t', '\u{1b}', '\u{7}']));
}

#[test]
fn imessage_notice_is_one_message_with_unicode_omission_within_2000_utf16_units() {
    let text = format!(
        "[아이메시지 요청 · 시험]\n{}\n회신: python3 /tmp/imsg_reply.py --origin guid",
        "가😀".repeat(3000)
    );
    let line = shown(&text);
    assert!(line.contains("\n```\n-# … (이하 "));
    assert!(line.encode_utf16().count() <= 2000);
    assert!(line.contains("… (이하 "));
}

#[test]
fn imessage_notice_suffix_origin_is_display_only_even_when_missing_or_mismatched() {
    for suffix in ["", " --origin other", " --origin first --origin second"] {
        let text =
            format!("[아이메시지 요청 · 시험]\n본문\n회신: python3 /tmp/imsg_reply.py{suffix}");
        assert_eq!(shown(&text), "-# ⌨️ tui direct input\n```\n시험: 본문\n```");
    }
}
