use std::collections::VecDeque;
use std::time::Instant;

use poise::serenity_prelude::ChannelId;
use serde_json::Value;

use super::tool_summary::{class, safe_tool_summary};
use crate::config::ToolActivityDisplay;
use crate::services::agent_protocol::StatusEvent;

const CAPACITY: usize = 64;
const DISPLAY_LIMIT: usize = 6;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    id: Option<String>,
    class: &'static str,
    summary: Option<String>,
    started: Instant,
    result: Option<bool>,
    elapsed: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct ToolActivityRing {
    rows: VecDeque<Row>,
    total: usize,
    failed: usize,
    evicted_fifo: usize,
    completed: bool,
}

pub(super) fn observation(
    name: Option<&str>,
    input: Option<&str>,
    id: Option<&str>,
    success: Option<bool>,
) -> StatusEvent {
    StatusEvent::ToolActivity {
        name: name.map(str::to_owned),
        input: input.map(str::to_owned),
        tool_use_id: id.filter(|id| !id.is_empty()).map(str::to_owned),
        success,
    }
}

impl ToolActivityRing {
    pub(super) fn is_empty(&self) -> bool {
        self.total == 0
    }

    pub(super) fn apply(&mut self, event: &StatusEvent, cwd: Option<&str>) {
        match event {
            StatusEvent::ToolActivity {
                name,
                input,
                tool_use_id,
                success,
            } => {
                if let Some(success) = success {
                    self.finish(tool_use_id.as_deref(), *success);
                } else if let Some(name) = name {
                    if tool_use_id
                        .as_ref()
                        .is_some_and(|id| self.rows.iter().any(|r| r.id.as_ref() == Some(id)))
                    {
                        return;
                    }
                    if self.rows.len() == CAPACITY {
                        let old = self.rows.pop_front().expect("full ring");
                        if old.id.is_none() && old.result.is_none() {
                            self.evicted_fifo = self.evicted_fifo.saturating_add(1);
                        }
                    }
                    self.total = self.total.saturating_add(1);
                    self.completed = false;
                    self.rows.push_back(Row {
                        id: tool_use_id.clone(),
                        class: class(name),
                        summary: input
                            .as_deref()
                            .and_then(|input| safe_tool_summary(name, input, cwd)),
                        started: Instant::now(),
                        result: None,
                        elapsed: None,
                    });
                }
            }
            StatusEvent::TurnCompleted { .. } => self.completed = true,
            _ => {}
        }
    }

    fn finish(&mut self, id: Option<&str>, success: bool) {
        let exact = id.and_then(|id| self.rows.iter().position(|r| r.id.as_deref() == Some(id)));
        if id.is_some() && exact.is_none() {
            return;
        }
        // Never attach a duplicate result or an unknown ID to an ID-bearing peer.
        let index = if exact.is_some() {
            exact
        } else {
            if self.evicted_fifo > 0 {
                self.evicted_fifo -= 1;
                return;
            }
            self.rows
                .iter()
                .position(|r| r.id.is_none() && r.result.is_none())
        };
        if let Some(row) = index.and_then(|index| self.rows.get_mut(index))
            && row.result.is_none()
        {
            row.result = Some(success);
            row.elapsed = Some(row.started.elapsed().as_secs());
            self.failed = self.failed.saturating_add(usize::from(!success));
        }
    }

    pub(super) fn render(&self, limit: usize) -> String {
        let mut selected = self
            .rows
            .iter()
            .rev()
            .filter(|r| r.result.is_none())
            .take(limit.min(DISPLAY_LIMIT))
            .collect::<Vec<_>>();
        for row in self.rows.iter().rev().filter(|r| r.result.is_some()) {
            if selected.len() == limit.min(DISPLAY_LIMIT) {
                break;
            }
            selected.push(row);
        }
        // Newest first, so budget pressure removes the oldest complete row.
        selected.sort_by_key(|row| std::cmp::Reverse(row.started));
        let running = if self.completed {
            0
        } else {
            self.rows.iter().filter(|r| r.result.is_none()).count()
        };
        let mut lines = vec![format!(
            "🔧 도구 {} · 실행 {running} · 실패 {}",
            self.total, self.failed
        )];
        for row in &selected {
            let glyph = match row.result {
                Some(true) => "✓",
                Some(false) => "✗",
                None if self.completed => "?",
                None => "▶",
            };
            let elapsed = row
                .elapsed
                .unwrap_or_else(|| row.started.elapsed().as_secs());
            let summary = row
                .summary
                .as_deref()
                .map(|s| format!("  {s}"))
                .unwrap_or_default();
            let time = if row.class == "Bash" || elapsed > 0 {
                format!("  {elapsed}초")
            } else {
                String::new()
            };
            lines.push(format!("{glyph} {}{summary}{time}", row.class));
        }
        if self.total > selected.len() {
            lines.push(format!("… 외 {}개", self.total - selected.len()));
        }
        lines.join("\n")
    }
}

impl super::PlaceholderLiveEvents {
    pub(in crate::services::discord) fn with_tool_activity_display(
        display: ToolActivityDisplay,
    ) -> Self {
        Self {
            tool_activity_display: display,
            ..Self::default()
        }
    }

    pub(in crate::services::discord) fn set_tool_activity_cwd(
        &self,
        channel: ChannelId,
        cwd: Option<&str>,
    ) {
        if let Some(cwd) = cwd {
            self.push_status_event(
                channel,
                StatusEvent::ToolActivityCwd {
                    cwd: cwd.to_owned(),
                },
            );
        }
    }
}

pub(super) fn json_result_observations(
    mut events: Vec<StatusEvent>,
    value: &Value,
) -> Vec<StatusEvent> {
    events.retain(|event| !matches!(event, StatusEvent::ToolActivity { .. }));
    if let Some(blocks) = value.pointer("/message/content").and_then(Value::as_array) {
        for block in blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
        {
            events.push(observation(
                None,
                None,
                block.get("tool_use_id").and_then(Value::as_str),
                Some(
                    !block
                        .get("is_error")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                ),
            ));
        }
    }
    events
}

pub(super) fn fit_panel(mut sections: Vec<String>) -> String {
    sections = sections
        .into_iter()
        .enumerate()
        .filter_map(|(index, section)| {
            if index == 0 {
                Some(section)
            } else {
                super::tool_summary::safe_section(&section)
            }
        })
        .collect();
    let format = |sections: &[String]| {
        sections
            .iter()
            .map(|s| crate::services::terminal_status_formatting::format_subtext_block(s))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    loop {
        let rendered = format(&sections);
        if rendered.len()
            <= super::super::single_message_panel::SINGLE_MESSAGE_PANEL_LIVE_BODY_BUDGET_BYTES
            && super::super::formatting::discord_message_units(&rendered)
                <= super::common::STATUS_PANEL_MAX_CHARS - 64
        {
            return rendered;
        }
        // Drop old tool rows before unrelated context and never split a row.
        let mut lines = sections[0].lines().map(str::to_owned).collect::<Vec<_>>();
        if let Some(index) = lines.iter().rposition(|line| {
            ["▶ ", "✓ ", "✗ ", "? "]
                .iter()
                .any(|prefix| line.starts_with(prefix))
        }) {
            lines.remove(index);
            if let Some(overflow) = lines.iter_mut().find(|line| line.starts_with("… 외 ")) {
                let count = overflow
                    .trim_start_matches("… 외 ")
                    .trim_end_matches('개')
                    .parse::<usize>()
                    .unwrap_or(0);
                *overflow = format!("… 외 {}개", count.saturating_add(1));
            } else {
                lines.insert(index, "… 외 1개".to_owned());
            }
            sections[0] = lines.join("\n");
        } else if sections.len() > 1 {
            sections.pop();
        } else {
            // Non-tool header fields have their own bounds; keep whole lines if a caller exceeds them.
            let mut kept = Vec::new();
            for line in rendered.lines() {
                let candidate = kept
                    .iter()
                    .copied()
                    .chain(std::iter::once(line))
                    .collect::<Vec<_>>()
                    .join("\n");
                if candidate.len() > 1400 || candidate.encode_utf16().count() > 1936 {
                    break;
                }
                kept.push(line);
            }
            return kept.join("\n");
        }
    }
}

#[cfg(test)]
#[path = "tool_activity_tests.rs"]
mod tests;
