use super::common::{CONTEXT_PANEL_LINE_MAX_CHARS, truncate_chars};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ContextPanelSnapshot {
    pub(super) input_tokens: u64,
    pub(super) cache_create_tokens: u64,
    pub(super) cache_read_tokens: u64,
    pub(super) context_window_tokens: u64,
    pub(super) compact_percent: u64,
}

impl ContextPanelSnapshot {
    fn used_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cache_create_tokens)
            .saturating_add(self.cache_read_tokens)
    }

    fn usage_percent(&self) -> Option<u64> {
        if self.context_window_tokens == 0 {
            return None;
        }
        let percent =
            (u128::from(self.used_tokens()) * 100) / u128::from(self.context_window_tokens);
        Some(percent.min(100) as u64)
    }
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

pub(super) fn render_context_panel_line(context: &ContextPanelSnapshot) -> Option<String> {
    let usage_percent = context.usage_percent()?;
    let icon = if usage_percent >= 85 {
        "⚠️"
    } else {
        "📦"
    };
    let used = format_token_count(context.used_tokens());
    let window = format_token_count(context.context_window_tokens);
    let mut line = format!(
        "Context   {icon} {used} / {window} tokens ({usage_percent}%) · auto-compact {}%",
        context.compact_percent
    );
    if usage_percent >= 85 {
        line.push_str(" — 자동 압축 직전");
    } else if usage_percent >= 75 {
        line.push_str(" (임박)");
    }
    Some(truncate_chars(&line, CONTEXT_PANEL_LINE_MAX_CHARS))
}
