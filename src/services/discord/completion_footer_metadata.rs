//! Shared terminal metadata for Discord completion footers.

use sqlx::Row;

use super::{ProviderKind, SharedData};
use crate::services::terminal_status_formatting::{
    format_elapsed_status, format_quota_status, format_subtext_lines,
};

const ELAPSED_PREFIX: &str = "⏱ ";
const RATE_LIMIT_PREFIX: &str = "⏳ ";
const HOST_PREFIX: &str = "🖥️ ";
const METADATA_SUFFIX_SEPARATOR: &str = "\u{2063}";
const COMPLETED_FOOTER_PREFIX: &str = "-# ";
const COMPLETED_FOOTER_SEPARATOR: &str = " · ";
const MAX_MODEL_LABEL_CHARS: usize = 48;
const MAX_HOST_LABEL_CHARS: usize = 63;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct CompletedTurnFooterSnapshot {
    pub(in crate::services::discord) elapsed_secs: Option<u64>,
    pub(in crate::services::discord) model: Option<String>,
    pub(in crate::services::discord) approved_model_ids: Vec<String>,
    pub(in crate::services::discord) host: Option<String>,
    pub(in crate::services::discord) context_used_tokens: Option<u128>,
    pub(in crate::services::discord) context_window_tokens: Option<u128>,
}

pub(in crate::services::discord) fn render_completed_turn_footer(
    snapshot: &CompletedTurnFooterSnapshot,
) -> Option<String> {
    let mut segments = Vec::with_capacity(4);
    if let Some(elapsed_secs) = snapshot.elapsed_secs {
        segments.push(format!(
            "{ELAPSED_PREFIX}{}",
            format_compact_elapsed(elapsed_secs)
        ));
    }
    if let Some(model) = snapshot
        .model
        .as_deref()
        .and_then(|model| approved_model_label(model, &snapshot.approved_model_ids))
    {
        segments.push(model);
    }
    if let Some(host) = snapshot.host.as_deref().and_then(sanitize_host_label) {
        segments.push(format!("{HOST_PREFIX}{host}"));
    }
    if let (Some(used), Some(window)) =
        (snapshot.context_used_tokens, snapshot.context_window_tokens)
    {
        if let Some(percent) = context_percent_floor(used, window) {
            segments.push(format!("📦 {} ({percent}%)", format_compact_tokens(used)));
        }
    }
    (!segments.is_empty()).then(|| {
        format!(
            "{COMPLETED_FOOTER_PREFIX}{}",
            segments.join(COMPLETED_FOOTER_SEPARATOR)
        )
    })
}

fn format_compact_elapsed(total_secs: u64) -> String {
    if total_secs == 0 {
        return "0s".to_string();
    }
    let units = [(86_400, "d"), (3_600, "h"), (60, "m"), (1, "s")];
    let mut remaining = total_secs;
    let mut rendered = String::new();
    for (unit_secs, suffix) in units {
        let value = remaining / unit_secs;
        remaining %= unit_secs;
        if value > 0 {
            rendered.push_str(&format!("{value}{suffix}"));
        }
    }
    rendered
}

fn format_compact_tokens(tokens: u128) -> String {
    const UNITS: [(u128, &str); 4] = [
        (1_000_000_000_000, "t"),
        (1_000_000_000, "b"),
        (1_000_000, "m"),
        (1_000, "k"),
    ];
    for (scale, suffix) in UNITS {
        if tokens >= scale {
            let whole = tokens / scale;
            let decimal = (tokens % scale) / (scale / 10);
            return if decimal == 0 {
                format!("{whole}{suffix}")
            } else {
                format!("{whole}.{decimal}{suffix}")
            };
        }
    }
    tokens.to_string()
}

fn context_percent_floor(used: u128, window: u128) -> Option<u8> {
    if window == 0 {
        return None;
    }
    let used = used.min(window);
    let window_hundredths = window / 100;
    let window_remainder = window % 100;
    (0..=100).rev().find(|percent| {
        let percent = u128::from(*percent);
        let fractional = window_remainder * percent;
        let threshold = window_hundredths * percent + fractional.div_ceil(100);
        used >= threshold
    })
}

fn approved_model_label(value: &str, approved_model_ids: &[String]) -> Option<String> {
    if !is_valid_approved_model_id(value) {
        return None;
    }
    approved_model_ids
        .iter()
        .any(|approved| approved == value && is_valid_approved_model_id(approved))
        .then(|| value.to_string())
}

fn is_valid_approved_model_id(value: &str) -> bool {
    !value.is_empty()
        && value.trim() == value
        && !value.chars().any(char::is_control)
        && value.chars().count() <= MAX_MODEL_LABEL_CHARS
}

fn sanitize_host_label(value: &str) -> Option<String> {
    let value = sanitize_bounded_label(value, MAX_HOST_LABEL_CHARS, false)?;
    let has_long_digit_run = value
        .split(|character: char| !character.is_ascii_digit())
        .any(|digits| digits.len() >= 4);
    (value.is_ascii() && !has_long_digit_run).then_some(value)
}

fn sanitize_bounded_label(value: &str, max_chars: usize, allow_model_dots: bool) -> Option<String> {
    if value.is_empty() || value.trim() != value || value.chars().any(char::is_control) {
        return None;
    }
    let allowed = |character: char| {
        character.is_alphanumeric() || character == '-' || (allow_model_dots && character == '.')
    };
    if !value.chars().all(allowed)
        || !value.chars().next().is_some_and(char::is_alphanumeric)
        || !value.chars().last().is_some_and(char::is_alphanumeric)
        || value.contains("..")
        || contains_private_identity_label(value)
        || looks_like_uuid(value)
        || (allow_model_dots && looks_like_fqdn(value))
    {
        return None;
    }
    let truncated = value.chars().take(max_chars).collect::<String>();
    let truncated = truncated.trim_end_matches(['-', '.']);
    (!truncated.is_empty()).then(|| truncated.to_string())
}

fn contains_private_identity_label(value: &str) -> bool {
    value.split(['-', '.', '_', ':', '/']).any(|component| {
        ["pid", "session", "channel", "thread", "dispatch"]
            .iter()
            .any(|reserved| component.eq_ignore_ascii_case(reserved))
    })
}

fn looks_like_uuid(value: &str) -> bool {
    value.matches('-').count() >= 4
        && value
            .chars()
            .all(|character| character == '-' || character.is_ascii_hexdigit())
}

fn looks_like_fqdn(value: &str) -> bool {
    let Some((prefix, suffix)) = value.rsplit_once('.') else {
        return false;
    };
    !prefix.is_empty()
        && suffix.len() >= 2
        && suffix
            .chars()
            .all(|character| character.is_ascii_alphabetic())
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(in crate::services::discord) struct CompletionFooterMetadata {
    elapsed: Option<String>,
    context: Option<String>,
    rate_limit: Option<String>,
    host: Option<String>,
}

pub(in crate::services::discord) async fn load_completion_footer_metadata(
    shared: &SharedData,
    provider: &ProviderKind,
    owner_started_at_unix: i64,
    inflight_started_at: Option<&str>,
) -> CompletionFooterMetadata {
    let rate_limit = terminal_rate_limit_summary(shared, provider).await;
    let host = crate::config_live_reload::current()
        .is_some_and(|config| config.cluster.enabled)
        .then(crate::services::cluster::node_registry::resolve_self_instance_id_without_config);
    completion_footer_metadata_at(
        chrono::Utc::now().timestamp(),
        owner_started_at_unix,
        inflight_started_at,
        rate_limit,
        host,
    )
}

pub(in crate::services::discord) fn append_completion_footer_metadata(
    block: String,
    metadata: &CompletionFooterMetadata,
) -> String {
    let (content, existing) = split_metadata_suffix(&block);
    let (content, rendered_context) = take_context_section(content);
    let merged = CompletionFooterMetadata {
        elapsed: existing.elapsed.or_else(|| metadata.elapsed.clone()),
        context: existing.context.or(rendered_context),
        rate_limit: existing.rate_limit.or_else(|| metadata.rate_limit.clone()),
        host: existing.host.or_else(|| metadata.host.clone()),
    };
    let lines = merged.lines();
    if lines.is_empty() {
        return block;
    }
    let content = content.trim_end();
    if content.is_empty() {
        format!("{METADATA_SUFFIX_SEPARATOR}\n{}", lines.join("\n"))
    } else {
        format!(
            "{content}\n\n{METADATA_SUFFIX_SEPARATOR}\n{}",
            lines.join("\n")
        )
    }
}

pub(in crate::services::discord) fn completion_footer_metadata_from_block(
    block: Option<&str>,
) -> CompletionFooterMetadata {
    block
        .map(split_metadata_suffix)
        .map(|(_, metadata)| metadata)
        .unwrap_or_default()
}

impl CompletionFooterMetadata {
    pub(in crate::services::discord) fn subtext_lines(&self) -> Vec<String> {
        let lines = self.lines();
        format_subtext_lines(lines.iter().map(String::as_str))
    }

    fn lines(&self) -> Vec<String> {
        [
            self.elapsed
                .as_deref()
                .map(|value| format!("{ELAPSED_PREFIX}{value}")),
            self.context.clone(),
            self.rate_limit
                .as_deref()
                .map(|value| format!("{RATE_LIMIT_PREFIX}{value}")),
            self.host
                .as_deref()
                .map(|value| format!("{HOST_PREFIX}{value}")),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

fn take_context_section(content: &str) -> (String, Option<String>) {
    let mut sections = content.split("\n\n").collect::<Vec<_>>();
    let Some(index) = sections.iter().position(|section| is_context_line(section)) else {
        return (content.to_string(), None);
    };
    let context = sections.remove(index).trim().to_string();
    (sections.join("\n\n"), Some(context))
}

fn is_context_line(line: &str) -> bool {
    matches!(line.trim().chars().next(), Some('📦' | '⚠')) && line.contains("auto-compact")
}

fn split_metadata_suffix(block: &str) -> (&str, CompletionFooterMetadata) {
    let separator = format!("\n{METADATA_SUFFIX_SEPARATOR}\n");
    let Some((content, suffix)) = block.split_once(&separator) else {
        return (block, CompletionFooterMetadata::default());
    };
    let mut metadata = CompletionFooterMetadata::default();
    for segment in suffix.split(&separator) {
        for line in segment.lines().map(str::trim) {
            if let Some(value) = line.strip_prefix(ELAPSED_PREFIX) {
                metadata
                    .elapsed
                    .get_or_insert_with(|| value.trim().to_string());
            } else if is_context_line(line) {
                metadata.context.get_or_insert_with(|| line.to_string());
            } else if let Some(value) = line.strip_prefix(RATE_LIMIT_PREFIX) {
                metadata
                    .rate_limit
                    .get_or_insert_with(|| value.trim().to_string());
            } else if let Some(value) = line.strip_prefix(HOST_PREFIX) {
                metadata
                    .host
                    .get_or_insert_with(|| value.trim().to_string());
            } else {
                return (block, CompletionFooterMetadata::default());
            }
        }
    }
    if metadata.lines().is_empty() {
        (block, CompletionFooterMetadata::default())
    } else {
        (content, metadata)
    }
}

fn completion_footer_metadata_at(
    now_unix: i64,
    owner_started_at_unix: i64,
    inflight_started_at: Option<&str>,
    rate_limit: Option<String>,
    host: Option<String>,
) -> CompletionFooterMetadata {
    let started_at_unix = (owner_started_at_unix > 0)
        .then_some(owner_started_at_unix)
        .or_else(|| inflight_started_at.and_then(super::inflight::parse_started_at_unix));
    let elapsed = started_at_unix
        .and_then(|started_at| format_elapsed_status(now_unix.saturating_sub(started_at)))
        .and_then(|line| line.strip_prefix(ELAPSED_PREFIX).map(str::to_string));
    CompletionFooterMetadata {
        elapsed,
        context: None,
        rate_limit: rate_limit.and_then(|value| nonempty(&value)),
        host: host.and_then(|value| nonempty(&value)),
    }
}

fn nonempty(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()).then(|| value.to_string())
}

async fn terminal_rate_limit_summary(
    shared: &SharedData,
    provider: &ProviderKind,
) -> Option<String> {
    let data = if let Some(pool) = shared.pg_pool.as_ref() {
        let provider = provider.as_str();
        sqlx::query("SELECT data FROM rate_limit_cache WHERE lower(provider) = lower($1) LIMIT 1")
            .bind(provider)
            .fetch_optional(pool)
            .await
            .ok()
            .flatten()
            .and_then(|row| row.try_get::<String, _>("data").ok())
    } else {
        None
    };
    format_quota_status(data.as_deref(), chrono::Utc::now().timestamp())
}

#[cfg(test)]
pub(in crate::services::discord) mod tests {
    use super::*;
    use chrono::TimeZone;

    fn completed_snapshot() -> CompletedTurnFooterSnapshot {
        CompletedTurnFooterSnapshot {
            elapsed_secs: Some(92),
            model: Some("fable-5".to_string()),
            approved_model_ids: vec!["fable-5".to_string()],
            host: Some("mac-book".to_string()),
            context_used_tokens: Some(261_000),
            context_window_tokens: Some(1_000_000),
        }
    }

    #[test]
    fn completed_footer_renders_owner_format_and_fixed_order_4860() {
        assert_eq!(
            render_completed_turn_footer(&completed_snapshot()).as_deref(),
            Some("-# ⏱ 1m32s · fable-5 · 🖥️ mac-book · 📦 261k (26%)")
        );
    }

    #[test]
    fn completed_footer_elapsed_boundaries_are_compact_4860() {
        for (seconds, expected) in [
            (0, "0s"),
            (1, "1s"),
            (59, "59s"),
            (60, "1m"),
            (61, "1m1s"),
            (3_599, "59m59s"),
            (3_600, "1h"),
            (3_661, "1h1m1s"),
            (86_400, "1d"),
            (90_061, "1d1h1m1s"),
        ] {
            let snapshot = CompletedTurnFooterSnapshot {
                elapsed_secs: Some(seconds),
                ..Default::default()
            };
            assert_eq!(
                render_completed_turn_footer(&snapshot).as_deref(),
                Some(format!("-# ⏱ {expected}").as_str())
            );
        }
    }

    #[test]
    fn completed_footer_token_boundaries_are_decimal_and_lowercase_4860() {
        for (tokens, expected) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1k"),
            (1_099, "1k"),
            (1_100, "1.1k"),
            (261_999, "261.9k"),
            (1_000_000, "1m"),
            (1_500_000, "1.5m"),
            (1_000_000_000, "1b"),
            (1_000_000_000_000, "1t"),
            (u128::MAX, "340282366920938463463374607.4t"),
        ] {
            let snapshot = CompletedTurnFooterSnapshot {
                context_used_tokens: Some(tokens),
                context_window_tokens: Some(u128::MAX),
                ..Default::default()
            };
            let rendered = render_completed_turn_footer(&snapshot).expect("context segment");
            let expected_percent = if tokens == u128::MAX { 100 } else { 0 };
            assert_eq!(rendered, format!("-# 📦 {expected} ({expected_percent}%)"));
        }
    }

    #[test]
    fn completed_footer_percent_is_floor_clamped_and_overflow_safe_4860() {
        for (used, window, expected) in [
            (1, 3, 33),
            (2, 3, 66),
            (999, 1_000, 99),
            (1_001, 1_000, 100),
            (u128::MAX, u128::MAX, 100),
            (u128::MAX - 1, u128::MAX, 99),
        ] {
            let snapshot = CompletedTurnFooterSnapshot {
                context_used_tokens: Some(used),
                context_window_tokens: Some(window),
                ..Default::default()
            };
            let rendered = render_completed_turn_footer(&snapshot).expect("context segment");
            assert!(rendered.ends_with(&format!("({expected}%)")), "{rendered}");
        }
    }

    #[test]
    fn completed_footer_omits_missing_and_zero_window_context_4860() {
        assert_eq!(
            render_completed_turn_footer(&CompletedTurnFooterSnapshot::default()),
            None
        );
        assert_eq!(
            render_completed_turn_footer(&CompletedTurnFooterSnapshot {
                context_used_tokens: Some(42),
                context_window_tokens: Some(0),
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            render_completed_turn_footer(&CompletedTurnFooterSnapshot {
                model: Some("fable-5".to_string()),
                approved_model_ids: vec!["fable-5".to_string()],
                context_used_tokens: Some(42),
                ..Default::default()
            })
            .as_deref(),
            Some("-# fable-5")
        );
    }

    #[test]
    fn completed_footer_renders_only_exact_approved_model_ids_4860() {
        for model in [
            "sonnet[1m]",
            "opus[1m]",
            "routed-sonnet[1m]",
            "anthropic/claude-sonnet-4-5",
            "anthropic/claude-sonnet-4-6[1m]",
            "openai/gpt-5.1",
            "openrouter/google-gemini-2.5-pro:free",
        ] {
            let snapshot = CompletedTurnFooterSnapshot {
                model: Some(model.to_string()),
                approved_model_ids: vec![model.to_string()],
                ..Default::default()
            };
            assert_eq!(
                render_completed_turn_footer(&snapshot).as_deref(),
                Some(format!("-# {model}").as_str()),
                "{model}"
            );
        }
    }

    #[test]
    fn completed_footer_model_membership_is_exact_and_fail_closed_4860() {
        let approved = "anthropic/claude-sonnet-4-5";
        for rejected in [
            "Anthropic/claude-sonnet-4-5",
            "anthropic/CLAUDE-sonnet-4-5",
            " anthropic/claude-sonnet-4-5",
            "anthropic/claude-sonnet-4-5 ",
            "anthropic/claude-sonnet-4-5\n",
            "host.example.com",
            "例子.公司",
            "../claude-sonnet-4-5",
            "/opt/models/claude-sonnet-4-5",
            "pid-1234",
            "session-deadbeef",
            "550e8400-e29b-41d4-a716-446655440000",
            "openai/AKIAIOSFODNN7EXAMPLE",
            "openai/eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOjF9.sig",
            "<@123456789>",
            "**fable-5**",
        ] {
            let snapshot = CompletedTurnFooterSnapshot {
                model: Some(rejected.to_string()),
                approved_model_ids: vec![approved.to_string()],
                ..Default::default()
            };
            assert_eq!(render_completed_turn_footer(&snapshot), None, "{rejected}");
        }
    }

    #[test]
    fn completed_footer_rejects_invalid_approved_entries_and_overlength_models_4860() {
        for invalid in [
            " approved",
            "approved ",
            "approved\nmodel",
            "approved\tmodel",
            &"a".repeat(MAX_MODEL_LABEL_CHARS + 1),
        ] {
            let snapshot = CompletedTurnFooterSnapshot {
                model: Some(invalid.to_string()),
                approved_model_ids: vec![invalid.to_string()],
                ..Default::default()
            };
            assert_eq!(render_completed_turn_footer(&snapshot), None, "{invalid:?}");
        }
    }

    #[test]
    fn completed_footer_accepted_model_is_exact_deterministic_and_bounded_4860() {
        let model = "anthropic/claude-sonnet-4-6[1m]";
        let snapshot = CompletedTurnFooterSnapshot {
            model: Some(model.to_string()),
            approved_model_ids: vec![model.to_string(), model.to_string()],
            ..Default::default()
        };
        let once = render_completed_turn_footer(&snapshot).expect("approved model");
        let twice = render_completed_turn_footer(&snapshot).expect("approved model");

        assert_eq!(once, twice);
        assert_eq!(once, format!("-# {model}"));
        assert_eq!(once.strip_prefix(COMPLETED_FOOTER_PREFIX), Some(model));
        assert!(model.chars().count() <= MAX_MODEL_LABEL_CHARS);
    }

    #[test]
    fn completed_footer_sanitizes_host_injection_paths_and_private_identity_4860() {
        for malicious_host in [
            "fable-5\n@everyone",
            "<@123456789>",
            "**fable-5**",
            "../fable-5",
            "/opt/models/fable-5",
            "pid-1234",
            "session-deadbeef",
            "channel-1234",
            "550e8400-e29b-41d4-a716-446655440000",
            "host.example.com",
            "12345",
            "mac-1234",
            "맥북",
            " fable-5",
            "fable-5 ",
        ] {
            let snapshot = CompletedTurnFooterSnapshot {
                host: Some(malicious_host.to_string()),
                ..Default::default()
            };
            assert_eq!(
                render_completed_turn_footer(&snapshot),
                None,
                "{malicious_host}"
            );
        }
    }

    #[test]
    fn completed_footer_truncates_host_on_utf8_character_boundary_4860() {
        let rendered = render_completed_turn_footer(&CompletedTurnFooterSnapshot {
            host: Some(format!("{}zz", "a".repeat(MAX_HOST_LABEL_CHARS))),
            ..Default::default()
        })
        .expect("safe host");
        let expected_host = "a".repeat(MAX_HOST_LABEL_CHARS);

        assert_eq!(rendered, format!("-# 🖥️ {expected_host}"));
        assert_eq!(rendered.lines().count(), 1);
    }

    #[test]
    fn completed_footer_is_single_line_deterministic_and_privacy_bounded_4860() {
        let snapshot = completed_snapshot();
        let once = render_completed_turn_footer(&snapshot).expect("footer");
        let twice = render_completed_turn_footer(&snapshot).expect("footer");

        assert_eq!(once, twice);
        assert_eq!(once.lines().count(), 1);
        for forbidden in [
            "quota",
            "threshold",
            "payload",
            "channel",
            "session",
            "dispatch",
            "5h:",
            "7d:",
            METADATA_SUFFIX_SEPARATOR,
        ] {
            assert!(!once.contains(forbidden), "{once}");
        }
    }

    #[test]
    fn visible_completed_footer_never_becomes_internal_metadata_4860() {
        let visible = "-# ⏱ 1m32s · fable-5 · 🖥️ mac-book · 📦 261k (26%)";
        assert_eq!(
            completion_footer_metadata_from_block(Some(visible)),
            CompletionFooterMetadata::default()
        );
        let metadata = completion_footer_metadata_at(
            1_800_000_154,
            1_800_000_000,
            None,
            None,
            Some("node-a".to_string()),
        );
        let appended = append_completion_footer_metadata(visible.to_string(), &metadata);

        assert!(appended.starts_with(visible));
        assert_eq!(appended.matches("-# ⏱ 1m32s").count(), 1);
        assert_eq!(appended.matches(METADATA_SUFFIX_SEPARATOR).count(), 1);
    }

    #[test]
    fn ownerless_metadata_uses_inflight_started_at_fallback_4806() {
        let fallback_started_at = chrono::Local
            .timestamp_opt(1_800_000_000, 0)
            .single()
            .expect("valid local timestamp")
            .format("%Y-%m-%d %H:%M:%S")
            .to_string();
        let metadata =
            completion_footer_metadata_at(1_800_000_154, 0, Some(&fallback_started_at), None, None);
        let block = append_completion_footer_metadata("Context".to_string(), &metadata);

        assert_eq!(
            block,
            format!("Context\n\n{METADATA_SUFFIX_SEPARATOR}\n⏱ 2m 34s")
        );
    }

    #[test]
    fn metadata_appender_is_idempotent_and_cache_miss_omits_quota_4806() {
        let metadata = completion_footer_metadata_at(
            1_800_000_154,
            1_800_000_000,
            None,
            None,
            Some("node-a".to_string()),
        );
        let once = append_completion_footer_metadata("Context".to_string(), &metadata);
        let twice = append_completion_footer_metadata(once.clone(), &metadata);

        assert_eq!(twice, once);
        assert_eq!(twice.matches("⏱ ").count(), 1);
        assert_eq!(twice.matches("🖥️ ").count(), 1);
        assert!(!twice.contains("⏳ "));
    }

    pub(in crate::services::discord) fn metadata_fixture_for_task_card_4806()
    -> CompletionFooterMetadata {
        completion_footer_metadata_at(
            1_800_000_154,
            1_800_000_000,
            None,
            Some("5h 80% · 7d 60%".to_string()),
            Some("node-a".to_string()),
        )
    }

    #[tokio::test]
    async fn metadata_load_omits_quota_when_cache_is_unavailable_4822() {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let metadata =
            load_completion_footer_metadata(&shared, &ProviderKind::Claude, 1_800_000_000, None)
                .await;
        let block = append_completion_footer_metadata(
            "📦 265.0k / 1.0M (26%) · auto-compact 80%".to_string(),
            &metadata,
        );

        assert!(block.contains("📦 265.0k / 1.0M (26%) · auto-compact 80%"));
        assert!(!block.contains("⏳ "));
        assert!(!block.contains("ctw:"));
    }

    #[test]
    fn completion_footer_fixture_pins_requested_order_and_dedup_4846() {
        let metadata = completion_footer_metadata_at(
            1_800_000_154,
            1_800_000_000,
            None,
            Some("5h: 3% (4h39m) │ 7d: 47% (4d20h) │ 7d-F: 34% (4d20h)".to_string()),
            Some("node-a".to_string()),
        );
        let initial = append_completion_footer_metadata(
            "Background agents\nWaiting for background agents ⠸\n\n📦 265.0k / 1.0M (26%) · auto-compact 80%".to_string(),
            &metadata,
        );
        let recovered = completion_footer_metadata_from_block(Some(&initial));
        let refreshed = append_completion_footer_metadata(
            "Background agents\nWaiting for background agents ⠸\n\n📦 265.0k / 1.0M (26%) · auto-compact 80%".to_string(),
            &recovered,
        );

        assert_eq!(initial, refreshed);
        assert_eq!(
            super::super::single_message_panel::completion_footer_subtext(&initial),
            "-# Background agents\n-# Waiting for background agents ⠸\n\n-# ⏱ 2m 34s\n-# 📦 265.0k / 1.0M (26%) · auto-compact 80%\n-# ⏳ 5h: 3% (4h39m) │ 7d: 47% (4d20h) │ 7d-F: 34% (4d20h)\n-# 🖥️ node-a"
        );
        assert!(!initial.contains("ctw:"));
    }

    #[test]
    fn bridge_and_watcher_finalize_inputs_render_identical_metadata_4806() {
        let metadata = completion_footer_metadata_at(
            1_800_000_154,
            1_800_000_000,
            None,
            Some("5h 80% · 7d 60%".to_string()),
            Some("node-a".to_string()),
        );
        let bridge_block = append_completion_footer_metadata("Context".to_string(), &metadata);
        let watcher_block = append_completion_footer_metadata("Context".to_string(), &metadata);

        assert_eq!(bridge_block, watcher_block);
        assert!(bridge_block.contains("⏱ 2m 34s"));
        assert!(bridge_block.contains("⏳ 5h 80% · 7d 60%"));
        assert!(bridge_block.contains("🖥️ node-a"));
    }

    #[test]
    fn metadata_roundtrip_preserves_all_available_lines_4806() {
        let metadata = completion_footer_metadata_at(
            1_800_000_154,
            1_800_000_000,
            None,
            Some("5h 80% · 7d 60%".to_string()),
            Some("node-a".to_string()),
        );
        let initial = append_completion_footer_metadata("Context".to_string(), &metadata);
        let recovered = completion_footer_metadata_from_block(Some(&initial));
        let refreshed =
            append_completion_footer_metadata("Subagents\n└ worker ⠸".to_string(), &recovered);

        assert!(refreshed.contains("⏱ 2m 34s"));
        assert!(refreshed.contains("⏳ 5h 80% · 7d 60%"));
        assert!(refreshed.contains("🖥️ node-a"));
    }

    #[test]
    fn quoted_prefix_lines_never_suppress_or_recover_metadata_4806() {
        let metadata = completion_footer_metadata_at(
            1_800_000_154,
            1_800_000_000,
            None,
            Some("5h 80%".to_string()),
            Some("node-a".to_string()),
        );
        let quoted = "Tasks\n⏱ quoted duration\n⏳ quoted quota\n🖥️ quoted host";
        let appended = append_completion_footer_metadata(quoted.to_string(), &metadata);

        assert_eq!(appended.matches("⏱ ").count(), 2);
        assert_eq!(appended.matches("⏳ ").count(), 2);
        assert_eq!(appended.matches("🖥️ ").count(), 2);
        assert_eq!(
            completion_footer_metadata_from_block(Some(quoted)),
            CompletionFooterMetadata::default()
        );
    }

    #[test]
    fn duplicate_separators_never_leave_old_metadata_in_content_4806() {
        let duplicated = format!(
            "Context\n\n{METADATA_SUFFIX_SEPARATOR}\n⏱ 2m 34s\n\n{METADATA_SUFFIX_SEPARATOR}\n⏳ 5h 80%"
        );
        let metadata = completion_footer_metadata_at(
            1_800_000_154,
            1_800_000_000,
            None,
            Some("5h 80%".to_string()),
            None,
        );
        let repaired = append_completion_footer_metadata(duplicated, &metadata);

        assert_eq!(repaired.matches(METADATA_SUFFIX_SEPARATOR).count(), 1);
        assert_eq!(repaired.matches("⏱ ").count(), 1);
        assert_eq!(repaired.matches("⏳ ").count(), 1);
        assert!(repaired.starts_with("Context\n\n"));
    }

    #[test]
    fn malformed_suffix_does_not_become_refresh_metadata_4806() {
        let block =
            format!("Tasks\n\n{METADATA_SUFFIX_SEPARATOR}\n⏱ quoted duration\nnot metadata");
        assert_eq!(
            completion_footer_metadata_from_block(Some(&block)),
            CompletionFooterMetadata::default()
        );
    }
}
