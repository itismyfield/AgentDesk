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
    fn usage_percent(&self) -> Option<u64> {
        if self.context_window_tokens == 0 {
            return None;
        }
        let used_tokens = self
            .input_tokens
            .saturating_add(self.cache_create_tokens)
            .saturating_add(self.cache_read_tokens);
        let percent = (u128::from(used_tokens) * 100) / u128::from(self.context_window_tokens);
        Some(percent.min(100) as u64)
    }
}

pub(super) fn render_context_panel_line(context: &ContextPanelSnapshot) -> Option<String> {
    let usage_percent = context.usage_percent()?;
    let icon = if usage_percent >= 85 {
        "⚠️"
    } else {
        "📦"
    };
    let mut line = format!(
        "Context   {icon} {usage_percent}% used · auto-compact {}%",
        context.compact_percent
    );
    if usage_percent >= 85 {
        line.push_str(" — 자동 압축 직전");
    } else if usage_percent >= 75 {
        line.push_str(" (임박)");
    }
    Some(truncate_chars(&line, CONTEXT_PANEL_LINE_MAX_CHARS))
}
