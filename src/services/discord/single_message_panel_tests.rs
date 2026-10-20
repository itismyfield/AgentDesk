use super::super::DISCORD_MSG_LIMIT;
use super::super::ProviderKind;
use super::super::footer_view_reconciler as footer_registry;
use crate::services::agent_protocol::StatusEvent;
use poise::serenity_prelude::{ChannelId, MessageId};

#[test]
fn final_wire_positions_survive_only_complete_lines_5304() {
    let channel = ChannelId::new(5_304_201);
    let shared = super::super::make_shared_data_for_tests();
    let events = &shared.ui.placeholder_live_events;
    for id in ["first", "second"] {
        events.push_status_event(
            channel,
            StatusEvent::BackgroundTaskStart {
                name: "Bash".into(),
                summary: "동일한 작업".into(),
                tool_use_id: id.into(),
            },
        );
        events.push_status_event(
            channel,
            StatusEvent::BackgroundTaskEnd {
                tool_use_id: id.into(),
                success: true,
            },
        );
    }
    let rendered = events.render_completion_footer(channel, &ProviderKind::Claude, "x");
    let block = rendered.block.as_deref().unwrap();
    assert_eq!(rendered.delivered_terminal_ids.len(), 2);
    assert_ne!(
        rendered.delivered_terminal_ids[0],
        rendered.delivered_terminal_ids[1]
    );
    assert_eq!(block.matches("동일한 작업 ✓").count(), 2);
    let first_end = rendered.terminal_line_ends[0];
    let mut prefix = 999;
    let subtext = super::completion_footer_subtext(block);
    let limit = super::super::formatting::discord_message_units(&subtext[..first_end]) + 7;
    let wire = super::compose_completion_footer_text_without_warning_with_limit(
        "body ✓ ✗",
        Some(block),
        limit,
        false,
        &mut prefix,
    );
    assert!(wire.ends_with('…'));
    assert_eq!(
        rendered.surviving_terminal_ids(prefix),
        rendered.delivered_terminal_ids[..1]
    );
    super::compose_completion_footer_text_without_warning_with_limit(
        "body ✓",
        Some(block),
        6,
        false,
        &mut prefix,
    );
    assert!(rendered.surviving_terminal_ids(prefix).is_empty());
    super::compose_completion_footer_text_tracked("body ✓ ✗", Some(block), &mut prefix);
    assert_eq!(
        rendered.surviving_terminal_ids(prefix),
        rendered.delivered_terminal_ids
    );
    let fenced = format!("```dangling\n{block}");
    super::compose_completion_footer_text_tracked("", Some(&fenced), &mut prefix);
    assert!(rendered.surviving_terminal_ids(prefix).is_empty());
    super::compose_completion_footer_text_tracked("body ✓ ✗", None, &mut prefix);
    assert_eq!(prefix, 0);
    let mut rendered = events.render_completion_footer(channel, &ProviderKind::Claude, "x");
    let padding = format!("{}\n", "a".repeat(650));
    let offset = super::completion_footer_subtext(&padding).len() + 1;
    for end in &mut rendered.terminal_line_ends {
        *end += offset;
    }
    let block = format!("{padding}{}", rendered.block.as_deref().unwrap());
    let warning = format!(
        "{block}\n\n⚠️ **턴을 완료하기 전에 커밋되지 않은 변경사항을 확인하세요.**\n{}",
        "w".repeat(1600)
    );
    let wire = super::compose_completion_footer_text_tracked("본문 ✓", Some(&warning), &mut prefix);
    assert!(wire.contains("변경사항"));
    assert!(rendered.surviving_terminal_ids(prefix).is_empty());
    super::compose_completion_footer_text_tracked("본문 ✓", Some(&block), &mut prefix);
    assert_eq!(
        rendered.surviving_terminal_ids(prefix),
        rendered.delivered_terminal_ids
    );
}

fn panel_portion(status_block: &str) -> &str {
    status_block
        .split_once('\n')
        .map(|(_, panel)| panel)
        .unwrap_or("")
}

fn footer_header(status_block: &str) -> &str {
    status_block.lines().next().unwrap_or("")
}

fn push_unfinished_subagent(channel_id: ChannelId) -> std::sync::Arc<super::super::SharedData> {
    let shared = super::super::make_shared_data_for_tests();
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::SubagentStart {
            subagent_type: Some("reviewer".to_string()),
            desc: Some("Long background job".to_string()),
            agent_id: None,
            tool_use_id: Some(format!("tool-{}", channel_id.get())),
            background: true,
        },
    );
    shared
}

fn push_unfinished_background_task(
    channel_id: ChannelId,
) -> std::sync::Arc<super::super::SharedData> {
    let shared = super::super::make_shared_data_for_tests();
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::BackgroundTaskStart {
            name: "Bash".to_string(),
            summary: "Run background codex".to_string(),
            tool_use_id: format!("tool-{}", channel_id.get()),
        },
    );
    shared
}

fn push_finished_and_running_background_tasks(
    channel_id: ChannelId,
) -> std::sync::Arc<super::super::SharedData> {
    let shared = super::super::make_shared_data_for_tests();
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::BackgroundTaskStart {
            name: "Bash".to_string(),
            summary: "Finished job".to_string(),
            tool_use_id: format!("tool-done-{}", channel_id.get()),
        },
    );
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::BackgroundTaskEnd {
            tool_use_id: format!("tool-done-{}", channel_id.get()),
            success: true,
        },
    );
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::BackgroundTaskStart {
            name: "Bash".to_string(),
            summary: "Running job".to_string(),
            tool_use_id: format!("tool-run-{}", channel_id.get()),
        },
    );
    shared
}

#[test]
fn single_message_panel_flag_defaults_on_when_unset() {
    // #3560: missing env var means default-ON (opt-out), not the legacy OFF.
    assert!(super::parse_single_message_panel_flag(None));
}

#[test]
fn single_message_panel_flag_treats_non_optout_values_as_enabled() {
    // #3560: only "0" / "false" disable the gate. Everything else — including
    // an empty string or garbage — keeps the default-ON feature enabled so it
    // is never quietly switched off by an unintended env value.
    for raw in [
        "1", "true", "TRUE", "TrUe", " true ", "", "yes", "on", "garbage",
    ] {
        assert!(
            super::parse_single_message_panel_flag(Some(raw)),
            "{raw:?} should enable the flag"
        );
    }
}

#[test]
fn single_message_panel_flag_rejects_documented_optout_values() {
    // #3560: the disable-list mirrors the original truthy-allowlist — only the
    // documented opt-out tokens "0" and case-insensitive "false" turn it off.
    for raw in ["0", "false", "FALSE"] {
        assert!(
            !super::parse_single_message_panel_flag(Some(raw)),
            "{raw:?} should disable the flag"
        );
    }
}

#[test]
fn footer_mode_requires_both_flags() {
    assert!(!super::footer_mode_enabled(true, true));
    assert!(!super::footer_mode_enabled(true, false));
    assert!(!super::footer_mode_enabled(false, true));
}

#[test]
fn footer_status_block_keeps_spinner_first() {
    let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
    let block = super::compose_footer_status_block("⠸", panel);

    assert!(block.starts_with("-# ⠸ 진행 중 — Claude (<t:1700000000:R>)"));
    assert!(!footer_header(&block).contains('🟢'));
    assert!(!block.contains("계속 처리 중"));
    assert!(block.contains("\n\n-# Subagents\n-# └ review inspect"));
}

#[test]
fn footer_strips_only_exact_panel_header_marker_suffix() {
    let marker = super::super::formatting::PLACEHOLDER_PROBE_MARKER;
    let suffix =
        super::compose_footer_status_block("⠸", &format!("-# 🔧 activity{marker}\r\ntime"));
    assert!(!suffix.contains(marker));
    let embedded = super::compose_footer_status_block(
        "⠸",
        &format!("-# 🔧 activity{marker} kept\nbody{marker}"),
    );
    assert_eq!(
        embedded.matches(marker).count(),
        2,
        "only the header suffix belongs to this boundary"
    );
}

#[test]
fn latest_tool_footer_header_is_recognized_after_spinner_merge() {
    let panel = "🔧 마지막 도구 ([Read] · src/lib.rs)\n턴 시작 : 07-25 09:00:00";
    let block = super::compose_footer_status_block("⠸", panel);

    assert_eq!(
        footer_header(&block),
        "-# ⠸ 마지막 도구 ([Read] · src/lib.rs)"
    );
    assert!(super::is_merged_footer_status_line(footer_header(&block)));
}

#[test]
fn footer_status_block_empty_panel_falls_back_to_processing_line() {
    assert_eq!(
        super::compose_footer_status_block("⠸", ""),
        "⠸ 계속 처리 중"
    );
    assert_eq!(
        super::compose_footer_status_block("⠸", " \n\t "),
        "⠸ 계속 처리 중"
    );
}

#[test]
fn footer_panel_under_budget_is_unchanged_s3() {
    let panel = "Header\n\nTools\n└ cargo test";
    let block = super::compose_footer_status_block("⠸", panel);

    assert_eq!(block, "-# ⠸ Header\n\n-# Tools\n-# └ cargo test");
    assert!(!panel_portion(&block).ends_with("\n…"));
}

#[test]
fn footer_panel_over_budget_excludes_merged_header_from_budget_s3() {
    let huge_panel = format!(
        "🟢 진행 중 — Claude (<t:1700000000:R>)\n{}\n{}\n{}",
        "a".repeat(590),
        "b".repeat(590),
        "c".repeat(590)
    );
    let block = super::compose_footer_status_block("⠸", &huge_panel);
    let (header, panel) = block
        .split_once('\n')
        .expect("over-budget panel should keep merged header and panel body");

    assert_eq!(header, "-# ⠸ 진행 중 — Claude (<t:1700000000:R>)");
    assert!(panel.len() <= super::SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES);
    assert!(panel.ends_with("\n-# …") || panel == "-# …");
    assert!(block.len() > panel.len());
}

#[test]
fn streaming_footer_semantic_compare_ignores_spinner_tick_only_3717() {
    let previous =
        "visible body\n\n⠋ 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
    let next =
        "visible body\n\n⠙ 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";

    assert!(super::same_streaming_footer_except_spinner_tick(
        previous, next
    ));
}

#[test]
fn streaming_footer_semantic_compare_keeps_body_and_panel_changes_3717() {
    let previous =
        "visible body\n\n⠋ 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
    let body_changed = "visible body plus output\n\n⠙ 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
    let panel_changed =
        "visible body\n\n⠙ 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review done";
    let completed =
        "visible body\n\n✅ 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";

    assert!(!super::same_streaming_footer_except_spinner_tick(
        previous,
        body_changed
    ));
    assert!(!super::same_streaming_footer_except_spinner_tick(
        previous,
        panel_changed
    ));
    assert!(!super::same_streaming_footer_except_spinner_tick(
        previous, completed
    ));
}

/// #3394: the footer finalization sink must re-balance fence parity. A
/// legacy/debug-shaped fenced Recent block chopped by the body clamp must
/// not emit a dangling ``` for Discord to render as literal text.
#[test]
fn footer_status_block_repairs_dangling_fence_after_clamp_3394() {
    let panel = format!(
        "🟢 진행 중 — Claude (<t:1700000000:R>)\n🖥️ Recent\n```text\n{}\n```",
        "echo hello\n".repeat(120)
    );
    let block = super::compose_footer_status_block("⠸", &panel);

    let fences = block.matches("```").count();
    assert_eq!(
        fences % 2,
        0,
        "footer leaked an unterminated fence: {block}"
    );
    assert!(!block.contains("```text") || fences >= 2);
}

/// #3495: when the panel clamp severs a legacy/debug-shaped fenced 🖥️ Recent
/// block, the WHOLE section (header + fence + body) is dropped — Discord
/// must never see a bare `🖥️ Recent` header with no terminal body (the
/// intermittent "터미널 칸이 사라짐" symptom). When the block fits, it is
/// preserved intact.
#[test]
fn footer_panel_clamp_drops_severed_recent_section_3495() {
    // Small leading line so the cut lands inside the Recent section: the
    // header (+ opener) would survive a naive clamp but its body cannot.
    let severed = format!(
        "🟢 진행 중 — Claude (<t:1700000000:R>)\nstreaming line\n🖥️ Recent (mac-book-release)\n```text\n{}\n```",
        "echo hello\n".repeat(130)
    );
    let block = super::compose_footer_status_block("⠸", &severed);
    assert!(
        !block.contains("🖥️ Recent"),
        "severed Recent header left without a terminal body: {block}"
    );
    assert_eq!(
        block.matches("```").count() % 2,
        0,
        "footer leaked an unterminated fence: {block}"
    );
    assert!(block.len() <= super::super::DISCORD_MSG_LIMIT);

    // Control: a Recent block that fits the budget is preserved intact.
    let fits = "🟢 진행 중 — Claude (<t:1700000000:R>)\n🖥️ Recent (host)\n```text\necho hi\n```";
    let kept = super::compose_footer_status_block("⠸", fits);
    assert!(
        kept.contains("🖥️ Recent"),
        "fitting Recent block was dropped: {kept}"
    );
    assert!(
        kept.contains("echo hi"),
        "fitting Recent body was dropped: {kept}"
    );
    assert_eq!(
        kept.matches("```").count() % 2,
        0,
        "fitting block unbalanced: {kept}"
    );
}

/// #3497: the raised live-panel budget renders a full ~1000B 🖥️ Recent
/// terminal block that the prior 600B budget would have truncated/dropped.
#[test]
fn footer_panel_budget_shows_recent_block_over_legacy_600_3497() {
    let recent_body = "L cargo build output line\n".repeat(40); // ~1040B
    assert!(recent_body.len() > 600 && recent_body.len() < 1100);
    let panel = format!(
        "🟢 진행 중 — Claude (<t:1700000000:R>)\n🖥️ Recent (host)\n```text\n{recent_body}```"
    );
    let block = super::compose_footer_status_block("⠸", &panel);
    // The full fenced block survives (not severed by the clamp) under the live-panel budget,
    // whereas a 600B budget would have dropped the whole Recent section (#3495).
    assert!(
        block.contains("🖥️ Recent"),
        "Recent header dropped under live-panel budget: {block}"
    );
    assert!(
        block.contains("L cargo build output line"),
        "Recent terminal body truncated under live-panel budget: {block}"
    );
    assert_eq!(
        block.matches("```").count() % 2,
        0,
        "unbalanced fence: {block}"
    );
    assert!(block.len() <= super::super::DISCORD_MSG_LIMIT);
}

/// #3394 round 2 (P1): when the response BODY carries an unterminated code
/// fence (the Discord-limit trim cut inside a body fence), the boundary-scoped
/// repair must drop only the body's dangling opener and KEEP the appended
/// completion footer — its ✓ marks must survive. On HEAD the combined repair
/// deleted from the body opener through the footer, taking the ✓ with it while
/// #3391 still evicted the slot, breaking delivered-ID honesty.
#[test]
fn compose_completion_footer_repair_scoped_to_body_keeps_footer_marks_3394() {
    let body = "Here is some output:\n\n```text\nchopped streamed body";
    let footer = "📦 154.6k / 1.0M (15%)\n\nTasks\n└ Bash Finished job ✓";
    let composed = super::compose_completion_footer_text(body, Some(footer));

    // The footer block (and its terminal ✓) survives composition.
    assert!(
        composed.contains("Bash Finished job ✓"),
        "footer ✓ was deleted by combined fence repair: {composed}"
    );
    assert!(composed.contains("📦"));
    assert!(composed.contains("Tasks"));
    // The body's dangling opener is dropped (boundary-scoped repair).
    assert!(
        !composed.contains("```text"),
        "body dangling fence leaked: {composed}"
    );
    assert!(composed.contains("Here is some output:"));
    // Combined parity is even.
    assert_eq!(
        composed.matches("```").count() % 2,
        0,
        "composed text has odd fence parity: {composed}"
    );
}

/// #3394 round 2 (P1): the registered-refresh edit is implemented by
/// `footer_view_reconciler/registry.rs::completion_footer_edit_for_registered_target_at`,
/// which uses `compose_completion_footer_text`. With no WIP warning, the
/// message limit remains `DISCORD_MSG_LIMIT` and `max_block_units` is 1994,
/// so the 600-byte task-section cap plus the other footer lines leave the
/// rendered footer's terminal mark in the edit text after body-only repair;
/// a successful edit can then evict only a mark present in that text. When
/// warning reservation shrinks the remaining block budget as low as 600
/// units, the inline clamp can still cut a retained terminal tail after its
/// render-local id was selected. That residual risk is accepted to keep the
/// warning from being the truncation victim: the possible sacrifice moves
/// to the footer tail instead.
#[test]
fn registered_refresh_repair_scoped_to_body_keeps_footer_marks_3394() {
    let channel_id = ChannelId::new(3_394_201);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_finished_and_running_background_tasks(channel_id);
    let body_with_dangling_fence = "Streaming reply\n\n```text\ncut off mid fence";
    let _ = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_394_301),
        &ProviderKind::Claude,
        1_800_000_000,
        body_with_dangling_fence,
        None,
        true,
    );

    let edit = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠸",
        1_800_000_001,
    )
    .expect("registered refresh should render the terminal mark");

    assert!(
        edit.text.contains("Bash Finished job ✓"),
        "registered-refresh footer ✓ was deleted by fence repair: {}",
        edit.text
    );
    assert!(edit.text.contains("Bash Running job ⠸"));
    assert!(
        !edit.text.contains("```text"),
        "body dangling fence leaked: {}",
        edit.text
    );
    assert_eq!(
        edit.text.matches("```").count() % 2,
        0,
        "registered-refresh composed text has odd fence parity: {}",
        edit.text
    );
    // #3391: the delivered ✓ slot identity is reported for this edit, and it is
    // genuinely present in the delivered text above.
    assert!(!edit.delivered_terminal_ids.is_empty());
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

/// #3394 round 2: a body with a BALANCED fence and a footer compose without
/// any repair touching the body's closed fence — parity stays even and both
/// the fenced body and the footer survive intact.
#[test]
fn completion_footer_subtext_prefixes_each_nonempty_line_4080() {
    assert_eq!(
        super::completion_footer_subtext(
            "📦 10 / 100 (10%) · auto-compact 50%\n\nTasks\n└ Bash Done ✓\n⏱ 2m 34s"
        ),
        "-# 📦 10 / 100 (10%) · auto-compact 50%\n\n-# Tasks\n-# └ Bash Done ✓\n-# ⏱ 2m 34s"
    );
}

#[test]
fn completion_footer_with_body_clamps_oversized_suffix_5177() {
    let body = "본문!";
    let completion = "한".repeat(2_497);
    let rendered_completion = super::completion_footer_subtext(&completion);
    assert_eq!(
        super::super::formatting::discord_message_units(&rendered_completion),
        2_500,
        "rendered completion fixture unit count"
    );
    let unclamped = format!("{body}\n\n{rendered_completion}");
    assert_eq!(
        super::super::formatting::discord_message_units(&unclamped),
        2_505,
        "origin/main with-body payload unit count"
    );

    let composed = super::compose_completion_footer_text(body, Some(&completion));
    assert!(
        super::super::formatting::discord_message_units(&composed) <= DISCORD_MSG_LIMIT,
        "final with-body payload must fit the shared limit: {}",
        super::super::formatting::discord_message_units(&composed)
    );
    assert!(composed.starts_with(body));
    assert!(composed.ends_with('…'));
}

#[test]
fn completion_footer_preserves_korean_payload_within_unit_limit_5177() {
    let body = "본문!";
    let completion = "한".repeat(1_990);
    let expected = format!(
        "{body}\n\n{}",
        super::completion_footer_subtext(&completion)
    );
    assert_eq!(
        super::super::formatting::discord_message_units(&expected),
        1_998,
        "fixture must fit the shared message-unit limit"
    );

    let composed = super::compose_completion_footer_text(body, Some(&completion));

    assert_eq!(
        composed, expected,
        "in-limit Korean footer must be preserved"
    );
}

#[test]
fn footer_only_preserves_korean_payload_within_unit_limit_5177() {
    let completion = "한".repeat(700);
    let expected = super::completion_footer_subtext(&completion);
    assert_eq!(
        super::super::formatting::discord_message_units(&expected),
        703,
        "rendered footer fixture unit count"
    );

    let composed = super::compose_completion_footer_text("", Some(&completion));

    assert_eq!(
        composed, expected,
        "in-limit footer-only Korean payload must not be byte-clamped"
    );
}

#[test]
fn streaming_placeholder_counts_supplementary_utf16_units_5178() {
    let body = "😀".repeat(1_200);
    assert_eq!(
        super::super::formatting::discord_message_units(&body),
        2_400,
        "fixture unit count"
    );
    let rendered = super::super::formatting::build_streaming_placeholder_text(&body, "status");
    assert_eq!(
        super::super::formatting::discord_message_units(&rendered),
        1_981,
        "retired placeholder preserves 1981 body units without the 8-unit footer"
    );
    assert!(rendered.starts_with('…'));
}

#[test]
fn streaming_rollover_limits_supplementary_frozen_chunk_units_5177() {
    let body = "😀".repeat(1_100);
    let plan = super::super::formatting::plan_streaming_rollover(&body, "STATUS")
        .expect("2200 message units must roll over before delivery");

    assert_eq!(
        super::super::formatting::discord_message_units(&plan.frozen_chunk),
        1_982,
        "frozen body reserves the 8-unit footer and 10-unit margin"
    );
    assert!(
        super::super::formatting::discord_message_units(&plan.display_snapshot)
            <= DISCORD_MSG_LIMIT
    );
    assert_eq!(
        format!("{}{}", plan.frozen_chunk, &body[plan.split_at..]),
        body
    );
}

#[test]
fn subtext_completion_footer_is_stripped_at_terminal_reconciliation_4080() {
    let text = "Final answer\n\n-# 📦 10 / 100 (10%) · auto-compact 50%\n\n-# Tasks\n-# └ Bash Done ✓\n-# ⏱ 2m 34s";
    assert_eq!(
        super::strip_streaming_footer(text, &ProviderKind::Claude),
        Some("Final answer".to_string())
    );
}

#[test]
fn compose_completion_footer_keeps_balanced_body_fence_intact_3394() {
    let body = "intro\n\n```text\nclosed body\n```\noutro";
    let footer = "📦 1.0k / 1.0M (1%)\n\nTasks\n└ Bash Done ✓";
    let composed = super::compose_completion_footer_text(body, Some(footer));

    assert!(composed.contains("```text\nclosed body\n```"));
    assert!(composed.contains("Bash Done ✓"));
    assert_eq!(composed.matches("```").count() % 2, 0);
}

#[test]
fn footer_panel_clamp_keeps_marker_inside_subtext_at_exact_boundary_4848() {
    let first_line = format!(
        "-# {}",
        "x".repeat(
            super::SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES - "-# ".len() - 1 - "-# …".len()
        )
    );
    let panel = format!("🟢 진행 중 — Claude (<t:1700000000:R>)\n{first_line}\n-# overflow");
    let block = super::compose_footer_status_block("⠸", &panel);
    let panel = panel_portion(&block);

    assert_eq!(
        panel.len(),
        super::SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES
    );
    assert!(
        panel.ends_with("\n-# …"),
        "marker escaped subtext: {panel:?}"
    );
    assert!(
        panel
            .lines()
            .filter(|line| !line.is_empty())
            .all(|line| line.starts_with("-# ")),
        "every non-empty clamped line must remain subtext: {panel:?}"
    );
}

#[test]
fn footer_panel_truncates_on_line_boundaries_s3() {
    let second = "a".repeat(500);
    let third = "b".repeat(500);
    let fourth = "c".repeat(500);
    let panel = format!("🟢 진행 중 — Claude (<t:1700000000:R>)\n\n{second}\n{third}\n{fourth}");
    let block = super::compose_footer_status_block("⠸", &panel);
    let truncated_lines: Vec<&str> = panel_portion(&block).lines().collect();

    let expected_second = format!("-# {second}");
    let expected_third = format!("-# {third}");
    assert_eq!(
        truncated_lines,
        vec![
            "",
            expected_second.as_str(),
            expected_third.as_str(),
            "-# …"
        ]
    );
    assert!(!panel_portion(&block).contains(fourth.as_str()));
    assert!(panel_portion(&block).len() <= super::SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES);
}

#[test]
fn footer_panel_byte_clamps_first_line_on_char_boundary_s3() {
    let panel_body_first_line = "가🙂".repeat(200);
    let panel = format!(
        "🟢 진행 중 — Claude (<t:1700000000:R>)\n{panel_body_first_line}\nSubagents\n└ reviewer inspect"
    );
    let block = super::compose_footer_status_block("⠸", &panel);
    let panel = panel_portion(&block);
    let panel_lines: Vec<&str> = panel.lines().collect();

    assert!(panel.len() <= super::SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES);
    assert_eq!(panel_lines.last().copied(), Some("-# …"));
    assert_eq!(panel_lines.len(), 2);
    assert!(!panel.contains("Subagents"));
}

#[test]
fn footer_rollover_reservation_is_bound_by_panel_budget_s3() {
    const STREAMING_PLACEHOLDER_MARGIN_CHARS: usize = 10;

    let huge_panel = format!(
        "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nTools\n{}",
        "└ cargo test --lib single_message_panel ".repeat(120)
    );
    let status_block = super::compose_footer_status_block("⠸", &huge_panel);
    let merged_header = footer_header(&status_block);
    let max_footer_chars =
        2 + merged_header.chars().count() + 1 + super::SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES;
    let footer = format!("\n\n{status_block}");
    let footer_chars = footer.chars().count();
    let expected_body_budget = DISCORD_MSG_LIMIT
        .saturating_sub(footer_chars + STREAMING_PLACEHOLDER_MARGIN_CHARS)
        .max(1);
    let minimum_body_budget = DISCORD_MSG_LIMIT
        .saturating_sub(max_footer_chars + STREAMING_PLACEHOLDER_MARGIN_CHARS)
        .max(1);
    let current_portion = "x".repeat(expected_body_budget + 1);
    let plan = super::super::formatting::plan_streaming_rollover(&current_portion, &status_block)
        .expect("body should roll over after reserving the bounded footer");

    assert!(footer_chars <= max_footer_chars);
    assert_eq!(plan.split_at, expected_body_budget);
    assert!(plan.split_at >= minimum_body_budget);
    assert!(!plan.display_snapshot.ends_with(&footer));
}

#[test]
fn footer_rollover_seed_carries_merged_header_but_frozen_chunk_does_not() {
    let panel =
        "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nTools\n└ cargo test --lib single_message_panel";
    let status_block = super::compose_footer_status_block("⠸", panel);
    let current_portion = "streamed body ".repeat(220);
    let plan = super::super::formatting::plan_streaming_rollover(&current_portion, &status_block)
        .expect("body should roll over after reserving the footer");
    let seed = super::super::formatting::build_streaming_placeholder_text("", &status_block);

    assert_eq!(seed, "⠋ 계속 처리 중");
    assert!(!seed.contains("-# Tools\n-# └ cargo test --lib single_message_panel"));
    assert!(!plan.frozen_chunk.contains("진행 중 — Claude"));
    assert!(!plan.frozen_chunk.contains("-# Tools\n-# └ cargo test"));
    assert!(!plan.display_snapshot.contains(&status_block));
}

#[test]
fn terminal_footer_strip_removes_panel_block() {
    let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
    let rendered = format!(
        "Final answer\n\n{}",
        super::compose_footer_status_block("⠸", panel)
    );
    let finalized = super::finalize_streaming_footer(&rendered, &ProviderKind::Claude)
        .expect("panel footer should strip at terminal reconciliation");

    assert_eq!(finalized, "Final answer");
    assert!(!finalized.contains("계속 처리 중"));
    assert!(!finalized.contains("진행 중 — Claude"));
    assert!(!finalized.contains("Subagents"));
}

#[test]
fn terminal_footer_strip_preserves_body_text_that_mentions_running_status() {
    let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
    let body = "Final answer\n\n본문에 진행 중 문구가 있어도 유지";
    let rendered = format!(
        "{body}\n\n{}",
        super::compose_footer_status_block("⠸", panel)
    );
    let finalized = super::finalize_streaming_footer(&rendered, &ProviderKind::Claude)
        .expect("merged footer should strip at terminal reconciliation");

    assert_eq!(finalized, body);
}

#[test]
fn footer_mode_strip_preserves_spinner_prefixed_user_body_without_panel_timestamp() {
    let body = "Final answer\n\n⠋ 진행 중 — user-authored line";

    assert_eq!(
        super::strip_streaming_footer(body, &ProviderKind::Claude),
        None
    );
    assert_eq!(
        super::finalize_streaming_footer(body, &ProviderKind::Claude),
        None
    );
}

#[test]
fn footer_only_body_strips_to_empty_for_cleanup_callers() {
    let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
    let rendered = super::compose_footer_status_block("⠸", panel);

    assert_eq!(
        super::strip_streaming_footer(&rendered, &ProviderKind::Claude),
        Some(String::new())
    );
    assert_eq!(
        super::finalize_streaming_footer(&rendered, &ProviderKind::Claude),
        None
    );
}

#[test]
fn subtext_footer_only_completion_surface_is_exposed_4080() {
    let footer = super::completion_footer_subtext(
        "📦 10 / 100 (10%) · auto-compact 50%\n\nTasks\n└ Bash Done ✓\n⏱ 2m 34s",
    );

    assert!(super::streaming_footer_only_surface_was_exposed(
        &footer,
        &ProviderKind::Claude
    ));
    assert_eq!(
        super::strip_streaming_footer(&footer, &ProviderKind::Claude),
        Some(String::new())
    );
}

#[test]
fn footer_only_surface_recognizes_split_fixed_kst_time_lines() {
    let panel = "🟢 진행 중\n턴 트리거: https://discord.com/channels/1/2/3\n턴 시작 : 11-15 07:13:20 (<t:1700000000:R>)\n마지막 업데이트 : 11-15 07:18:20 (<t:1700000300:R>)";
    let rendered = super::compose_footer_status_block("⠸", panel);

    assert!(super::streaming_footer_only_surface_was_exposed(
        &rendered,
        &ProviderKind::Claude
    ));
}

#[test]
fn footer_only_surface_recognizes_legacy_combined_time_line() {
    let panel = "🟢 진행 중\n\n마지막 업데이트 : 11-15 07:18:20 (<t:1700000300:R>) / 턴 시작 : 11-15 07:13:20 (<t:1700000000:R>)";
    let rendered = super::compose_footer_status_block("⠸", panel);

    assert!(super::streaming_footer_only_surface_was_exposed(
        &rendered,
        &ProviderKind::Claude
    ));
}

#[test]
fn terminal_footer_replacement_keeps_completion_context_block() {
    let panel = "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n└ review inspect";
    let rendered = format!(
        "Final answer\n\n{}",
        super::compose_footer_status_block("⠸", panel)
    );
    let completion = "📦 154.6k / 1.0M (15%) · auto-compact 60%";

    let finalized = super::finalize_streaming_footer_with_completion(
        &rendered,
        &ProviderKind::Claude,
        Some(completion),
    )
    .expect("streaming footer should be replaced by completion block");

    assert_eq!(finalized, format!("Final answer\n\n-# {completion}"));
    assert!(!finalized.contains("진행 중 — Claude"));
    assert!(!finalized.contains("Subagents"));
}

#[test]
fn legacy_context_footer_is_stripped_during_format_transition_4846() {
    let legacy = "Final answer\n\n-# Context   📦 154.6k / 1.0M tokens (15%) · auto-compact 60%\n\n-# Tasks\n-# └ Bash Done ✓";

    assert_eq!(
        super::strip_streaming_footer(legacy, &ProviderKind::Claude),
        Some("Final answer".to_string())
    );
}

#[test]
fn completion_footer_strip_supports_suppression_exposure_test() {
    let completion =
        "📦 154.6k / 1.0M (15%) · auto-compact 60%\n\nSubagents\n└ bgworker Long job ✓";

    assert_eq!(
        super::strip_streaming_footer(completion, &ProviderKind::Claude),
        Some(String::new())
    );
    assert_eq!(
        super::strip_streaming_footer(
            &format!("visible assistant body\n\n{completion}"),
            &ProviderKind::Claude,
        ),
        Some("visible assistant body".to_string())
    );
}

#[test]
fn completion_footer_strip_preserves_bare_user_section_heading_without_slot_evidence() {
    let body = "visible assistant body\n\nSubagents\n- user-authored note";

    assert_eq!(
        super::strip_streaming_footer(body, &ProviderKind::Claude),
        None
    );
    assert_eq!(
        super::finalize_streaming_footer(body, &ProviderKind::Claude),
        None
    );
}

#[test]
fn completion_footer_strip_still_removes_real_slot_section() {
    let body = "visible assistant body\n\nSubagents\n└ bgworker Long job ✓";

    assert_eq!(
        super::strip_streaming_footer(body, &ProviderKind::Claude),
        Some("visible assistant body".to_string())
    );
}

#[test]
fn completion_footer_strip_removes_frozen_supersede_shape() {
    let completion =
        "📦 154.6k / 1.0M (15%) · auto-compact 60%\n\nSubagents\n└ bgworker Long job …";

    assert_eq!(
        super::strip_streaming_footer(
            &format!("visible assistant body\n\n{completion}"),
            &ProviderKind::Claude,
        ),
        Some("visible assistant body".to_string())
    );
}

#[test]
fn registering_new_target_supersedes_old_footer_once_and_keeps_snapshot() {
    let channel_id = ChannelId::new(3_089_021);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    assert!(
        shared
            .ui
            .placeholder_live_events
            .set_context_panel_usage(channel_id, None, 154_600, 0, 0, 1_000_000, 60,)
    );
    let old_block = shared
        .ui
        .placeholder_live_events
        .render_completion_footer(channel_id, &ProviderKind::Claude, "⠸")
        .block
        .expect("old footer block");
    assert!(old_block.contains('⠸'));
    assert!(old_block.contains("📦 "));
    assert!(old_block.contains("Subagents\n└ "));

    assert_eq!(
        footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_089_121),
            &ProviderKind::Claude,
            1_800_000_000,
            "Old answer",
            Some(&old_block),
            true,
        ),
        None
    );
    let new_block = old_block.replace('⠸', "⠼");
    let supersede = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_089_122),
        &ProviderKind::Claude,
        1_800_000_010,
        "New answer",
        Some(&new_block),
        true,
    )
    .expect("new target should supersede old target");

    assert_eq!(supersede.message_id, MessageId::new(3_089_121));
    assert!(supersede.remove_after_edit);
    assert!(supersede.text.starts_with("Old answer\n\n-# 📦 "));
    assert!(supersede.text.contains("-# Subagents\n-# └ "));
    assert!(supersede.text.contains('…'));
    assert!(!supersede.text.contains('⠸'));
    assert!(!supersede.text.contains('⠼'));

    let latest = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠋",
        1_800_000_011,
    )
    .expect("new target should be the only registered target");
    assert_eq!(latest.message_id, MessageId::new(3_089_122));
    assert!(!latest.text.contains("Old answer"));
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn carried_entry_notification_updates_latest_target_not_superseded_text() {
    let channel_id = ChannelId::new(3_089_022);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = super::super::make_shared_data_for_tests();
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::BackgroundTaskStart {
            name: "Bash".to_string(),
            summary: "Carried bash".to_string(),
            tool_use_id: "toolu_latest_bash".to_string(),
        },
    );
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::SubagentStart {
            subagent_type: Some("bgworker".to_string()),
            desc: Some("Carried agent".to_string()),
            agent_id: None,
            tool_use_id: Some("toolu_latest_agent".to_string()),
            background: true,
        },
    );
    let old_block = shared
        .ui
        .placeholder_live_events
        .render_completion_footer(channel_id, &ProviderKind::Claude, "⠸")
        .block
        .expect("old footer block");
    let _ = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_089_221),
        &ProviderKind::Claude,
        1_800_000_000,
        "Old answer",
        Some(&old_block),
        true,
    );
    shared
        .ui
        .placeholder_live_events
        .clear_channel_preserving_footer_residuals(channel_id);
    let carried_block = shared
        .ui
        .placeholder_live_events
        .render_completion_footer(channel_id, &ProviderKind::Claude, "⠼")
        .block
        .expect("carried footer block");
    let supersede = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_089_222),
        &ProviderKind::Claude,
        1_800_000_010,
        "New answer",
        Some(&carried_block),
        true,
    )
    .expect("new target should supersede old target");
    assert_eq!(supersede.message_id, MessageId::new(3_089_221));
    assert!(supersede.text.contains("Carried bash …"));
    assert!(supersede.text.contains("Carried agent …"));
    assert!(!supersede.text.contains('✓'));

    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::BackgroundTaskEnd {
            tool_use_id: "toolu_latest_bash".to_string(),
            success: true,
        },
    );
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::SubagentEnd {
            success: true,
            agent_id: None,
            desc: None,
            tool_use_id: Some("toolu_latest_agent".to_string()),
            summary: None,
            ack_only: false,
        },
    );
    let latest = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠋",
        1_800_000_011,
    )
    .expect("latest target should receive finalization");
    assert_eq!(latest.message_id, MessageId::new(3_089_222));
    assert!(latest.text.contains("Carried bash ✓"));
    assert!(latest.text.contains("Carried agent ✓"));
    assert!(!supersede.text.contains('✓'));
    footer_registry::completion_footer_record_edit_result_for_edit(
        shared.as_ref(),
        channel_id,
        &latest,
        true,
    );
    assert!(!footer_registry::completion_footer_has_registered_target(
        channel_id
    ));
}

#[test]
fn completion_footer_terminal_mark_renders_once_then_next_edit_drops_it() {
    let channel_id = ChannelId::new(3_391_101);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_finished_and_running_background_tasks(channel_id);
    let _ = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_391_201),
        &ProviderKind::Claude,
        1_800_000_000,
        "Final answer",
        None,
        true,
    );

    let edit = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠸",
        1_800_000_001,
    )
    .expect("first refresh should render the terminal mark");
    assert!(edit.text.contains("Bash Finished job ✓"));
    assert!(edit.text.contains("Bash Running job ⠸"));
    assert!(!edit.remove_after_edit);

    footer_registry::completion_footer_record_edit_result_for_edit(
        shared.as_ref(),
        channel_id,
        &edit,
        true,
    );

    let next = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠼",
        1_800_000_002,
    )
    .expect("running entry keeps the target registered");
    assert!(!next.text.contains("Finished job"));
    assert!(next.text.contains("Bash Running job ⠼"));
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn completion_footer_failed_delivery_retries_terminal_mark() {
    let channel_id = ChannelId::new(3_391_102);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_finished_and_running_background_tasks(channel_id);
    let _ = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_391_202),
        &ProviderKind::Claude,
        1_800_000_000,
        "Final answer",
        None,
        true,
    );
    let edit = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠸",
        1_800_000_001,
    )
    .expect("first refresh should render the terminal mark");

    footer_registry::completion_footer_record_edit_result_for_edit(
        shared.as_ref(),
        channel_id,
        &edit,
        false,
    );

    let retry = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠼",
        1_800_000_002,
    )
    .expect("failed edit keeps the target registered");
    assert!(retry.text.contains("Bash Finished job ✓"));
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn completion_footer_failed_final_edit_keeps_target_then_retry_commits_4025() {
    let channel_id = ChannelId::new(4_025_001);
    let message_id = MessageId::new(4_025_101);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_background_task(channel_id);
    let _ = footer_registry::register_completion_footer_target(
        channel_id,
        message_id,
        &ProviderKind::Claude,
        1_800_000_000,
        "Final answer",
        None,
        true,
    );
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::BackgroundTaskEnd {
            tool_use_id: format!("tool-{}", channel_id.get()),
            success: true,
        },
    );

    let failed_edit = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠸",
        1_800_000_001,
    )
    .expect("final terminal refresh should target the registered message");
    assert_eq!(failed_edit.message_id, message_id);
    assert!(failed_edit.text.contains("Bash Run background codex ✓"));
    assert!(failed_edit.remove_after_edit);

    assert!(
        footer_registry::completion_footer_record_edit_result_for_edit(
            shared.as_ref(),
            channel_id,
            &failed_edit,
            false,
        )
    );
    assert!(
        footer_registry::completion_footer_has_registered_target(channel_id),
        "failed final edit must retain its retry target"
    );
    assert_eq!(
        footer_registry::completion_footer_registered_failure_count(channel_id),
        Some(1),
        "failed final edit must count toward the existing retry cap"
    );
    let retained = shared.ui.placeholder_live_events.render_completion_footer(
        channel_id,
        &ProviderKind::Claude,
        "⠼",
    );
    assert!(
        retained
            .block
            .as_deref()
            .is_some_and(|block| block.contains("Bash Run background codex ✓")),
        "failed delivery must not evict the terminal slot"
    );

    let retry = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠼",
        1_800_000_002,
    )
    .expect("failed final edit should remain available for retry");
    assert_eq!(retry.message_id, failed_edit.message_id);
    assert_eq!(retry.text, failed_edit.text);
    assert!(retry.remove_after_edit);

    assert!(
        footer_registry::completion_footer_record_edit_result_for_edit(
            shared.as_ref(),
            channel_id,
            &retry,
            true,
        )
    );
    assert!(!footer_registry::completion_footer_has_registered_target(
        channel_id
    ));
    assert_eq!(
        shared
            .ui
            .placeholder_live_events
            .render_completion_footer(channel_id, &ProviderKind::Claude, "⠴")
            .block,
        None,
        "terminal slot must be evicted only after successful recorded delivery"
    );
}

#[test]
fn completion_footer_evicts_all_terminal_marks_delivered_by_one_edit() {
    let channel_id = ChannelId::new(3_391_103);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_finished_and_running_background_tasks(channel_id);
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::BackgroundTaskStart {
            name: "Bash".to_string(),
            summary: "Failed sweep".to_string(),
            tool_use_id: "tool-fail-3391103".to_string(),
        },
    );
    shared.ui.placeholder_live_events.push_status_event(
        channel_id,
        StatusEvent::BackgroundTaskEnd {
            tool_use_id: "tool-fail-3391103".to_string(),
            success: false,
        },
    );
    let _ = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_391_203),
        &ProviderKind::Claude,
        1_800_000_000,
        "Final answer",
        None,
        true,
    );
    let edit = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠸",
        1_800_000_001,
    )
    .expect("first refresh should render both terminal marks");
    assert!(edit.text.contains("Bash Finished job ✓"));
    assert!(edit.text.contains("Bash Failed sweep ✗"));

    footer_registry::completion_footer_record_edit_result_for_edit(
        shared.as_ref(),
        channel_id,
        &edit,
        true,
    );

    let next = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠼",
        1_800_000_002,
    )
    .expect("running entry keeps the target registered");
    assert!(!next.text.contains("Finished job"));
    assert!(!next.text.contains("Failed sweep"));
    assert!(next.text.contains("Bash Running job ⠼"));
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn migration_does_not_carry_delivered_terminal_marks_to_new_target() {
    let channel_id = ChannelId::new(3_391_104);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_finished_and_running_background_tasks(channel_id);
    let old_block = shared
        .ui
        .placeholder_live_events
        .render_completion_footer(channel_id, &ProviderKind::Claude, "⠸")
        .block
        .expect("old footer block");
    assert_eq!(
        footer_registry::register_completion_footer_target(
            channel_id,
            MessageId::new(3_391_204),
            &ProviderKind::Claude,
            1_800_000_000,
            "Old answer",
            Some(&old_block),
            true,
        ),
        None
    );
    let edit = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠸",
        1_800_000_001,
    )
    .expect("old target refresh");
    assert!(edit.text.contains("Bash Finished job ✓"));
    footer_registry::completion_footer_record_edit_result_for_edit(
        shared.as_ref(),
        channel_id,
        &edit,
        true,
    );

    // #3386 migration: the channel footer moves to a newer message. The new
    // footer must not carry the already-delivered terminal mark, while the
    // frozen snapshot of the superseded message keeps it (that delivered
    // render IS "the once").
    let new_block = shared
        .ui
        .placeholder_live_events
        .render_completion_footer(channel_id, &ProviderKind::Claude, "⠼")
        .block
        .expect("running entry still renders");
    assert!(!new_block.contains("Finished job"));
    let supersede = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_391_205),
        &ProviderKind::Claude,
        1_800_000_010,
        "New answer",
        Some(&new_block),
        true,
    )
    .expect("new target should supersede old target");
    assert_eq!(supersede.message_id, MessageId::new(3_391_204));
    assert!(supersede.delivered_terminal_ids.is_empty());
    assert!(supersede.text.contains("Bash Finished job ✓"));
    assert!(supersede.text.contains("Bash Running job …"));

    let latest = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠋",
        1_800_000_011,
    )
    .expect("new target refresh");
    assert_eq!(latest.message_id, MessageId::new(3_391_205));
    assert!(!latest.text.contains("Finished job"));
    assert!(latest.text.contains("Bash Running job ⠋"));
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn ttl_freezes_carried_entries_on_latest_target() {
    let channel_id = ChannelId::new(3_089_023);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_background_task(channel_id);
    shared
        .ui
        .placeholder_live_events
        .clear_channel_preserving_footer_residuals(channel_id);
    let carried_block = shared
        .ui
        .placeholder_live_events
        .render_completion_footer(channel_id, &ProviderKind::Claude, "⠸")
        .block
        .expect("carried footer block");
    let now = 1_800_000_000;
    let _ = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_089_123),
        &ProviderKind::Claude,
        now - super::COMPLETION_FOOTER_MAX_IDLE_ANIMATION_SECS - 1,
        "Latest answer",
        Some(&carried_block),
        true,
    );

    let edit = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠼",
        now,
    )
    .expect("carried latest target should receive TTL freeze edit");

    assert_eq!(edit.message_id, MessageId::new(3_089_123));
    assert!(edit.remove_after_edit);
    assert!(
        edit.text
            .contains("-# Tasks\n-# └ Bash Run background codex …")
    );
    assert!(!edit.text.contains('⠼'));
    assert!(!edit.text.contains('✓'));
    footer_registry::completion_footer_record_edit_result_for_edit(
        shared.as_ref(),
        channel_id,
        &edit,
        true,
    );
    assert!(!footer_registry::completion_footer_has_registered_target(
        channel_id
    ));
}

#[test]
fn completion_footer_ttl_freezes_unfinished_entries_then_forgets_target() {
    let channel_id = ChannelId::new(3_089_001);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let now = 1_800_000_000;
    let _ = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_089_101),
        &ProviderKind::Claude,
        now - super::COMPLETION_FOOTER_MAX_IDLE_ANIMATION_SECS - 1,
        "Final answer",
        None,
        true,
    );

    let edit = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠸",
        now,
    )
    .expect("expired unfinished footer should render one freeze edit");

    assert!(edit.remove_after_edit);
    assert!(edit.text.contains("-# Subagents\n-# └"));
    assert!(edit.text.contains('…'));
    assert!(!edit.text.contains('⠸'));
    assert!(!edit.text.contains('✓'));

    footer_registry::completion_footer_record_edit_result(channel_id, edit.remove_after_edit, true);
    assert!(!footer_registry::completion_footer_has_registered_target(
        channel_id
    ));
}

#[test]
fn completion_footer_ttl_freezes_unfinished_background_bash_task() {
    let channel_id = ChannelId::new(3_089_011);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_background_task(channel_id);
    let now = 1_800_000_000;
    let _ = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_089_111),
        &ProviderKind::Claude,
        now - super::COMPLETION_FOOTER_MAX_IDLE_ANIMATION_SECS - 1,
        "Final answer",
        None,
        true,
    );

    let edit = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠸",
        now,
    )
    .expect("expired unfinished background Bash footer should render one freeze edit");

    assert!(edit.remove_after_edit);
    assert!(
        edit.text
            .contains("-# Tasks\n-# └ Bash Run background codex")
    );
    assert!(edit.text.contains('…'));
    assert!(!edit.text.contains('⠸'));
    assert!(!edit.text.contains('✓'));

    footer_registry::completion_footer_record_edit_result(channel_id, edit.remove_after_edit, true);
    assert!(!footer_registry::completion_footer_has_registered_target(
        channel_id
    ));
}

#[test]
fn completion_footer_below_ttl_keeps_animating_registered_target() {
    let channel_id = ChannelId::new(3_089_002);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let now = 1_800_000_000;
    let _ = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_089_102),
        &ProviderKind::Claude,
        now - super::COMPLETION_FOOTER_MAX_IDLE_ANIMATION_SECS + 1,
        "Final answer",
        None,
        true,
    );

    let edit = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠸",
        now,
    )
    .expect("non-expired unfinished footer should render an animated edit");

    assert!(!edit.remove_after_edit);
    assert!(edit.text.contains("-# Subagents\n-# └"));
    assert!(edit.text.contains('⠸'));

    footer_registry::completion_footer_record_edit_result(channel_id, edit.remove_after_edit, true);
    assert!(footer_registry::completion_footer_has_registered_target(
        channel_id
    ));
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn completion_footer_skips_refresh_when_committed_text_is_unchanged_3717() {
    let channel_id = ChannelId::new(3_717_001);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let now = 1_800_000_000;
    let _ = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_717_101),
        &ProviderKind::Claude,
        now,
        "Final answer",
        None,
        true,
    );

    let first = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠸",
        now + 1,
    )
    .expect("first refresh should edit the footer");
    assert!(first.text.contains('⠸'));
    footer_registry::completion_footer_record_edit_result_for_edit(
        shared.as_ref(),
        channel_id,
        &first,
        true,
    );

    assert_eq!(
        footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            now + 2,
        ),
        None,
        "same rendered footer text must not issue a redundant Discord edit"
    );

    let next = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠼",
        now + 3,
    )
    .expect("changed spinner frame still edits the stable target");
    assert_eq!(next.message_id, MessageId::new(3_717_101));
    assert!(next.text.contains('⠼'));
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn duplicate_same_message_registration_preserves_committed_noop_state_3717() {
    let channel_id = ChannelId::new(3_717_002);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let msg_id = MessageId::new(3_717_102);
    let now = 1_800_000_000;
    let _ = footer_registry::register_completion_footer_target(
        channel_id,
        msg_id,
        &ProviderKind::Claude,
        now,
        "Final answer",
        None,
        true,
    );
    let first = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠸",
        now + 1,
    )
    .expect("first refresh should edit the footer");
    footer_registry::completion_footer_record_edit_result_for_edit(
        shared.as_ref(),
        channel_id,
        &first,
        true,
    );

    assert_eq!(
        footer_registry::register_completion_footer_target(
            channel_id,
            msg_id,
            &ProviderKind::Claude,
            now + 2,
            "Final answer",
            first.completion_block.as_deref(),
            true,
        ),
        None,
        "same message id is a duplicate registration, not a supersede"
    );
    assert_eq!(
        footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            now + 3,
        ),
        None,
        "duplicate registration must not lose the committed-text dedupe state"
    );

    let changed = footer_registry::completion_footer_edit_for_registered_target_at(
        shared.as_ref(),
        channel_id,
        "⠼",
        now + 4,
    )
    .expect("changed content still edits the original stable target");
    assert_eq!(changed.message_id, msg_id);
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn stale_owner_cannot_refresh_newer_footer_target_3717() {
    let channel_id = ChannelId::new(3_717_003);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let old_owner = footer_registry::CompletionFooterOwner::new(10, 1_800_000_000);
    let new_owner = footer_registry::CompletionFooterOwner::new(11, 1_800_000_010);
    let _ = footer_registry::register_completion_footer_target_for_owner(
        channel_id,
        MessageId::new(3_717_103),
        new_owner,
        &ProviderKind::Claude,
        new_owner.started_at_unix,
        "New answer",
        None,
        true,
    );

    assert_eq!(
        footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
            shared.as_ref(),
            channel_id,
            Some(old_owner),
            "⠸",
            1_800_000_011,
        ),
        None,
        "older owner must not edit the newer registered footer target"
    );

    let edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
        shared.as_ref(),
        channel_id,
        Some(new_owner),
        "⠸",
        1_800_000_012,
    )
    .expect("registered owner should refresh its target");
    assert_eq!(edit.message_id, MessageId::new(3_717_103));
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn stale_owner_cannot_register_over_newer_footer_target_3717() {
    let channel_id = ChannelId::new(3_717_007);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let old_owner = footer_registry::CompletionFooterOwner::new(50, 1_800_000_000);
    let new_owner = footer_registry::CompletionFooterOwner::new(51, 1_800_000_010);
    let _ = footer_registry::register_completion_footer_target_for_owner(
        channel_id,
        MessageId::new(3_717_108),
        new_owner,
        &ProviderKind::Claude,
        new_owner.started_at_unix,
        "New answer",
        None,
        true,
    );

    assert_eq!(
        footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_109),
            old_owner,
            &ProviderKind::Claude,
            old_owner.started_at_unix,
            "Old answer",
            None,
            true,
        ),
        None,
        "older completion must not replace the newer registered footer target"
    );

    let edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
        shared.as_ref(),
        channel_id,
        Some(new_owner),
        "⠸",
        1_800_000_011,
    )
    .expect("newer target should remain registered after stale register attempt");
    assert_eq!(edit.message_id, MessageId::new(3_717_108));
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn stale_edit_snapshot_is_not_current_after_target_changes_3717() {
    let channel_id = ChannelId::new(3_717_009);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let old_owner = footer_registry::CompletionFooterOwner::new(70, 1_800_000_000);
    let new_owner = footer_registry::CompletionFooterOwner::new(71, 1_800_000_010);
    let _ = footer_registry::register_completion_footer_target_for_owner(
        channel_id,
        MessageId::new(3_717_112),
        old_owner,
        &ProviderKind::Claude,
        old_owner.started_at_unix,
        "Old answer",
        None,
        true,
    );
    let old_edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
        shared.as_ref(),
        channel_id,
        Some(old_owner),
        "⠸",
        1_800_000_001,
    )
    .expect("old owner should initially receive an edit");
    assert!(footer_registry::completion_footer_edit_still_registered(
        channel_id, &old_edit
    ));

    let _ = footer_registry::register_completion_footer_target_for_owner(
        channel_id,
        MessageId::new(3_717_113),
        new_owner,
        &ProviderKind::Claude,
        new_owner.started_at_unix,
        "New answer",
        None,
        true,
    );

    assert!(
        !footer_registry::completion_footer_edit_still_registered(channel_id, &old_edit),
        "refresh callers must skip an edit snapshot after another target registers"
    );
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn stale_owner_cannot_clear_newer_footer_target_3717() {
    let channel_id = ChannelId::new(3_717_008);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let old_owner = footer_registry::CompletionFooterOwner::new(60, 1_800_000_000);
    let new_owner = footer_registry::CompletionFooterOwner::new(61, 1_800_000_010);
    let _ = footer_registry::register_completion_footer_target_for_owner(
        channel_id,
        MessageId::new(3_717_110),
        new_owner,
        &ProviderKind::Claude,
        new_owner.started_at_unix,
        "New answer",
        None,
        true,
    );

    assert_eq!(
        footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_111),
            old_owner,
            &ProviderKind::Claude,
            old_owner.started_at_unix,
            "Old answer",
            None,
            false,
        ),
        None,
        "older no-residual completion must not clear the newer footer target"
    );

    let edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
        shared.as_ref(),
        channel_id,
        Some(new_owner),
        "⠸",
        1_800_000_011,
    )
    .expect("newer target should remain registered after stale clear attempt");
    assert_eq!(edit.message_id, MessageId::new(3_717_110));
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn stale_edit_result_cannot_remove_newer_footer_target_3717() {
    let channel_id = ChannelId::new(3_717_004);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let old_owner = footer_registry::CompletionFooterOwner::new(20, 1_800_000_000);
    let new_owner = footer_registry::CompletionFooterOwner::new(21, 1_800_000_010);
    let _ = footer_registry::register_completion_footer_target_for_owner(
        channel_id,
        MessageId::new(3_717_104),
        old_owner,
        &ProviderKind::Claude,
        old_owner.started_at_unix,
        "Old answer",
        None,
        true,
    );
    let old_edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
        shared.as_ref(),
        channel_id,
        Some(old_owner),
        "⠸",
        1_800_000_001,
    )
    .expect("old owner should initially receive an edit");
    let _ = footer_registry::register_completion_footer_target_for_owner(
        channel_id,
        MessageId::new(3_717_105),
        new_owner,
        &ProviderKind::Claude,
        new_owner.started_at_unix,
        "New answer",
        None,
        true,
    );

    let mut stale_remove = old_edit.clone();
    stale_remove.remove_after_edit = true;
    footer_registry::completion_footer_record_edit_result_for_edit(
        shared.as_ref(),
        channel_id,
        &stale_remove,
        true,
    );

    let new_edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
        shared.as_ref(),
        channel_id,
        Some(new_owner),
        "⠼",
        1_800_000_011,
    )
    .expect("stale edit result must not remove the newer target");
    assert_eq!(new_edit.message_id, MessageId::new(3_717_105));
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn older_turn_start_cannot_supersede_newer_footer_target_3717() {
    let channel_id = ChannelId::new(3_717_005);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let old_owner = footer_registry::CompletionFooterOwner::new(30, 1_800_000_000);
    let new_owner = footer_registry::CompletionFooterOwner::new(31, 1_800_000_010);
    let _ = footer_registry::register_completion_footer_target_for_owner(
        channel_id,
        MessageId::new(3_717_106),
        new_owner,
        &ProviderKind::Claude,
        new_owner.started_at_unix,
        "New answer",
        None,
        true,
    );

    assert_eq!(
        footer_registry::completion_footer_supersede_registered_target_for_owner(
            channel_id,
            Some(old_owner)
        ),
        None,
        "older turn-start cleanup must not supersede the newer footer target"
    );
    assert!(
        footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
            shared.as_ref(),
            channel_id,
            Some(new_owner),
            "⠸",
            1_800_000_011,
        )
        .is_some(),
        "newer target should remain registered after stale supersede attempt"
    );
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn same_second_lower_snowflake_cannot_supersede_higher_footer_owner_3717() {
    let channel_id = ChannelId::new(3_717_006);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let old_owner = footer_registry::CompletionFooterOwner::new(40, 1_800_000_000);
    let new_owner = footer_registry::CompletionFooterOwner::new(41, 1_800_000_000);
    let _ = footer_registry::register_completion_footer_target_for_owner(
        channel_id,
        MessageId::new(3_717_107),
        new_owner,
        &ProviderKind::Claude,
        new_owner.started_at_unix,
        "New answer",
        None,
        true,
    );

    assert_eq!(
        footer_registry::completion_footer_supersede_registered_target_for_owner(
            channel_id,
            Some(old_owner)
        ),
        None,
        "same-second stale owner with lower Discord snowflake must not supersede"
    );
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn same_second_unknown_owner_does_not_block_known_owner_3717() {
    let channel_id = ChannelId::new(3_717_010);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let unknown_owner = footer_registry::CompletionFooterOwner::new(0, 1_800_000_000);
    let known_owner = footer_registry::CompletionFooterOwner::new(81, 1_800_000_000);
    let _ = footer_registry::register_completion_footer_target_for_owner(
        channel_id,
        MessageId::new(3_717_114),
        unknown_owner,
        &ProviderKind::Claude,
        unknown_owner.started_at_unix,
        "Unknown owner answer",
        None,
        true,
    );

    assert!(
        footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_115),
            known_owner,
            &ProviderKind::Claude,
            known_owner.started_at_unix,
            "Known owner answer",
            None,
            true,
        )
        .is_some(),
        "same-second user_msg_id=0 owner must not be treated as provably newer"
    );
    let edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
        shared.as_ref(),
        channel_id,
        Some(known_owner),
        "⠸",
        1_800_000_001,
    )
    .expect("known owner should replace same-second unknown target");
    assert_eq!(edit.message_id, MessageId::new(3_717_115));
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn same_second_unknown_owner_cannot_register_over_known_owner_3717() {
    let channel_id = ChannelId::new(3_717_011);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let known_owner = footer_registry::CompletionFooterOwner::new(81, 1_800_000_000);
    let unknown_owner = footer_registry::CompletionFooterOwner::new(0, 1_800_000_000);
    let _ = footer_registry::register_completion_footer_target_for_owner(
        channel_id,
        MessageId::new(3_717_116),
        known_owner,
        &ProviderKind::Claude,
        known_owner.started_at_unix,
        "Known owner answer",
        None,
        true,
    );

    assert_eq!(
        footer_registry::register_completion_footer_target_for_owner(
            channel_id,
            MessageId::new(3_717_117),
            unknown_owner,
            &ProviderKind::Claude,
            unknown_owner.started_at_unix,
            "Unknown owner answer",
            None,
            true,
        ),
        None,
        "same-second user_msg_id=0 owner must not replace a known owner"
    );
    let edit = footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
        shared.as_ref(),
        channel_id,
        Some(known_owner),
        "⠸",
        1_800_000_001,
    )
    .expect("known owner should remain registered");
    assert_eq!(edit.message_id, MessageId::new(3_717_116));
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn same_second_unknown_owner_cannot_supersede_known_owner_3717() {
    let channel_id = ChannelId::new(3_717_012);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let known_owner = footer_registry::CompletionFooterOwner::new(82, 1_800_000_000);
    let unknown_owner = footer_registry::CompletionFooterOwner::new(0, 1_800_000_000);
    let _ = footer_registry::register_completion_footer_target_for_owner(
        channel_id,
        MessageId::new(3_717_118),
        known_owner,
        &ProviderKind::Claude,
        known_owner.started_at_unix,
        "Known owner answer",
        None,
        true,
    );

    assert_eq!(
        footer_registry::completion_footer_supersede_registered_target_for_owner(
            channel_id,
            Some(unknown_owner)
        ),
        None,
        "same-second user_msg_id=0 owner must not supersede a known owner"
    );
    assert!(
        footer_registry::completion_footer_edit_for_registered_target_at_for_owner(
            shared.as_ref(),
            channel_id,
            Some(known_owner),
            "⠸",
            1_800_000_001,
        )
        .is_some(),
        "known owner should remain registered after unknown supersede attempt"
    );
    footer_registry::completion_footer_forget_registered_target(channel_id);
}

#[test]
fn completion_footer_consecutive_edit_failures_evict_registered_target() {
    let channel_id = ChannelId::new(3_089_003);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let _ = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_089_103),
        &ProviderKind::Claude,
        1_800_000_000,
        "Final answer",
        None,
        true,
    );

    for expected_failures in 1..super::COMPLETION_FOOTER_MAX_CONSECUTIVE_EDIT_FAILURES {
        footer_registry::completion_footer_record_edit_result(channel_id, false, false);
        assert_eq!(
            footer_registry::completion_footer_registered_failure_count(channel_id),
            Some(expected_failures)
        );
        assert!(footer_registry::completion_footer_has_registered_target(
            channel_id
        ));
    }

    footer_registry::completion_footer_record_edit_result(channel_id, false, false);
    assert!(!footer_registry::completion_footer_has_registered_target(
        channel_id
    ));
    assert_eq!(
        footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            1_800_000_005,
        ),
        None
    );
}

#[test]
fn completion_footer_forget_registered_target_suppresses_future_edits() {
    let channel_id = ChannelId::new(3_089_004);
    footer_registry::completion_footer_forget_registered_target(channel_id);
    let shared = push_unfinished_subagent(channel_id);
    let _ = footer_registry::register_completion_footer_target(
        channel_id,
        MessageId::new(3_089_104),
        &ProviderKind::Claude,
        1_800_000_000,
        "Final answer",
        None,
        true,
    );

    footer_registry::completion_footer_forget_registered_target(channel_id);

    assert_eq!(
        footer_registry::completion_footer_edit_for_registered_target_at(
            shared.as_ref(),
            channel_id,
            "⠸",
            1_800_000_005,
        ),
        None
    );
}

#[test]
fn footer_status_block_stays_within_discord_limit() {
    let huge_panel = format!(
        "🟢 진행 중 — Claude (<t:1700000000:R>)\n\nSubagents\n{}",
        "└ reviewer ".repeat(1_000)
    );
    let status_block = super::compose_footer_status_block("⠸", &huge_panel);
    let rendered =
        super::super::formatting::build_streaming_placeholder_text("body", &status_block);

    assert!(rendered.len() <= DISCORD_MSG_LIMIT);
    assert_eq!(rendered, "body");
}

// ---- codex round-1 High-b: tail-anchored reclaim detection ----

#[test]
fn reclaim_tail_footer_matches_when_footer_is_last_section() {
    // A genuine frozen panel: prose body then a Tasks footer with a spinner
    // as the message's final section. Must reclaim, body preserved verbatim.
    let body = "여기 실제 응답 본문입니다.\n\n두 번째 문단도 보존되어야 합니다.";
    let frozen = format!("{body}\n\nTasks\n└ Bash 백그라운드 실행 ⠧");
    assert!(super::text_has_frozen_spinner_footer(&frozen));
    let out = super::reclaim_finalize_text(&frozen, &ProviderKind::Claude)
        .expect("real tail footer must reclaim");
    assert!(out.starts_with(body), "body must be preserved verbatim");
    assert!(out.contains(super::STARTUP_RECLAIM_STOP_MARKER));
    assert!(!out.contains('⠧'), "spinner must be gone");
}

#[test]
fn reclaim_tail_footer_ignores_mid_body_footer_quote() {
    // This channel posts bot messages that QUOTE a footer/Tasks example in
    // the middle of ordinary prose. The quote is followed by real prose, so
    // the trailing run never reaches it: the message must NOT be reclaimed
    // and the body must never be truncated.
    let quoted = "여기 패널 예시를 인용합니다:\n\nTasks\n└ Bash 실행 중 ⠧\n\n그리고 이것은 인용 뒤에 오는 실제 본문 산문입니다.";
    assert!(
        !super::text_has_frozen_spinner_footer(quoted),
        "mid-body footer quote must not be detected as a frozen tail footer"
    );
    assert_eq!(
        super::reclaim_finalize_text(quoted, &ProviderKind::Claude),
        None,
        "mid-body footer quote must never be stripped (no body truncation)"
    );
}

#[test]
fn reclaim_tail_footer_ignores_mid_body_streaming_status_quote() {
    // Same risk for a streaming status line quoted mid-body.
    let quoted =
        "상태 라인 예시:\n\n⠧ 진행 중 — claude (<t:1700000000:R>)\n\n뒤따르는 실제 본문 산문.";
    assert!(!super::text_has_frozen_spinner_footer(quoted));
    assert_eq!(
        super::reclaim_finalize_text(quoted, &ProviderKind::Claude),
        None
    );
}

#[test]
fn reclaim_tail_footer_ignores_completed_tail_footer() {
    // A completed tail footer (terminal ✓ marks, no live spinner) must not
    // be reclaimed even though it IS the message's last section.
    let done = "완료된 응답 본문.\n\nTasks\n└ Bash 백그라운드 실행 ✓";
    assert!(!super::text_has_frozen_spinner_footer(done));
    assert_eq!(
        super::reclaim_finalize_text(done, &ProviderKind::Claude),
        None
    );
}

#[test]
fn reclaim_tail_footer_matches_clamped_frozen_tail_with_truncation_marker() {
    // codex round-2 Medium: when the frozen footer overran the 600-byte
    // budget, `clamp_completion_task_section` keeps a leading prefix and
    // appends a bare `…` truncation-marker line. The frozen spinner still
    // rides a KEPT slot line, but the marker is the message's last line. The
    // tail walk must pass the marker through and reclaim the clamped tail
    // (previously it bailed on the marker and returned None), stripping BOTH
    // the footer and the marker while preserving the body verbatim.
    let body = "여기 실제 응답 본문입니다.\n\n두 번째 문단도 보존되어야 합니다.";
    // Exact clamp shape: kept prefix (header + frozen slot) then `…` line,
    // mirroring `format!("{prefix}\n{TRUNCATION_MARKER}")`.
    let clamped_footer = "Tasks\n└ Bash 백그라운드 실행 … ⠧\n…";
    let frozen = format!("{body}\n\n{clamped_footer}");
    assert!(
        super::text_has_frozen_spinner_footer(&frozen),
        "clamped frozen tail (footer prefix + `…` marker) must be detected"
    );
    let out = super::reclaim_finalize_text(&frozen, &ProviderKind::Claude)
        .expect("clamped frozen tail must reclaim");
    assert!(out.starts_with(body), "body must be preserved verbatim");
    assert!(out.contains(super::STARTUP_RECLAIM_STOP_MARKER));
    assert!(!out.contains('⠧'), "frozen spinner must be gone");
    assert!(
        !out.contains('…'),
        "trailing truncation marker must be stripped with the footer"
    );
}

#[test]
fn reclaim_tail_footer_matches_frozen_panel_when_body_ends_with_bare_marker() {
    // codex round-3 Medium: the body's own last line is a standalone `…` just
    // above the footer (NOT a clamp marker — the clamp marker only ever sits
    // BELOW the footer). The two-phase walk must still reclaim the real frozen
    // footer below; the body `…` stays in the preserved body. Previously the
    // marker passed through transparently, made `…` the run's first line, and
    // `opens_footer` failed → the frozen panel was missed.
    let body = "본문이 여기서 끝납니다.\n…";
    let frozen = format!("{body}\n\nTasks\n└ Bash 실행 중 ⠧");
    assert!(
        super::text_has_frozen_spinner_footer(&frozen),
        "frozen footer must be detected even when the body ends with a bare `…`"
    );
    let out = super::reclaim_finalize_text(&frozen, &ProviderKind::Claude)
        .expect("frozen footer below a body `…` must reclaim");
    assert!(
        out.starts_with(body),
        "body (including its `…` line) preserved"
    );
    assert!(out.contains('…'), "the body's own `…` line must survive");
    assert!(!out.contains('⠧'), "frozen spinner must be gone");
    assert!(out.contains(super::STARTUP_RECLAIM_STOP_MARKER));
}

#[test]
fn reclaim_tail_footer_matches_clamped_streaming_status_with_marker() {
    // The byte clamp `clamp_footer_panel_text` appends the same `…` marker.
    // A frozen merged/streaming status line (leading-spinner shape) followed
    // by the marker must also reclaim.
    let body = "스트리밍 본문 텍스트.";
    let frozen = format!("{body}\n\n⠧ 진행 중 — claude (<t:1700000000:R>)\n…");
    assert!(super::text_has_frozen_spinner_footer(&frozen));
    let out = super::reclaim_finalize_text(&frozen, &ProviderKind::Claude)
        .expect("clamped frozen streaming tail must reclaim");
    assert!(out.starts_with(body), "body preserved verbatim");
    assert!(!out.contains('⠧'));
    assert!(!out.contains('…'), "marker stripped with footer");
}

#[test]
fn reclaim_tail_footer_ignores_bare_truncation_marker_without_footer() {
    // A bare `…` tail with ordinary prose above it (no footer run) must NOT
    // be reclaimed: the marker passes the walk but the run's first line is
    // prose, which fails `opens_footer`. The body is never touched.
    let prose = "그냥 평범한 본문 산문입니다. 패널 푸터가 없습니다.\n…";
    assert!(
        !super::text_has_frozen_spinner_footer(prose),
        "a lone `…` after prose must not anchor a footer"
    );
    assert_eq!(
        super::reclaim_finalize_text(prose, &ProviderKind::Claude),
        None
    );
}

#[test]
fn reclaim_tail_footer_ignores_mid_body_quote_with_trailing_marker() {
    // codex round-2 Medium High-b re-confirmation: a footer QUOTED mid-body
    // (followed by real prose) must STILL not be reclaimed even though a `…`
    // marker now passes the walk transparently. The trailing prose breaks the
    // walk before it ever reaches the quoted footer, so the marker allowance
    // cannot revive a mid-body quote match.
    let quoted = "여기 패널 예시를 인용합니다:\n\nTasks\n└ Bash 실행 중 ⠧\n…\n\n그리고 이것은 인용 뒤에 오는 실제 본문 산문입니다.";
    assert!(
        !super::text_has_frozen_spinner_footer(quoted),
        "mid-body footer quote (with a marker) must not be a frozen tail footer"
    );
    assert_eq!(
        super::reclaim_finalize_text(quoted, &ProviderKind::Claude),
        None,
        "mid-body footer quote must never be stripped"
    );
}
