//! Single-line task surfaces; completion metadata remains with the response footer.
use super::super::{session_notice as inline, tui_task_card as card};
use super::*;

impl TaskCardPayload {
    pub(super) fn is_monitor(&self) -> bool {
        matches!(self, Self::Task(note) if note.summary.as_deref().unwrap_or("").trim_start().starts_with("Monitor event"))
    }

    pub(super) fn is_completion_card(&self) -> bool {
        match self {
            Self::Task(note) => {
                !self.is_monitor() && note.result.as_deref().is_some_and(|s| !s.trim().is_empty())
            }
            Self::Subagent(card) => card.lines().skip(1).any(|line| !line.trim().is_empty()),
        }
    }

    pub(super) fn monitor_fingerprint(&self) -> String {
        let Self::Task(note) = self else {
            return content_hash(&format!("{self:?}"));
        };
        full_fingerprint(&[
            "monitor-observation-v1",
            note.task_id.as_deref().unwrap_or(""),
            note.tool_use_id.as_deref().unwrap_or(""),
            note.status.as_deref().unwrap_or(""),
            &card::decode_entities_once(note.summary.as_deref().unwrap_or("")),
            &card::decode_entities_once(note.result.as_deref().unwrap_or("")),
        ])
    }

    pub(super) fn legacy_render(&self, count: u64) -> String {
        match self {
            Self::Task(note) => card::format_task_notification_card(note, count),
            Self::Subagent(text) => text.clone(),
        }
    }

    pub(super) fn render(&self, count: u64) -> String {
        match self {
            Self::Task(note) if self.is_completion_card() => {
                card::format_task_notification_card(note, count)
            }
            Self::Task(note) => {
                let status = note.status.as_deref().unwrap_or("");
                let summary = card::decode_entities_once(note.summary.as_deref().unwrap_or(""));
                let result = card::decode_entities_once(note.result.as_deref().unwrap_or(""));
                if self.is_monitor() {
                    let description = summary
                        .trim()
                        .strip_prefix("Monitor event:")
                        .unwrap_or(&summary)
                        .trim()
                        .trim_matches('"');
                    let detail = if result.trim().is_empty() {
                        inline::preview(description)
                    } else {
                        format!(
                            "{}: {}",
                            inline::preview(description),
                            inline::preview(&result)
                        )
                    };
                    inline::finish(format!("-# 📡 {detail} · {count}회"))
                } else {
                    let detail = if !summary.trim().is_empty() {
                        summary
                    } else if !result.trim().is_empty() {
                        result
                    } else if status.is_empty() {
                        "Task notification".to_string()
                    } else {
                        format!("Task {status}")
                    };
                    inline::subtext(card::status_icon(status), &detail)
                }
            }
            Self::Subagent(text) if self.is_completion_card() => text.clone(),
            Self::Subagent(text) => {
                let mut lines = text.lines();
                let header = lines.next().unwrap_or("📋 Subagent notification");
                let icon = if header.starts_with('✅') {
                    "✅"
                } else if header.starts_with('❌') {
                    "❌"
                } else {
                    "📋"
                };
                let body = lines
                    .filter(|line| !line.trim().is_empty())
                    .collect::<Vec<_>>()
                    .join("\n");
                inline::subtext(icon, if body.is_empty() { header } else { &body })
            }
        }
    }

    pub(super) fn render_with_completion_metadata(
        &self,
        count: u64,
        metadata: &super::super::completion_footer_metadata::CompletionFooterMetadata,
    ) -> String {
        let rendered = self.render(count);
        if !self.is_completion_card() {
            return rendered;
        }
        let lines = metadata.subtext_lines();
        if lines.is_empty() {
            return rendered;
        }
        card::clamp_discord_message_content(&format!(
            "{}\n\n{}",
            rendered.trim_end(),
            lines.join("\n")
        ))
    }
}
