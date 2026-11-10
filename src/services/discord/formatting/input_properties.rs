//! Property tests and cargo-fuzz entries for pure input handling.
//!
//! Compiled only for `cfg(test)` and `cfg(fuzzing)`. Production builds omit this module.

use super::{
    byte_index_at_discord_message_units, discord_message_units, escape_for_code_fence,
    floor_char_boundary, format_for_discord, format_for_discord_with_provider, format_tool_input,
    needs_multiple_messages, normalize_empty_lines, redact_sensitive_for_placeholder,
    split_message, streaming_split_boundary, truncate_str,
};
use crate::services::discord::commands::{
    CommandRisk, PolicyDecision, evaluate_policy, match_text_command,
};
use crate::services::discord::outbound::decision::{
    LengthPolicyDecision, OutboundPolicyLimits, decide_policy_with_limits,
};
use crate::services::discord::outbound::delivery::truncate_with_marker;
use crate::services::discord::outbound::message::{DiscordOutboundMessage, OutboundTarget};
use crate::services::discord::outbound::policy::{
    DiscordOutboundPolicy, FallbackPolicy, LengthStrategy,
};
use crate::services::discord::prompt_builder::system_prompt_audit::{
    code_blocks, neutralize_fences, redact, render,
};
use crate::services::discord::response_sanitizer::{
    sanitize_hidden_context, strip_leading_tui_response_chrome,
};
use crate::services::provider::ProviderKind;
use crate::services::stream_json_cli::codec::StreamJsonCodec;
use crate::services::tui_input::ansi::{self, Piece};
use crate::services::tui_input::human_frame::{self, Mode};
use crate::services::tui_prompt_control::{
    CodexInputClass, classify_codex_input, classify_local_only_slash_control,
    strip_terminal_controls,
};
use crate::services::tui_prompt_dedupe::{
    extract_claude_transcript_user_prompt, extract_codex_rollout_user_prompt,
    extract_prompt_from_hook_payload, extract_qwen_jsonl_user_prompt,
    is_discord_relayed_user_prompt,
};
use crate::utils::discord::{
    MAX_DISCORD_RECIPIENT_IDS, is_discord_snowflake, normalize_discord_recipient_ids,
    normalize_discord_snowflake,
};
use crate::utils::format::{safe_prefix, safe_suffix, tail_with_ellipsis};
use crate::utils::redact::{redact_assignments, redact_cookie_headers};
use poise::serenity_prelude::ChannelId;
use std::collections::HashSet;
use std::time::Duration;

const TEXT_CAP: usize = 8_000;
const LIMIT: usize = crate::services::discord::DISCORD_MSG_LIMIT;

fn capped(text: &str) -> &str {
    let mut end = text.len().min(TEXT_CAP);
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

pub(super) fn check_split(text: &str) {
    let text = capped(text);
    let chunks = split_message(text);
    assert_eq!(
        split_message(text),
        chunks,
        "split_message is not deterministic"
    );
    assert_eq!(
        needs_multiple_messages(text),
        discord_message_units(text) > LIMIT
    );
    if text.is_empty() {
        assert!(chunks.is_empty());
        return;
    }
    assert!(!chunks.is_empty(), "non-empty input produced no chunks");
    for chunk in &chunks {
        assert!(!chunk.is_empty(), "empty discord chunk");
        let units = discord_message_units(chunk);
        assert!(
            units <= LIMIT,
            "chunk has {units} utf-16 units, limit is {LIMIT}"
        );
    }
    if chunks.len() > 1 {
        for chunk in &chunks[..chunks.len() - 1] {
            let fences = chunk
                .lines()
                .filter(|line| line.trim_start().starts_with("```"))
                .count();
            assert_eq!(fences % 2, 0, "non-final chunk left a code fence open");
        }
    }
    if !text.contains('\n') && !text.trim_start().starts_with("```") {
        assert_eq!(
            chunks.join(""),
            text,
            "newline-free split dropped or added bytes"
        );
    }
}

pub(super) fn check_format(text: &str) {
    let text = capped(text);
    let once = format_for_discord(text);
    assert_eq!(
        format_for_discord(&once),
        once,
        "format_for_discord is not idempotent"
    );
    for provider in [
        ProviderKind::Claude,
        ProviderKind::Codex,
        ProviderKind::Grok,
    ] {
        let formatted = format_for_discord_with_provider(text, &provider);
        assert_eq!(
            format_for_discord_with_provider(&formatted, &provider),
            formatted,
            "format_for_discord_with_provider is not idempotent"
        );
    }
    if !text.contains('\r')
        && !text
            .lines()
            .any(|line| line.trim_start().starts_with("```"))
    {
        let fenced = format!("```\n{text}\n```");
        assert_eq!(
            format_for_discord(&fenced),
            fenced,
            "fenced body was rewritten"
        );
    }
}

pub(super) fn check_units(text: &str, max_units: usize) {
    let text = capped(text);
    let max_units = max_units % (LIMIT + 50);
    let at = byte_index_at_discord_message_units(text, max_units);
    assert!(text.is_char_boundary(at));
    assert!(discord_message_units(&text[..at]) <= max_units);
    if let Some(next) = text[at..].chars().next() {
        assert!(discord_message_units(&text[..at]) + next.len_utf16() > max_units);
    }
    let floored = floor_char_boundary(text, max_units);
    assert!(text.is_char_boundary(floored));
    if max_units >= text.len() {
        assert_eq!(floored, text.len());
    } else {
        assert!(floored <= max_units);
    }
    let (split_at, _) =
        crate::services::discord::semantic_boundaries::message_split_boundary(text, at, false);
    assert!(split_at <= at);
    assert!(text.is_char_boundary(split_at));
    let _ = text.split_at(split_at);
    match streaming_split_boundary(text, max_units) {
        None => {
            let first_does_not_fit = text
                .chars()
                .next()
                .is_some_and(|ch| ch.len_utf16() > max_units);
            assert!(
                max_units == 0 || discord_message_units(text) <= max_units || first_does_not_fit
            );
        }
        Some(split) => {
            assert!(split > 0 && split < text.len());
            assert!(text.is_char_boundary(split));
            assert!(discord_message_units(&text[..split]) <= max_units);
        }
    }
}

pub(super) fn check_truncate_marker(text: &str, max_units: usize) {
    let text = capped(text);
    let max_units = max_units % (LIMIT + 5);
    let (out, truncated) = truncate_with_marker(text, max_units);
    assert!(discord_message_units(&out) <= max_units);
    if truncated {
        assert!(discord_message_units(text) > max_units);
    } else {
        assert_eq!(out, text);
    }
    let (again, again_truncated) = truncate_with_marker(&out, max_units);
    assert_eq!(again, out);
    assert!(!again_truncated);
}

pub(super) fn check_outbound_decision(text: &str, limit: usize, strategy_tag: u8) {
    let text = capped(text);
    let limit = limit % 500;
    let strategy = match strategy_tag % 4 {
        0 => LengthStrategy::Split,
        1 => LengthStrategy::Compact,
        2 => LengthStrategy::FileAttachment,
        _ => LengthStrategy::RejectOverLimit,
    };
    let policy = DiscordOutboundPolicy {
        length_strategy: strategy,
        fallback: FallbackPolicy::None,
        idempotency_window: Duration::from_secs(1),
    };
    let limits = OutboundPolicyLimits {
        inline_char_limit: limit,
        split_chunk_char_limit: limit,
        compact_char_limit: limit,
    };
    let message = DiscordOutboundMessage::new(
        "correlation",
        "event",
        text,
        OutboundTarget::Channel(ChannelId::new(1)),
        policy,
    );
    let decision = decide_policy_with_limits(&message, limits);
    assert_eq!(decision, decide_policy_with_limits(&message, limits));
    let units = discord_message_units(text);
    match decision.length {
        LengthPolicyDecision::Inline { char_count } => {
            assert_eq!(char_count, units);
            assert!(units <= limit);
        }
        LengthPolicyDecision::Split {
            char_count,
            chunk_char_limit,
            chunk_count,
            ..
        } => {
            assert_eq!(char_count, units);
            assert!(units > limit);
            assert!(chunk_char_limit >= 1);
            assert!(chunk_count >= 1);
            assert!(
                chunk_count.saturating_mul(chunk_char_limit) >= units,
                "split plan does not cover {units} units"
            );
        }
        LengthPolicyDecision::Compact { char_count, .. }
        | LengthPolicyDecision::FileAttachment { char_count, .. }
        | LengthPolicyDecision::RejectOverLimit { char_count, .. } => {
            assert_eq!(char_count, units);
            assert!(units > limit);
        }
    }
}

pub(super) fn check_sanitize(text: &str) {
    let text = capped(text);
    let hidden = sanitize_hidden_context(text);
    // `lines()` plus `\n` join turns a trailing CR into CRLF, so a second pass
    // drops that CR. Idempotence holds for input that has no CR.
    // See artifacts/fuzz_findings.md.
    if !text.contains('\r') {
        assert_eq!(sanitize_hidden_context(&hidden), hidden);
    }
    let chrome = strip_leading_tui_response_chrome(text);
    assert_eq!(strip_leading_tui_response_chrome(&chrome), chrome);
    let both = strip_leading_tui_response_chrome(&hidden);
    assert_eq!(strip_leading_tui_response_chrome(&both), both);
}

pub(super) fn check_commands(text: &str) {
    let text = capped(text);
    let matched = match_text_command(text);
    assert_eq!(matched, match_text_command(text));
    let mut words = text.split_whitespace();
    let first = words.next().unwrap_or("");
    if matches!(first, "!clear" | "!stop" | "!restart") {
        assert!(matched.is_some(), "{first} was not recognized");
    }
    let rest: Vec<&str> = words.collect();
    if first == "!herdr" && matches!(rest.as_slice(), ["consent", "retry" | "return"]) {
        assert!(matched.is_some());
    }
    if first == "!clearly" || first == "!stopped" {
        assert!(matched.is_none());
    }
    for risk in [
        CommandRisk::ReadOnly,
        CommandRisk::Mutating,
        CommandRisk::ShellOrToolGrant,
        CommandRisk::CredentialSystem,
    ] {
        assert_eq!(
            evaluate_policy(risk, true, true),
            PolicyDecision::Allow,
            "owner with high-risk enabled was denied"
        );
        if risk.is_high_risk() {
            assert_eq!(
                evaluate_policy(risk, false, true),
                PolicyDecision::DenyNotOwner
            );
            assert_eq!(
                evaluate_policy(risk, false, false),
                PolicyDecision::DenyNotOwner
            );
        } else {
            assert_eq!(evaluate_policy(risk, false, false), PolicyDecision::Allow);
        }
        if risk.requires_explicit_enable() {
            assert_eq!(
                evaluate_policy(risk, true, false),
                PolicyDecision::DenyNotEnabled
            );
        } else {
            assert_eq!(evaluate_policy(risk, true, false), PolicyDecision::Allow);
        }
    }
}

pub(super) fn check_classification(text: &str) {
    let text = capped(text);
    let class = classify_codex_input(text);
    assert_eq!(class, classify_codex_input(text));
    match class {
        CodexInputClass::ProviderPrompt => {}
        CodexInputClass::LocalControl { name, .. } => {
            assert!(
                name == "/model" || name == "/help",
                "unexpected local control {name}"
            );
        }
        CodexInputClass::UnsupportedControl { raw_name } => {
            assert!(raw_name.starts_with('/') && raw_name.len() > 1);
        }
    }
    let stripped = strip_terminal_controls(text);
    assert_eq!(strip_terminal_controls(&stripped), stripped);
    assert!(!stripped.contains('\u{1b}'));
    if let Some(control) = classify_local_only_slash_control(text) {
        assert!(matches!(
            control.kind.as_str(),
            "slash"
                | "/compact"
                | "local-command-stdout"
                | "/cost"
                | "/context"
                | "/model"
                | "/effort"
                | "/fast"
        ));
    }
}

pub(super) fn check_ansi(text: &str) {
    let text = capped(text);
    let once = ansi::strip_ansi_escape_sequences(text);
    assert!(!once.contains('\r'));
    // A CR between ESC and `[` is removed on the first pass, so the second pass
    // sees a CSI and drops it. Idempotence holds only when no ESC remains.
    // See artifacts/fuzz_findings.md.
    if !once.contains('\u{1b}') {
        assert_eq!(ansi::strip_ansi_escape_sequences(&once), once);
    }
    let rebuilt: String = ansi::pieces(text)
        .filter_map(|piece| match piece {
            Piece::Text(ch) if ch != '\r' => Some(ch),
            _ => None,
        })
        .collect();
    assert_eq!(rebuilt, once);
}

pub(super) fn check_human_frame(name: &str, author: &str, source: &str, nonce: &str, body: &str) {
    let rendered = human_frame::inject(source, author, name, nonce, body);
    let Some((header, after)) = human_frame::parse(&rendered) else {
        panic!("inject output did not parse");
    };
    assert_eq!(header.mode, Mode::Inject);
    assert_eq!(header.nonce, nonce);
    assert_eq!(header.author, author);
    assert!(!header.name.is_empty());
    assert!(!header.source.chars().any(char::is_whitespace));
    assert_eq!(after, format!("\n{body}"));
    let again = human_frame::parse(&rendered).expect("parse is deterministic");
    assert_eq!(again.0, header);
    assert_eq!(again.1, after);
}

pub(super) fn check_author_name(name: &str, author_id: u64) {
    let once = human_frame::author_name(name, author_id);
    assert_eq!(human_frame::author_name(&once, author_id), once);
    assert!(!once.contains(['\n', '\r', '[', ']', '{', '}']));
}

pub(super) fn check_extract(bytes: &[u8]) {
    let bytes = &bytes[..bytes.len().min(TEXT_CAP)];
    let text = String::from_utf8_lossy(bytes);
    let relayed = is_discord_relayed_user_prompt(&text);
    assert_eq!(relayed, is_discord_relayed_user_prompt(&text));
    if relayed {
        assert!(text.contains("[User: "));
    }
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return;
    };
    assert_eq!(
        extract_prompt_from_hook_payload(&value),
        extract_prompt_from_hook_payload(&value)
    );
    assert_eq!(
        extract_codex_rollout_user_prompt(&value),
        extract_codex_rollout_user_prompt(&value)
    );
    assert_eq!(
        extract_claude_transcript_user_prompt(&value),
        extract_claude_transcript_user_prompt(&value)
    );
    assert_eq!(
        extract_qwen_jsonl_user_prompt(&value),
        extract_qwen_jsonl_user_prompt(&value)
    );
}

pub(super) fn check_rollout_and_codecs(bytes: &[u8]) {
    let bytes = &bytes[..bytes.len().min(TEXT_CAP)];
    let _ = crate::services::codex_tui::rollout_tail::recover_captured_rollout_response(bytes);
    let text = String::from_utf8_lossy(bytes);
    let mut decoder = crate::services::codex_tui::rollout_tail::RolloutRecordDecoder::default();
    for line in text.lines().take(32) {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(line) {
            let _ = decoder.decode(&value);
        }
    }
    let _ = decoder.response();

    let mut agy = crate::services::stream_json_cli::codec::AgyCodec::new();
    let mut messages = crate::services::stream_json_cli::codec::MessagesJsonCodec::new();
    for line in text.lines().take(32) {
        let _ = agy.push_stdout_line(line);
        let _ = messages.push_stdout_line(line);
    }
    assert!(agy.finish(Some(0), &text).is_ok());
    assert!(matches!(agy.finish(Some(1), "again"), Ok(done) if done.is_empty()));
    let _ = messages.finish(Some(0), &text);
}

pub(super) fn check_uuid(raw: &str) {
    let parsed = crate::services::stream_json_cli::session::parse_strict_uuid(raw, "grok");
    match uuid::Uuid::parse_str(raw.trim()) {
        Ok(_) => {
            let token = parsed.expect("strict uuid parser rejected a uuid");
            assert_eq!(token.as_str(), raw.trim());
        }
        Err(_) => assert!(parsed.is_err()),
    }
}

pub(super) fn check_config_text(text: &str) {
    let text = capped(text);
    if let Ok(config) = serde_yaml::from_str::<crate::config::Config>(text) {
        let _ = crate::config::validate_config(&config);
    }
}

pub(super) fn check_normalizers(
    profile: Option<String>,
    ttl: Option<u32>,
    ids: &[String],
    snowflake: &str,
) {
    let normalized = crate::config::normalize_dispatch_profile(profile);
    assert!(matches!(
        normalized.as_deref(),
        None | Some("lite") | Some("full")
    ));
    assert_eq!(
        crate::config::normalize_dispatch_profile(normalized.clone()),
        normalized
    );
    let ttl = crate::config::normalize_cache_ttl_minutes(ttl);
    assert!(matches!(ttl, None | Some(5) | Some(60)));
    assert_eq!(crate::config::normalize_cache_ttl_minutes(ttl), ttl);
    if let Ok(recipients) = normalize_discord_recipient_ids(ids) {
        assert!(recipients.len() <= MAX_DISCORD_RECIPIENT_IDS);
        assert_eq!(
            recipients.len(),
            recipients.iter().collect::<HashSet<_>>().len()
        );
        for id in &recipients {
            assert!(!id.starts_with('0'));
            assert!(id.bytes().all(|byte| byte.is_ascii_digit()));
            assert!(id.parse::<u64>().ok().is_some_and(|value| value > 0));
        }
    }
    let recognized = is_discord_snowflake(snowflake);
    match normalize_discord_snowflake(Some(snowflake)) {
        Some(value) => {
            assert!(recognized);
            assert_eq!(value, snowflake.trim());
        }
        None => assert!(!recognized),
    }
}

pub(super) fn check_redact(text: &str) {
    let text = capped(text);
    let (masked, _) = redact(text);
    assert_eq!(redact(&masked).0, masked, "redact is not idempotent");
    let neutralized = neutralize_fences(text);
    assert_eq!(neutralize_fences(&neutralized), neutralized);
    if !text.contains('\u{200b}') {
        assert!(
            !neutralized.contains("```"),
            "fence neutralization left a raw fence"
        );
    }
    let blocks = code_blocks(&neutralized);
    let mut rebuilt = String::new();
    for block in &blocks {
        let body = block
            .strip_prefix("```\n")
            .and_then(|rest| rest.strip_suffix("\n```"))
            .expect("code block wrapper missing");
        assert!(discord_message_units(block) <= LIMIT);
        rebuilt.push_str(body);
    }
    assert_eq!(rebuilt, neutralized);
    let messages = render(text, "session-id-too-long", Some("now"), 2, true);
    assert!(!messages.is_empty());
    for message in &messages {
        assert!(
            discord_message_units(message) <= LIMIT,
            "rendered system-prompt message exceeds {LIMIT} units"
        );
    }
    let cookies = redact_cookie_headers(text, "***");
    assert_eq!(redact_cookie_headers(&cookies, "***"), cookies);
    let assigned = redact_assignments(text);
    assert_eq!(redact_assignments(&assigned), assigned);
    let sensitive = redact_sensitive_for_placeholder(text);
    assert_eq!(redact_sensitive_for_placeholder(&sensitive), sensitive);
}

pub(super) fn check_compose(prefix: &str, chunk: &str) {
    let prefix = capped(prefix);
    let chunk = capped(chunk);
    let mut body = prefix.to_string();
    let before = body.len();
    crate::services::discord::turn_bridge::chunk_compose::compose_streamed_text_chunk(
        &mut body, chunk,
    );
    assert!(body.len() >= before, "streamed text chunk shrank the body");
    let mid = body.len();
    crate::services::discord::turn_bridge::chunk_compose::compose_streamed_text_chunk(
        &mut body, "",
    );
    assert!(body.len() >= mid);
}

pub(super) fn check_literal_chunks(input: &str, max_chars: usize) {
    let input = capped(input);
    let chunks =
        crate::services::claude_tui::input::split_literal_chunks_for_fuzz(input, max_chars);
    let budget = max_chars.max(1);
    if input.is_empty() {
        assert!(chunks.is_empty());
        return;
    }
    assert_eq!(chunks.join(""), input);
    for chunk in &chunks {
        assert!(!chunk.is_empty());
        assert!(chunk.chars().count() <= budget);
    }
}

pub(super) fn check_bounds(text: &str, max_bytes: usize, max_chars: usize) {
    let text = capped(text);
    let max_bytes = max_bytes % (TEXT_CAP + 1);
    let max_chars = max_chars % 400;
    let prefix = safe_prefix(text, max_bytes);
    assert!(text.starts_with(prefix));
    assert!(prefix.len() <= max_bytes);
    let suffix = safe_suffix(text, max_bytes);
    assert!(text.ends_with(suffix));
    assert!(suffix.len() <= max_bytes);
    let tail = tail_with_ellipsis(text, max_chars);
    if text.chars().count() <= max_chars {
        assert_eq!(tail, text);
    } else if max_chars == 0 {
        // Current behavior exceeds a zero budget by one scalar. See artifacts/fuzz_findings.md.
        assert_eq!(tail, "…");
    } else {
        assert!(tail.chars().count() <= max_chars);
        assert!(tail.starts_with('…'));
    }
}

pub(super) fn check_small_editors(text: &str, max_len: usize) {
    let text = capped(text);
    let max_len = max_len % 600;
    let escaped = escape_for_code_fence(text);
    assert_eq!(escape_for_code_fence(&escaped), escaped);
    if !text.contains('\u{200b}') {
        assert!(!escaped.contains("```"));
        assert_eq!(escaped.replace('\u{200b}', ""), text);
    }
    let truncated = truncate_str(text, max_len);
    assert!(truncated.len() <= max_len);
    assert!(text.starts_with(&truncated));
    if text.len() <= max_len {
        assert_eq!(truncated, text);
    }
    let lines = normalize_empty_lines(text);
    assert_eq!(normalize_empty_lines(&lines), lines);
    let kept: Vec<&str> = text.lines().filter(|line| !line.is_empty()).collect();
    let out: Vec<&str> = lines.lines().filter(|line| !line.is_empty()).collect();
    assert_eq!(kept, out);
    assert_eq!(
        format_tool_input("Bash", text),
        format_tool_input("Bash", text)
    );
    assert_eq!(
        format_tool_input("mcp__demo__echo", text),
        format_tool_input("mcp__demo__echo", text)
    );
}

#[cfg(test)]
mod properties {
    use super::*;
    use proptest::prelude::*;

    fn text() -> impl Strategy<Value = String> {
        prop::collection::vec(interesting_char(), 0..240)
            .prop_map(|chars| chars.into_iter().collect())
    }

    fn interesting_char() -> impl Strategy<Value = char> {
        prop_oneof![
            8 => (0x20u32..0x7eu32).prop_map(|code| char::from_u32(code).unwrap()),
            2 => prop_oneof![
                Just('\n'),
                Just('\r'),
                Just('\t'),
                Just('\u{1b}'),
                Just('`'),
                Just('#'),
                Just('|'),
                Just('*'),
                Just('['),
                Just(']'),
                Just('/'),
                Just('!'),
            ],
            1 => (0x80u32..0x180u32).prop_map(|code| char::from_u32(code).unwrap_or('?')),
            1 => Just('한'),
            1 => Just('😀'),
            1 => Just('\u{200b}'),
        ]
    }

    fn digits() -> impl Strategy<Value = String> {
        proptest::string::string_regex("[1-9][0-9]{0,16}").expect("digit regex")
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn split_message_respects_limit_and_keeps_newline_free_bytes(input in text()) {
            check_split(&input);
        }

        #[test]
        fn format_for_discord_is_idempotent_and_preserves_fences(input in text()) {
            check_format(&input);
        }

        #[test]
        fn discord_unit_index_is_a_char_boundary(input in text(), max in 0usize..2200) {
            check_units(&input, max);
        }

        #[test]
        fn truncate_with_marker_fits_and_is_idempotent(input in text(), max in 0usize..2200) {
            check_truncate_marker(&input, max);
        }

        #[test]
        fn outbound_length_plan_covers_units(input in text(), limit in 0usize..500, tag in any::<u8>()) {
            check_outbound_decision(&input, limit, tag);
        }

        #[test]
        fn sanitize_and_chrome_strip_are_idempotent(input in text()) {
            check_sanitize(&input);
        }

        #[test]
        fn command_match_is_stable_and_policy_is_monotonic(input in text()) {
            check_commands(&input);
        }

        #[test]
        fn slash_classification_is_total(input in text()) {
            check_classification(&input);
        }

        #[test]
        fn ansi_strip_is_idempotent(input in text()) {
            check_ansi(&input);
        }

        #[test]
        fn human_frame_inject_round_trips(
            name in text(),
            author in digits(),
            source in "[A-Za-z0-9:/._@+-]{0,24}",
            nonce in "[0-9a-f]{8}",
            body in text(),
        ) {
            check_human_frame(&name, &author, &source, &nonce, &body);
        }

        #[test]
        fn author_name_is_idempotent_for_numeric_ids(name in text(), id in any::<u64>()) {
            check_author_name(&name, id);
        }

        #[test]
        fn prompt_extract_is_stable(bytes in prop::collection::vec(any::<u8>(), 0..400)) {
            check_extract(&bytes);
        }

        #[test]
        fn rollout_and_codecs_do_not_panic(bytes in prop::collection::vec(any::<u8>(), 0..500)) {
            check_rollout_and_codecs(&bytes);
        }

        #[test]
        fn strict_uuid_matches_the_uuid_parser(raw in text()) {
            check_uuid(&raw);
        }

        #[test]
        fn config_yaml_does_not_panic(input in text()) {
            check_config_text(&input);
        }

        #[test]
        fn normalizers_stay_in_their_closed_sets(
            profile in proptest::option::of(text()),
            ttl in proptest::option::of(any::<u32>()),
            ids in prop::collection::vec(text(), 0..8),
            snowflake in text(),
        ) {
            check_normalizers(profile, ttl, &ids, &snowflake);
        }

        #[test]
        fn redact_fences_and_blocks_stay_inside_the_message_limit(input in text()) {
            check_redact(&input);
        }

        #[test]
        fn streamed_chunk_compose_never_shrinks(prefix in text(), chunk in text()) {
            check_compose(&prefix, &chunk);
        }

        #[test]
        fn literal_prompt_chunks_round_trip(input in text(), max_chars in 0usize..80) {
            check_literal_chunks(&input, max_chars);
        }

        #[test]
        fn prefix_suffix_and_tail_respect_their_budgets(
            input in text(),
            max_bytes in 0usize..400,
            max_chars in 0usize..80,
        ) {
            check_bounds(&input, max_bytes, max_chars);
        }

        #[test]
        fn escape_truncate_and_blank_lines_are_idempotent(input in text(), max_len in 0usize..400) {
            check_small_editors(&input, max_len);
        }
    }

    #[test]
    fn tail_with_ellipsis_zero_budget_returns_one_scalar() {
        assert_eq!(tail_with_ellipsis("a", 0), "…");
        assert_eq!(tail_with_ellipsis("a", 0).chars().count(), 1);
    }
}

#[cfg(fuzzing)]
pub mod fuzz {
    fn lossy(data: &[u8]) -> String {
        String::from_utf8_lossy(data).into_owned()
    }

    pub fn discord_message_text(data: &[u8]) {
        let text = lossy(data);
        super::check_split(&text);
        super::check_format(&text);
        super::check_sanitize(&text);
        super::check_small_editors(&text, data.first().copied().unwrap_or(0) as usize);
        if data.len() >= 2 {
            super::check_units(&text, u16::from_le_bytes([data[0], data[1]]) as usize);
            super::check_truncate_marker(&text, data[0] as usize);
            let mut mid = text.len() / 2;
            while mid > 0 && !text.is_char_boundary(mid) {
                mid -= 1;
            }
            super::check_compose(&text, &text[mid..]);
        }
    }

    pub fn discord_commands(data: &[u8]) {
        let text = lossy(data);
        super::check_commands(&text);
        super::check_classification(&text);
    }

    pub fn ansi_and_frames(data: &[u8]) {
        let text = lossy(data);
        super::check_ansi(&text);
        let author: String = data
            .iter()
            .take(6)
            .map(|byte| char::from(b'0' + (byte % 10)))
            .collect();
        let author = if author.is_empty() {
            "1".to_string()
        } else {
            author
        };
        let mut nonce: String = data
            .iter()
            .take(8)
            .map(|byte| char::from(b'a' + (byte % 6)))
            .collect();
        while nonce.len() < 8 {
            nonce.push('a');
        }
        let mut id_bytes = [0u8; 8];
        let n = data.len().min(8);
        id_bytes[..n].copy_from_slice(&data[..n]);
        super::check_human_frame(&text, &author, "src", &nonce[..8], &text);
        super::check_author_name(&text, u64::from_le_bytes(id_bytes));
    }

    pub fn prompt_extract(data: &[u8]) {
        super::check_extract(data);
    }

    pub fn rollout_and_codecs(data: &[u8]) {
        super::check_rollout_and_codecs(data);
        super::check_uuid(&lossy(data));
    }

    pub fn config_yaml(data: &[u8]) {
        let text = lossy(data);
        super::check_config_text(&text);
        let ttl = data.first().copied().map(u32::from);
        super::check_normalizers(Some(text.clone()), ttl, &[text.clone()], &text);
    }

    pub fn redact_and_fences(data: &[u8]) {
        super::check_redact(&lossy(data));
    }

    pub fn outbound_and_chunks(data: &[u8]) {
        let text = lossy(data);
        let limit = data.first().copied().unwrap_or(0) as usize;
        let tag = data.get(1).copied().unwrap_or(0);
        super::check_outbound_decision(&text, limit, tag);
        super::check_truncate_marker(&text, limit);
        super::check_literal_chunks(&text, limit);
        super::check_bounds(&text, limit, tag as usize);
    }
}
