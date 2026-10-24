use super::{format_task_notification_card, parse_task_notification};

fn completed(result: &str, usage: &str) -> String {
    format!(
        "<task-notification><task-id>completion-private-id</task-id>\
         <status>completed</status><summary>Agent &quot;verify &lt;T&gt;&quot; completed</summary>\
         <result>{result}</result><usage>{usage}</usage></task-notification>"
    )
}

#[test]
fn completion_entry_combines_header_and_keeps_quoted_report_with_real_usage() {
    let raw = completed(
        "First finding.\nSecond finding.",
        "<subagent_tokens>177612</subagent_tokens><tool_uses>37</tool_uses>\
         <duration_ms>504983</duration_ms>",
    );
    let card = format_task_notification_card(&parse_task_notification(&raw), 1);

    assert_eq!(
        card,
        "✅ **Agent \"verify <T>\" completed**\n\n> First finding.\n> Second finding.\n\n\
         -# 37 tools · ⏱ 8m 24s · 🪙 177.6k tok"
    );
    assert!(!card.contains("Task completed"), "{card}");
    assert!(!card.contains("completion-private-id"), "{card}");
    assert!(
        !card.contains('$'),
        "aggregate tokens cannot determine cost: {card}"
    );
}

#[test]
fn failed_workflow_entry_uses_one_header_and_preserves_latest_report() {
    let raw = "<task-notification><task-id>workflow-private-id</task-id>\
        <status>failed</status><summary>Dynamic workflow &quot;verify&quot; failed</summary>\
        <result>Compile failed.\nRetry after fixing the type.</result>\
        <usage><agent_count>3</agent_count><subagent_tokens>1341096</subagent_tokens>\
        <tool_uses>8</tool_uses><duration_ms>120000</duration_ms></usage></task-notification>";
    let card = format_task_notification_card(&parse_task_notification(raw), 3);

    assert_eq!(
        card.lines().next(),
        Some("❌ **Dynamic workflow \"verify\" failed** · updated 3x")
    );
    assert!(
        card.contains("> Latest: Compile failed.\n> Retry after fixing the type."),
        "{card}"
    );
    assert!(
        card.ends_with("-# 3 agents · 8 tools · ⏱ 2m · 🪙 1.3m tok"),
        "{card}"
    );
    assert!(!card.contains("task workflow"), "{card}");
}

#[test]
fn completion_entry_omits_unavailable_or_malformed_tokens_and_cost() {
    for usage in ["", "<subagent_tokens>unknown</subagent_tokens>"] {
        let raw = completed("Useful report.", usage);
        let card = format_task_notification_card(&parse_task_notification(&raw), 1);
        assert!(
            card.starts_with("✅ **Agent \"verify <T>\" completed**\n\n> "),
            "{card}"
        );
        assert!(!card.contains("🪙"), "{card}");
        assert!(!card.contains('$'), "{card}");
        assert!(!card.contains("-# task "), "{card}");
    }
}

#[test]
fn completion_entry_token_total_is_scoped_to_structured_usage() {
    let raw = completed(
        "Quoted <subagent_tokens>999999</subagent_tokens> is prose.",
        "<subagent_tokens>177612</subagent_tokens><tool_uses>2</tool_uses>",
    );
    let card = format_task_notification_card(&parse_task_notification(&raw), 1);
    assert!(card.ends_with("-# 2 tools · 🪙 177.6k tok"), "{card}");
    assert!(!card.contains("999.9k tok"), "{card}");
}

#[test]
fn completion_entry_collapses_summary_newlines_without_expanding_discord_body() {
    let raw = "<task-notification><task-id>header-private-id</task-id>\
        <status>completed</status><summary>First line\nsecond line</summary>\
        <result>Quoted detail.</result></task-notification>";
    let card = format_task_notification_card(&parse_task_notification(raw), 1);
    assert_eq!(card.lines().next(), Some("✅ **First line second line**"));
    assert!(card.contains("\n\n> Quoted detail."), "{card}");
}

#[test]
fn monitor_report_keeps_legacy_renderer_bytes_and_stays_outside_completion_cards() {
    let raw = "<task-notification><task-id>monitor-private-id</task-id>\
        <summary>Monitor event: &quot;resident daemon&quot;</summary>\
        <result>Trigger one.\nTrigger two.</result>\
        <usage><subagent_tokens>177612</subagent_tokens><tool_uses>4</tool_uses>\
        <duration_ms>45000</duration_ms></usage></task-notification>";
    let note = parse_task_notification(raw);
    assert!(!super::has_completion_body(&note));
    assert_eq!(
        format_task_notification_card(&note, 2),
        "📋 Task event · updated 2x\n**Monitor event: \"resident daemon\"**\n\n\
         > Latest: Trigger one.\n> Trigger two.\n\n-# task monitor- · 4 tools · ⏱ 45s"
    );
}
