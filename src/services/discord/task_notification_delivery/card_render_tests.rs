use super::*;

fn task(status: Option<&str>, summary: &str, result: Option<&str>) -> TaskCardPayload {
    TaskCardPayload::Task(card::TaskNotification {
        status: status.map(str::to_string),
        summary: Some(summary.to_string()),
        result: result.map(str::to_string),
        ..Default::default()
    })
}

#[test]
fn monitor_render_uses_provider_subtext_header() {
    let payload = task(
        None,
        "Monitor event: \"coord events\"",
        Some("latest event"),
    );
    assert_eq!(
        payload.render(9),
        "-# 📡 coord events: latest event · 9회",
        "monitor render must retain its single-line provider subtext header"
    );
}

#[test]
fn monitor_render_summarizes_multiline_event_at_render_entrypoint() {
    let payload = task(
        None,
        "Monitor event: \"coord events\"",
        Some("latest *event*\nsecond private detail\nthird detail"),
    );
    assert_eq!(
        payload.render(9),
        "-# 📡 coord events: latest \\*event\\* (+2줄) · 9회",
        "monitor render must summarize raw multiline events before transport guards"
    );
}

#[test]
fn short_task_status_render_is_one_subtext_line() {
    for (status, icon) in [("completed", "✅"), ("failed", "❌"), ("stopped", "🛑")] {
        let payload = task(Some(status), "worker *status*\nprivate next line", None);
        assert_eq!(
            payload.render(1),
            format!("-# {icon} worker \\*status\\* (+1줄)"),
            "short task status must use the shared one-line formatter"
        );
    }
}

#[test]
fn monitor_render_keeps_markdown_and_unicode_separators_on_one_line() {
    let payload = task(
        None,
        "Monitor event: \"coord events\"",
        Some("# [event](<url>) `code`\u{2028}next\u{2029}last"),
    );
    let rendered = payload.render(1);
    assert_eq!(
        rendered, "-# 📡 coord events: \\# \\[event\\](‹url›) \\`code\\` (+2줄) · 1회",
        "monitor render must escape markdown and consume Unicode line separators"
    );
    assert!(inline::is_inline(&rendered));
}

#[test]
fn parsed_bodyless_subagent_status_has_exactly_one_status_icon() {
    for (payload, icon, detail) in [
        (
            r#"{"status":{"completed":""}}"#,
            "📋",
            "no completed/failed status",
        ),
        (
            r#"{"status":{"failed":"  "}}"#,
            "📋",
            "no completed/failed status",
        ),
        (r#"{"status":{}}"#, "📋", "no completed/failed status"),
        ("not-json", "⚠️", "malformed payload omitted"),
    ] {
        let event = TaskCardEvent::from_subagent_prompt_with_source_event_id(
            42,
            "codex",
            "renderer-session",
            &format!("<subagent_notification>{payload}</subagent_notification>"),
            Some("renderer-source"),
        );
        assert!(!event.payload.is_completion_card());
        let rendered = event.payload.render(1);
        assert_eq!(
            rendered,
            format!("-# {icon} Subagent notification (tmux : `renderer-session`) — {detail}")
                .replace('`', "\\`"),
            "the actual parser and payload renderer must produce one status icon"
        );
        assert_eq!(rendered.matches(icon).count(), 1);
        assert!(inline::is_inline(&rendered));
    }
}
