//! Shared formatting for live-turn and completion status subtext.

use serde_json::Value;

const SUBTEXT_PREFIX: &str = "-# ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextWindowUsage {
    pub(crate) used_tokens: u64,
    pub(crate) window_tokens: u64,
}

/// Prefix every non-empty status line with Discord's subtext marker.
pub(crate) fn format_subtext_lines<'a>(lines: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    lines
        .into_iter()
        .filter_map(|line| {
            let line = line.trim();
            (!line.is_empty()).then(|| {
                if line.starts_with(SUBTEXT_PREFIX) {
                    line.to_string()
                } else {
                    format!("{SUBTEXT_PREFIX}{line}")
                }
            })
        })
        .collect()
}

pub(crate) fn format_elapsed_status(total_secs: i64) -> Option<String> {
    (total_secs > 0).then(|| format!("⏱ {}", format_turn_duration(total_secs)))
}

pub(crate) fn format_usage_status(
    cache_json: Option<&str>,
    now_unix: i64,
    context: Option<ContextWindowUsage>,
) -> Option<String> {
    let rendered = cache_json
        .and_then(|cache_json| serde_json::from_str::<Value>(cache_json).ok())
        .and_then(|value| value.get("buckets").and_then(Value::as_array).cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|bucket| render_bucket(bucket, now_unix))
        .collect::<Vec<_>>();
    render_usage_parts(rendered, context)
}

pub(crate) fn format_usage_status_segments<'a>(
    segments: impl IntoIterator<Item = &'a str>,
    context: Option<ContextWindowUsage>,
) -> Option<String> {
    let rendered = segments
        .into_iter()
        .filter_map(parse_rendered_bucket)
        .collect::<Vec<_>>();
    render_usage_parts(rendered, context)
}

fn render_usage_parts(
    mut rendered: Vec<RenderedBucket>,
    context: Option<ContextWindowUsage>,
) -> Option<String> {
    rendered.sort_by_key(|bucket| bucket.order);
    let mut parts = rendered
        .into_iter()
        .map(|bucket| bucket.text)
        .collect::<Vec<_>>();
    if let Some(context) = context.filter(|context| context.window_tokens > 0) {
        let used = context.used_tokens.min(context.window_tokens);
        let percent = ((u128::from(used) * 100) / u128::from(context.window_tokens)) as u64;
        parts.push(format!(
            "ctw: {percent}% ({}/{})",
            format_compact_tokens(used),
            format_compact_tokens(context.window_tokens)
        ));
    }
    (!parts.is_empty()).then(|| parts.join(" │ "))
}

#[derive(Debug, PartialEq, Eq)]
struct RenderedBucket {
    order: u8,
    text: String,
}

fn parse_rendered_bucket(segment: &str) -> Option<RenderedBucket> {
    let segment = segment.trim();
    let (order, prefix) = if segment.starts_with("5h:") {
        (0, "5h:")
    } else if segment.starts_with("7d-F:") {
        (2, "7d-F:")
    } else if segment.starts_with("7d:") {
        (1, "7d:")
    } else {
        return None;
    };
    Some(RenderedBucket {
        order,
        text: format!("{prefix} {}", segment.strip_prefix(prefix)?.trim_start()),
    })
}

fn render_bucket(bucket: &Value, now_unix: i64) -> Option<RenderedBucket> {
    let raw_name = bucket.get("name")?.as_str()?;
    let (order, name) = match raw_name {
        "5h" => (0, "5h"),
        "7d" => (1, "7d"),
        "7d Sonnet" | "7d-F" => (2, "7d-F"),
        _ => return None,
    };
    let used = bucket
        .get("used")
        .and_then(Value::as_i64)
        .or_else(|| {
            bucket
                .get("utilization")
                .and_then(Value::as_f64)
                .map(|value| value.floor() as i64)
        })
        .or_else(|| {
            bucket
                .get("remaining")
                .and_then(Value::as_i64)
                .map(|remaining| 100 - remaining)
        })?
        .clamp(0, 100);
    let reset = bucket.get("reset").and_then(Value::as_i64).unwrap_or(0);
    let reset_suffix = (reset > 0).then(|| {
        let remaining = reset.saturating_sub(now_unix).max(0);
        format!(" ({})", format_reset_duration(remaining))
    });
    Some(RenderedBucket {
        order,
        text: format!("{name}: {used}%{}", reset_suffix.unwrap_or_default()),
    })
}

fn format_turn_duration(total_secs: i64) -> String {
    let minutes = total_secs / 60;
    let seconds = total_secs % 60;
    if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

fn format_reset_duration(total_secs: i64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let minutes = (total_secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d{hours}h")
    } else if hours > 0 {
        format!("{hours}h{minutes}m")
    } else {
        format!("{minutes}m")
    }
}

fn format_compact_tokens(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{}K", tokens / 1_000)
    } else {
        tokens.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_usage_reset_fast_tier_and_context_window_4822() {
        let data = r#"{"buckets":[
            {"name":"7d Sonnet","used":34,"reset":1800417600},
            {"name":"5h","remaining":97,"reset":1800016740},
            {"name":"7d","utilization":47.8,"reset":1800417600}
        ]}"#;
        assert_eq!(
            format_usage_status(
                Some(data),
                1_800_000_000,
                Some(ContextWindowUsage {
                    used_tokens: 265_000,
                    window_tokens: 1_000_000,
                })
            )
            .as_deref(),
            Some("5h: 3% (4h39m) │ 7d: 47% (4d20h) │ 7d-F: 34% (4d20h) │ ctw: 26% (265K/1.0M)")
        );
    }

    #[test]
    fn renders_context_window_when_quota_is_unavailable_4822() {
        assert_eq!(
            format_usage_status(
                None,
                1_800_000_000,
                Some(ContextWindowUsage {
                    used_tokens: 265_000,
                    window_tokens: 1_000_000,
                })
            )
            .as_deref(),
            Some("ctw: 26% (265K/1.0M)")
        );
        assert_eq!(
            format_usage_status(
                Some("not-json"),
                1_800_000_000,
                Some(ContextWindowUsage {
                    used_tokens: 265_000,
                    window_tokens: 1_000_000,
                })
            )
            .as_deref(),
            Some("ctw: 26% (265K/1.0M)")
        );
    }

    #[test]
    fn shared_subtext_formatter_is_idempotent_4822() {
        assert_eq!(
            format_subtext_lines(["⠙ 진행 중", "-# 턴 시작 : now", ""]),
            vec!["-# ⠙ 진행 중", "-# 턴 시작 : now"]
        );
    }
}
