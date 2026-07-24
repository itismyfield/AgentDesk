
use super::{
    MonitorHandoffReason, MonitorHandoffStatus, build_monitor_handoff_placeholder,
    build_monitor_handoff_placeholder_with_live_events, build_placeholder_status_block,
    build_status_panel_streaming_edit_text, build_streaming_placeholder_text, format_for_discord,
    format_for_discord_with_provider, format_for_discord_with_status_panel,
    plan_streaming_rollover,
};
use crate::services::provider::ProviderKind;

const LIVENESS_FOOTER: &str = "⠸ 계속 처리 중";

#[test]
fn plan_streaming_rollover_strips_liveness_footer_from_frozen_chunk_s0() {
    let footer = format!("\n\n{LIVENESS_FOOTER}");
    let body_budget = super::DISCORD_MSG_LIMIT
        .saturating_sub(super::char_count(&footer) + super::STREAMING_PLACEHOLDER_MARGIN)
        .max(1);
    let current_portion = "x".repeat(body_budget + 64);

    let plan = plan_streaming_rollover(&current_portion, LIVENESS_FOOTER)
        .expect("current portion should roll over once footer budget is reserved");

    assert_eq!(super::char_count(&plan.frozen_chunk), body_budget);
    assert_eq!(plan.frozen_chunk, &current_portion[..plan.split_at]);
    assert!(!plan.frozen_chunk.contains(LIVENESS_FOOTER));
    assert!(plan.display_snapshot.ends_with(&footer));
}

#[test]
fn rollover_seed_starts_as_liveness_footer_only_s0() {
    let seed = build_streaming_placeholder_text("", LIVENESS_FOOTER);

    assert_eq!(seed, LIVENESS_FOOTER);
}

#[test]
fn plan_streaming_rollover_reserves_footer_length_before_2000_char_limit_s0() {
    let footer = format!("\n\n{LIVENESS_FOOTER}");
    let body_budget = super::DISCORD_MSG_LIMIT
        .saturating_sub(super::char_count(&footer) + super::STREAMING_PLACEHOLDER_MARGIN)
        .max(1);
    let current_portion = "x".repeat(body_budget + 1);
    assert!(super::char_count(&current_portion) < super::DISCORD_MSG_LIMIT);

    let plan = plan_streaming_rollover(&current_portion, LIVENESS_FOOTER)
        .expect("body fits raw Discord limit but not the reserved footer budget");

    assert_eq!(plan.split_at, body_budget);
    assert!(super::char_count(&plan.display_snapshot) <= super::DISCORD_MSG_LIMIT);
    assert!(plan.display_snapshot.ends_with(&footer));
}

#[test]
fn no_rollover_body_and_footer_under_limit_stays_single_message_s0() {
    let current_portion = "short streamed body";
    let rendered = build_streaming_placeholder_text(current_portion, LIVENESS_FOOTER);

    assert!(plan_streaming_rollover(current_portion, LIVENESS_FOOTER).is_none());
    assert_eq!(rendered, format!("{current_portion}\n\n{LIVENESS_FOOTER}"));
    assert!(super::char_count(&rendered) < super::DISCORD_MSG_LIMIT);
}

#[test]
fn empty_body_with_near_limit_footer_stays_footer_only_s0() {
    let oversized_footer = "⠸".repeat(super::DISCORD_MSG_LIMIT);
    let rendered = build_streaming_placeholder_text("", &oversized_footer);

    assert!(plan_streaming_rollover("", &oversized_footer).is_none());
    assert!(super::char_count(&rendered) <= super::DISCORD_MSG_LIMIT);
    assert!(rendered.starts_with('⠸'));
    assert!(!rendered.contains("\n\n"));
}

#[test]
fn single_message_panel_s0_streaming_footer_present_and_final_body_absent() {
    let streamed = build_status_panel_streaming_edit_text(
        "Final answer",
        LIVENESS_FOOTER,
        &ProviderKind::Codex,
    );
    assert_eq!(streamed, "Final answer\n\n⠸ 계속 처리 중");

    let finalized = format_for_discord_with_status_panel(&streamed, &ProviderKind::Codex);
    assert_eq!(finalized, "Final answer");
}

#[test]
fn monitor_handoff_active_keeps_processing_tail_last() {
    let text = build_monitor_handoff_placeholder_with_live_events(
        MonitorHandoffStatus::Active,
        MonitorHandoffReason::AsyncDispatch,
        1_700_000_000,
        Some("⚙ Bash: cargo build"),
        None,
        None,
        None,
        None,
        None,
        Some("```text\n[Bash] cargo build\n```"),
    );

    assert!(text.contains("```text\n[Bash] cargo build\n```"));
    assert!(text.ends_with("⠋ 계속 처리 중 · 시작 <t:1700000000:R>"));
}

#[test]
fn monitor_handoff_terminal_states_drop_processing_tail() {
    let text = build_monitor_handoff_placeholder(
        MonitorHandoffStatus::Completed,
        MonitorHandoffReason::AsyncDispatch,
        1_700_000_000,
        None,
        None,
    );

    assert!(text.starts_with("✅ **응답 완료**\n"));
    assert!(!text.contains("계속 처리 중"));
}

#[test]
fn status_panel_disabled_codex_formatter_keeps_legacy_tool_markers() {
    let input = "[Bash] /bin/zsh -lc \"ls\"\nkeep";
    let output = format_for_discord_with_provider(input, &ProviderKind::Codex);
    assert_eq!(output, "⚙️ Bash\nkeep");
}

#[test]
fn format_for_discord_does_not_insert_blank_line_before_header() {
    let input = "previous line\n## Heading\nfollowing line";
    let output = format_for_discord(input);
    assert_eq!(output, "previous line\n**Heading**\nfollowing line");
}

#[test]
fn format_for_discord_does_not_insert_blank_line_before_list() {
    let input = "lead-in paragraph\n- first item\n- second item\ntrailing line";
    let output = format_for_discord(input);
    assert_eq!(
        output,
        "lead-in paragraph\n- first item\n- second item\ntrailing line"
    );
}

#[test]
fn format_for_discord_preserves_explicit_blank_line_when_agent_provides_one() {
    let input = "first paragraph\n\nsecond paragraph";
    let output = format_for_discord(input);
    assert_eq!(output, "first paragraph\n\nsecond paragraph");
}

#[test]
fn format_for_discord_collapses_codex_quad_newline_to_single_blank_3475() {
    // #3475: codex/adk-cdx bodies arrive with 4-newline paragraph gaps
    // (agent_message chunks each carry their own \n\n after #3431/#3468).
    // The final relayed body must collapse to at most one blank visual line,
    // through the codex-specific formatting entry points.
    let input = "질문은 코드 기준으로 확인.\n\n\n\n검색상 파일과.\n\n\n\n여기서 분기가 하나.";
    let expected = "질문은 코드 기준으로 확인.\n\n검색상 파일과.\n\n여기서 분기가 하나.";

    assert_eq!(format_for_discord(input), expected);
    assert_eq!(
        format_for_discord_with_provider(input, &ProviderKind::Codex),
        expected
    );
    assert_eq!(
        format_for_discord_with_status_panel(input, &ProviderKind::Codex),
        expected
    );
    // No 3+ newline run survives in the relayed body.
    assert!(!format_for_discord(input).contains("\n\n\n"));
}

#[test]
fn format_for_discord_with_provider_hides_raw_subagent_notification() {
    let input = r#"<subagent_notification>
{"agent_path":"/tmp/private-agent","status":{"completed":"Read-only review complete.\n\n1. Check relay path."}}
</subagent_notification>"#;

    let output = format_for_discord_with_provider(input, &ProviderKind::Codex);

    assert!(output.contains("Subagent completed"));
    assert!(output.contains("Read-only review complete."));
    assert!(output.contains("1. Check relay path."));
    assert!(!output.contains("<subagent_notification>"));
    assert!(!output.contains("agent_path"));
    assert!(!output.contains("/tmp/private-agent"));
    assert!(!output.contains("{\""));
}

#[test]
fn format_for_discord_sanitizes_subagent_after_tui_chrome_strip() {
    let input = "No response requested.\n<subagent_notification>{\"agent_path\":\"/tmp/private-agent\",\"status\":{\"completed\":\"Read-only review complete.\"}}</subagent_notification>";

    let output = format_for_discord_with_provider(input, &ProviderKind::Codex);
    assert!(output.contains("Subagent completed"));
    assert!(output.contains("Read-only review complete."));
    assert!(!output.contains("No response requested."));
    assert!(!output.contains("<subagent_notification>"));
    assert!(!output.contains("agent_path"));
    assert!(!output.contains("/tmp/private-agent"));

    let status_panel_output = format_for_discord_with_status_panel(input, &ProviderKind::Codex);
    assert!(status_panel_output.contains("Subagent completed"));
    assert!(!status_panel_output.contains("<subagent_notification>"));
    assert!(!status_panel_output.contains("agent_path"));
}

#[test]
fn format_for_discord_sanitizes_provider_reuse_user_prefixed_subagent_3777() {
    let input = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this \
Codex thread still apply. Treat only this turn's user request, reply context, uploaded \
files, and memory recall below as new actionable input.\n\n\
[User: 0hbujang (ID: 343742347365974026)] \
<subagent_notification>{\"agent_path\":\"/tmp/private-agent\",\"status\":{\"completed\":\"Review complete.\"}}</subagent_notification>";

    let output = format_for_discord_with_provider(input, &ProviderKind::Codex);
    assert!(output.contains("Subagent completed"));
    assert!(output.contains("Review complete."));
    assert!(!output.contains("[Provider Session Reuse]"));
    assert!(!output.contains("[User:"));
    assert!(!output.contains("<subagent_notification>"));
    assert!(!output.contains("agent_path"));
    assert!(!output.contains("/tmp/private-agent"));
}

#[test]
fn format_for_discord_sanitizes_provider_reuse_chrome_then_user_subagent_3818() {
    let input = "[Provider Session Reuse]\n\
The prior authoritative Discord, role, and tool instructions already present in this \
Codex thread still apply. Treat only this turn's user request, reply context, uploaded \
files, and memory recall below as new actionable input.\n\n\
No response requested.\n\
[User: 0hbujang (ID: 343742347365974026)] \
<subagent_notification>{\"agent_path\":\"/tmp/private-agent\",\"status\":{\"completed\":\"Review complete.\"}}</subagent_notification>";

    let output = format_for_discord_with_provider(input, &ProviderKind::Codex);
    assert!(output.contains("Subagent completed"));
    assert!(output.contains("Review complete."));
    assert!(!output.contains("[Provider Session Reuse]"));
    assert!(!output.contains("No response requested."));
    assert!(!output.contains("[User:"));
    assert!(!output.contains("<subagent_notification>"));
    assert!(!output.contains("agent_path"));
    assert!(!output.contains("/tmp/private-agent"));

    let status_panel_output = format_for_discord_with_status_panel(input, &ProviderKind::Codex);
    assert!(status_panel_output.contains("Subagent completed"));
    assert!(!status_panel_output.contains("<subagent_notification>"));
    assert!(!status_panel_output.contains("[User:"));
}

#[test]
fn placeholder_status_block_summarizes_subagent_notification_3818() {
    let input = r#"<subagent_notification>
{"agent_path":"/tmp/private-agent","status":{"completed":"Review complete.\n\nVERDICT: CLEAN"}}
</subagent_notification>"#;

    let from_full_response = build_placeholder_status_block("⠙", None, None, input);
    assert!(from_full_response.contains("Subagent completed"));
    assert!(!from_full_response.contains("<subagent_notification>"));
    assert!(!from_full_response.contains("agent_path"));
    assert!(!from_full_response.contains("/tmp/private-agent"));

    let from_current_tool = build_placeholder_status_block("⠙", None, Some(input), "");
    assert!(from_current_tool.contains("Subagent completed"));
    assert!(!from_current_tool.contains("<subagent_notification>"));
    assert!(!from_current_tool.contains("agent_path"));
    assert!(!from_current_tool.contains("/tmp/private-agent"));
}

#[test]
fn format_for_discord_preserves_blank_lines_inside_code_block_3475() {
    // #3475 acceptance: the blank-line collapse must NOT touch code block
    // contents, so relayed code/tool output keeps its intentional spacing.
    let input = "before\n```text\nline1\n\n\n\nline2\n```\nafter";
    let output = format_for_discord(input);
    assert_eq!(output, input);
    // Prose around the fence still collapses normally.
    let mixed = "p1\n\n\n\n```\ncode\n\n\n\nmore\n```\n\n\n\np2";
    let mixed_out = format_for_discord(mixed);
    assert_eq!(mixed_out, "p1\n\n```\ncode\n\n\n\nmore\n```\n\np2");
}

#[test]
fn format_for_discord_removes_trailing_streaming_status_footer() {
    let input = "Final answer\n\n⠋ Processing...";
    let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
    assert_eq!(output, "Final answer");
}

#[test]
fn format_for_discord_removes_trailing_korean_processing_footer() {
    let input = "Final answer\n\n⠋ 계속 처리 중";
    let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
    assert_eq!(output, "Final answer");
}

#[test]
fn finalize_stale_streaming_footer_strips_completed_body() {
    // #3104: a turn that streamed then returned idle leaves the last edit
    // text ending in `⠏ 계속 처리 중`; finalize must strip it.
    let last_edit = "E2E answer\n- did the work\n\n⠏ 계속 처리 중";
    let finalized = super::finalize_stale_streaming_footer(last_edit, &ProviderKind::Claude);
    assert_eq!(finalized.as_deref(), Some("E2E answer\n- did the work"));
}

#[test]
fn finalize_stale_streaming_footer_leaves_streaming_body_untouched() {
    // A genuinely-still-streaming body (no trailing footer) is left as-is so
    // the reconciliation pass never clears a live footer prematurely.
    let still_streaming = "Partial answer so far";
    assert_eq!(
        super::finalize_stale_streaming_footer(still_streaming, &ProviderKind::Claude),
        None
    );
}

#[test]
fn finalize_stale_streaming_footer_skips_footer_only_body() {
    // Footer-only placeholder (no real content) must NOT be edited to blank;
    // the caller's delete/replace path owns that case.
    let footer_only = "⠏ 계속 처리 중";
    assert_eq!(
        super::finalize_stale_streaming_footer(footer_only, &ProviderKind::Claude),
        None
    );
}

#[test]
fn text_ends_with_streaming_footer_detects_korean_footer() {
    assert!(super::text_ends_with_streaming_footer(
        "Answer\n\n⠏ 계속 처리 중"
    ));
    assert!(!super::text_ends_with_streaming_footer(
        "Answer\n\nmore text"
    ));
}

#[test]
fn format_for_discord_removes_leading_tui_no_response_chrome() {
    let input = "No response requested.\n\nFinal answer";
    let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
    assert_eq!(output, "Final answer");
}

#[test]
fn format_for_discord_preserves_legitimate_no_response_sentence() {
    let input = "No response requested. But here is the explanation.";
    let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
    assert_eq!(output, input);
}

#[test]
fn format_for_discord_keeps_non_trailing_spinner_text() {
    let input = "⠋ Processing...\nFinal answer";
    let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
    assert_eq!(output, input);
}

#[test]
fn format_for_discord_removes_stacked_streaming_status_footers() {
    let input = "Final answer\n\n⠋ Processing...\n⠙ Working...";
    let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
    assert_eq!(output, "Final answer");
}

#[test]
fn format_for_discord_removes_placeholder_waiting_before_streaming_footer() {
    let input = "Final answer\n⏳ 대기 중...\n\n⠋ Processing...";
    let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
    assert_eq!(output, "Final answer");
}

#[test]
fn format_for_discord_keeps_trailing_spinner_without_known_status_shape() {
    let input = "Final answer\n\n⠋ note";
    let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
    assert_eq!(output, input);
}

#[test]
fn flag_off_formatter_preserves_trailing_merged_footer_shaped_user_text() {
    let input = "Final answer\n\n⠋ 진행 중 — user-authored line";
    let output = format_for_discord_with_provider(input, &ProviderKind::Claude);

    assert_eq!(output, input);
    assert_eq!(
        super::finalize_stale_streaming_footer(input, &ProviderKind::Claude),
        None
    );
}

#[test]
fn format_for_discord_removes_ascii_spinner_status_footer() {
    let input = "Final answer\n\n| Processing...";
    let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
    assert_eq!(output, "Final answer");
}

#[test]
fn format_for_discord_keeps_trailing_ascii_bullet_status_text() {
    let input = "Final answer\n- Working on the backend now";
    let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
    assert_eq!(output, input);
}

#[test]
fn format_for_discord_keeps_trailing_ascii_table_row() {
    let input = "Final answer\n| Processing fee | 3% |";
    let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
    assert_eq!(output, input);
}

#[test]
fn format_for_discord_preserves_trailing_blank_without_footer() {
    let input = "Final answer\n\n";
    let output = format_for_discord_with_provider(input, &ProviderKind::Claude);
    assert_eq!(output, "Final answer");
}

// #3089 A0 — characterization of the chunker + streaming-rollover splitter
// (design §5 A0 item 1: split_message chunk boundaries; item 4: streaming
// rollover split algorithm). Value-exact pins so any change to chunk
// boundaries/ordering, the Discord limit cliff, or the rollover
// split point fails BEFORE the #3089 controller cutover. Nested inside this
// `#[cfg(test)] mod` block => ZERO production LoC under the ratchet
// (formatting.rs baseline 2802 stays unchanged).
mod a0_characterization_tests {
    use super::super::super::semantic_boundaries::{
        message_split_boundary, semantic_sentence_split_boundary,
    };
    use super::super::{
        DISCORD_MSG_LIMIT, char_count, long_message_reply_builders, plan_streaming_rollover,
        split_message, streaming_split_boundary,
    };

    // -------------------------------------------------------------------
    // split_message — the single chunker (design §5 A0 item 1)
    // -------------------------------------------------------------------

    #[test]
    fn a0_split_message_keeps_short_body_as_a_single_verbatim_chunk() {
        let body = "hello world\nsecond line";
        let chunks = split_message(body);
        assert_eq!(chunks.len(), 1, "short body must stay one chunk");
        assert_eq!(chunks[0], body, "the single chunk must be byte-identical");
    }

    #[test]
    fn a0_split_message_effective_limit_is_msg_limit_minus_ten_outside_code_block() {
        let effective_limit = DISCORD_MSG_LIMIT - 10; // 1990
        assert_eq!(effective_limit, 1990, "pins the 2000-10 effective limit");

        let exactly_at_limit = "a".repeat(effective_limit);
        let chunks = split_message(&exactly_at_limit);
        assert_eq!(
            chunks.len(),
            1,
            "a body of exactly effective_limit chars stays a single chunk"
        );

        let one_over = "a".repeat(effective_limit + 1);
        let chunks = split_message(&one_over);
        assert_eq!(
            chunks.len(),
            2,
            "one char over the effective limit splits into two chunks"
        );
    }

    #[test]
    fn a0_split_message_keeps_700_korean_chars_as_one_chunk_issue_4214() {
        let body = "가".repeat(700);
        assert!(
            body.len() > DISCORD_MSG_LIMIT,
            "UTF-8 byte length reproduces the old premature split condition"
        );
        assert_eq!(char_count(&body), 700);

        let chunks = split_message(&body);
        assert_eq!(chunks.len(), 1, "700 Korean chars fit one Discord message");
        assert_eq!(chunks[0], body);

        let replies = long_message_reply_builders(&body);
        assert_eq!(
            replies.len(),
            1,
            "reply builders must not enter the delayed multi-chunk path"
        );
        assert_eq!(
            replies[0].content.as_deref(),
            Some(body.as_str()),
            "single reply preserves the original Korean body"
        );
    }

    #[test]
    fn a0_split_message_bounds_long_korean_chunks_by_character_count_issue_4214() {
        let body = "한".repeat(DISCORD_MSG_LIMIT + 25);
        let chunks = split_message(&body);

        assert!(chunks.len() >= 2, "over-limit Korean body splits");
        assert!(
            chunks.iter().all(|chunk| !chunk.is_empty()),
            "no empty chunk may be emitted for Korean splits"
        );
        assert!(
            chunks
                .iter()
                .all(|chunk| char_count(chunk) <= DISCORD_MSG_LIMIT),
            "every emitted chunk must fit Discord's character limit"
        );
        assert_eq!(
            chunks.concat(),
            body,
            "Korean chunks reassemble without losing or corrupting code points"
        );
    }

    #[test]
    fn a0_split_message_hard_splits_newline_free_body_at_the_effective_limit() {
        let body = "a".repeat(2500);
        let chunks = split_message(&body);
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            char_count(&chunks[0]),
            1990,
            "hard split at effective_limit"
        );
        assert_eq!(char_count(&chunks[1]), 2500 - 1990, "remainder length");
        // Order + completeness: concatenation reproduces the input.
        assert_eq!(format!("{}{}", chunks[0], chunks[1]), body);
    }

    #[test]
    fn a0_split_message_prefers_last_newline_and_strips_the_boundary_newline() {
        let head = "a".repeat(1000);
        let tail = "b".repeat(1500);
        let body = format!("{head}\n{tail}"); // newline at byte 1000, within 1990
        let chunks = split_message(&body);
        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks[0], head,
            "first chunk ends at the last newline (excl.)"
        );
        assert_eq!(
            chunks[1], tail,
            "the boundary newline is stripped from the next chunk head"
        );
    }

    #[test]
    fn a0_split_message_uses_semantic_sentence_boundary_when_no_newline_exists() {
        let head = format!("{}확인합니다.", "a".repeat(1480));
        let tail = format!("`NullRHI`{}", "b".repeat(1000));
        let body = format!("{head}{tail}");
        let chunks = split_message(&body);

        assert_eq!(chunks.len(), 2);
        assert_eq!(
            chunks[0], head,
            "newline-free prose should split at a sentence boundary before hard-splitting"
        );
        assert_eq!(chunks[1], tail);
        assert_eq!(format!("{}{}", chunks[0], chunks[1]), body);
    }

    #[test]
    fn a0_split_message_leading_newline_does_not_emit_an_empty_chunk() {
        // Issue #1043 guard.
        let body = format!("\n{}", "a".repeat(2200));
        let chunks = split_message(&body);
        assert!(
            chunks.iter().all(|c| !c.is_empty()),
            "no empty chunk may be emitted (#1043)"
        );
        assert!(chunks.len() >= 2, "the long body still splits");
    }

    #[test]
    fn a0_split_message_reopens_code_fence_across_a_chunk_boundary() {
        let mut body = String::from("```rust\n");
        body.push_str(&"x".repeat(2100)); // forces a split while the fence is open
        let chunks = split_message(&body);
        assert!(chunks.len() >= 2, "long fenced body splits");
        assert!(
            chunks[0].ends_with("\n```"),
            "first chunk closes the open fence: {:?}",
            &chunks[0][chunks[0].len().saturating_sub(8)..]
        );
        assert!(
            chunks[1].starts_with("```rust\n"),
            "next chunk re-opens the fence with the same language tag"
        );
    }

    #[test]
    fn a0_long_message_reply_builders_split_without_continuation_markers() {
        let body = "a".repeat(2500);
        let replies = long_message_reply_builders(&body);
        assert_eq!(replies.len(), 2);
        let first = replies[0].content.as_ref().expect("first content");
        let second = replies[1].content.as_ref().expect("second content");

        // Continuation markers ([n/m]) were removed per operator request:
        // the relay must not prepend chunk-index prefixes.
        assert!(!first.starts_with('['));
        assert!(!second.starts_with('['));
        assert!(char_count(first) <= DISCORD_MSG_LIMIT);
        assert!(char_count(second) <= DISCORD_MSG_LIMIT);
    }

    // -------------------------------------------------------------------
    // streaming_split_boundary — rollover boundary primitive (§5 A0 item 4)
    // -------------------------------------------------------------------

    #[test]
    fn a0_streaming_split_boundary_is_none_when_text_fits() {
        assert_eq!(streaming_split_boundary("short", 100), None);
        assert_eq!(streaming_split_boundary("anything", 0), None);
    }

    #[test]
    fn a0_streaming_split_boundary_prefers_paragraph_then_newline_then_whitespace() {
        // Preference: "\n\n" (+2) > "\n" (+1) > whitespace > hard safe_end.
        // safe_end = 50; each break is past safe_end/2 (=25) so the
        // "early break => hard split" guard does not fire and the preferred
        // boundary is used (the index INCLUDES the delimiter).

        // Paragraph break at byte 30 ("\n\n") => split at 30 + 2 = 32.
        let para = format!("{}\n\n{}", "x".repeat(30), "c".repeat(100));
        assert_eq!(
            streaming_split_boundary(&para, 50),
            Some(32),
            "splits just after the paragraph break"
        );

        // Single newline at byte 30 => split at 30 + 1 = 31.
        let nl = format!("{}\n{}", "x".repeat(30), "e".repeat(100));
        assert_eq!(
            streaming_split_boundary(&nl, 50),
            Some(31),
            "splits just after the single newline"
        );

        // Whitespace (space) at byte 30, no newline => split at 30 + 1 = 31.
        let ws = format!("{} {}", "x".repeat(30), "f".repeat(100));
        assert_eq!(
            streaming_split_boundary(&ws, 50),
            Some(31),
            "splits just after the last whitespace"
        );
    }

    #[test]
    fn a0_streaming_split_boundary_paragraph_beats_a_later_single_newline() {
        // MIXED delimiters that DISPROVE priority (codex Medium 4): a
        // paragraph break at byte 26 ("\n\n" => 26 + 2 = 28) precedes a LATER
        // single newline at byte 40 (=> 41), both inside safe_end = 50 and
        // both past safe_end/2 = 25. Production prefers paragraph
        // (`paragraph_split.or(newline_split)`), so the split is 28. If that
        // `.or` were reordered to newline-first, the later newline would win
        // and the split would be 41 — a DIFFERENT value, so this pins the
        // paragraph > single-newline priority, not just the position.
        let body = format!(
            "{}\n\n{}\n{}",
            "x".repeat(26),
            "y".repeat(12),
            "z".repeat(100)
        );
        assert_eq!(
            streaming_split_boundary(&body, 50),
            Some(28),
            "paragraph break wins over a later single newline"
        );
    }

    #[test]
    fn a0_streaming_split_boundary_single_newline_beats_a_later_space() {
        // MIXED delimiters: a single newline at byte 30 (=> 31) precedes a
        // LATER space at byte 42 (=> 43), no paragraph break, both past
        // safe_end/2. Production prefers newline over whitespace
        // (`newline_split.or(whitespace_split)`), so the split is 31. If the
        // chain were reordered to whitespace-first, the later space would win
        // (43) — a DIFFERENT value, pinning the single-newline > whitespace
        // priority.
        let body = format!("{}\n{} {}", "x".repeat(30), "y".repeat(11), "w".repeat(100));
        assert_eq!(
            streaming_split_boundary(&body, 50),
            Some(31),
            "single newline wins over a later space"
        );
    }

    #[test]
    fn a0_streaming_split_boundary_sentence_beats_a_later_space() {
        let head = format!("{}확인합니다.", "x".repeat(20));
        let body = format!("{}{} {}", head, "y".repeat(5), "z".repeat(100));
        assert_eq!(
            streaming_split_boundary(&body, 50),
            Some(head.len()),
            "sentence boundary wins over a later whitespace split"
        );
    }

    #[test]
    fn a0_semantic_sentence_split_boundary_skips_markdown_continuations_and_code_fences() {
        assert_eq!(
            semantic_sentence_split_boundary("확인합니다.`NullRHI`"),
            Some("확인합니다.".len()),
            "inline-code follow-up after Korean sentence is a readable split point"
        );
        assert_eq!(
            semantic_sentence_split_boundary("Use `foo.bar` in config"),
            None,
            "inline code punctuation is not a sentence split point"
        );
        let code_window = "println!(\"done.\"); keep streaming inside fence";
        assert_eq!(
            message_split_boundary(code_window, code_window.len(), true),
            (code_window.len(), "hard"),
            "already-open code fences must not use semantic sentence splits"
        );
        assert_eq!(
            semantic_sentence_split_boundary("- item. more text"),
            None,
            "list items keep their existing markdown continuation behavior"
        );
        assert_eq!(
            semantic_sentence_split_boundary("| Col | value."),
            None,
            "table-like lines keep their existing markdown continuation behavior"
        );
        assert_eq!(
            semantic_sentence_split_boundary("version 1.2"),
            None,
            "decimal points are not sentence boundaries"
        );
        assert_eq!(
            semantic_sentence_split_boundary("config.yaml"),
            None,
            "single-token file extensions are not sentence boundaries"
        );
        assert_eq!(
            semantic_sentence_split_boundary("```text\nDone.\n```"),
            None,
            "code-fence content must not be sentence-split"
        );
    }

    #[test]
    fn a0_streaming_split_boundary_hard_splits_when_break_is_in_first_half() {
        // "preferred < safe_end / 2 => use safe_end": an early break is
        // rejected in favor of a hard split.
        let body = format!("ab\n{}", "g".repeat(100));
        assert_eq!(
            streaming_split_boundary(&body, 50),
            Some(50),
            "an early break is rejected; hard-split at safe_end"
        );
    }

    // -------------------------------------------------------------------
    // plan_streaming_rollover — the rollover plan (§5 A0 item 4)
    //
    // Both turn_bridge and tmux_watcher call THIS single function, so
    // "same input => same output" is the duplication-free behavior to lock.
    // -------------------------------------------------------------------

    #[test]
    fn a0_plan_streaming_rollover_reserves_footer_and_margin_before_the_2000_cliff() {
        // body_budget = 2000 - ((2 + char_count(status)) + 10). For "STATUS":
        // footer "\n\nSTATUS" = 8; body_budget = 2000 - 18 = 1982.
        let status = "STATUS";
        let body = "Z".repeat(2500);
        let plan = plan_streaming_rollover(&body, status).expect("a long body must roll over");
        assert_eq!(
            plan.split_at, 1982,
            "rollover split point pins the footer+margin reservation"
        );
        assert_eq!(
            plan.frozen_chunk,
            "Z".repeat(1982),
            "frozen chunk is body[..split_at]"
        );
        assert_eq!(char_count(&plan.frozen_chunk), plan.split_at);
    }

    #[test]
    fn a0_plan_streaming_rollover_is_none_for_empty_or_short_body() {
        assert_eq!(plan_streaming_rollover("", "STATUS"), None);
        assert_eq!(plan_streaming_rollover("short body", "STATUS"), None);
    }

    #[test]
    fn a0_plan_streaming_rollover_is_deterministic_for_both_caller_surfaces() {
        // Identical (body, status) must yield byte-identical plans on every
        // call, so a future per-surface re-derivation is caught.
        let body = "line one\nline two\n".repeat(200); // > body_budget, newlines
        let a = plan_streaming_rollover(&body, "STATUS").expect("rolls over");
        let b = plan_streaming_rollover(&body, "STATUS").expect("rolls over");
        assert_eq!(a, b, "same input must produce an identical rollover plan");
        assert_eq!(a.frozen_chunk, body[..a.split_at]);
    }
}
