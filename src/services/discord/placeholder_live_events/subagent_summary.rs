//! TUI-parity `Done (N tools · M tokens · Xs)` summary formatting for finished
//! subagent slots (#3086). Split out of `status_panel.rs` to keep that file
//! within the placeholder_live_events namespace size cap.

use crate::services::agent_protocol::SubagentSummary;

/// Formats the TUI-parity `Done (N tools · M tokens · Xs)` summary. Each part is
/// included only when present, so a partial summary still renders what it has.
/// Returns `None` when no part is available.
pub(super) fn render_subagent_done_summary(summary: &SubagentSummary) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(count) = summary.tool_count {
        let noun = if count == 1 { "tool" } else { "tools" };
        parts.push(format!("{count} {noun}"));
    }
    if let Some(tokens) = summary.tokens {
        parts.push(format!("{} tokens", format_compact_count(tokens)));
    }
    if let Some(secs) = summary.duration_secs {
        parts.push(format_duration_secs(secs));
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("Done ({})", parts.join(" · ")))
}

/// Compact count formatting mirroring the TUI (`28824` → `28.8k`, `1_500_000`
/// → `1.5m`). Values under 1000 render verbatim.
fn format_compact_count(value: u64) -> String {
    if value < 1_000 {
        return value.to_string();
    }
    if value < 1_000_000 {
        let scaled = value as f64 / 1_000.0;
        return format!("{}k", trim_one_decimal(scaled));
    }
    let scaled = value as f64 / 1_000_000.0;
    format!("{}m", trim_one_decimal(scaled))
}

/// Renders a one-decimal number without a trailing `.0` (`28.8` stays,
/// `19.0` → `19`).
fn trim_one_decimal(value: f64) -> String {
    let rounded = (value * 10.0).round() / 10.0;
    if (rounded.fract()).abs() < f64::EPSILON {
        format!("{}", rounded as u64)
    } else {
        format!("{rounded:.1}")
    }
}

/// Humanizes a duration in seconds the way the TUI does: `45s`, `19m`, `1h2m`.
fn format_duration_secs(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs}s");
    }
    let minutes = secs / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    let rem_minutes = minutes % 60;
    if rem_minutes == 0 {
        format!("{hours}h")
    } else {
        format!("{hours}h{rem_minutes}m")
    }
}
