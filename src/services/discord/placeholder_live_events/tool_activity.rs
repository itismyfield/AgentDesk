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
    class: String,
    summary: Option<String>,
    started: Instant,
    started_unix: i64,
    result: Option<bool>,
    elapsed: Option<u64>,
    background: bool,
    codex_spawn: bool,
    launched: bool,
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
        source_unix: None,
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
                source_unix,
            } => {
                if let Some(success) = success {
                    self.finish(tool_use_id.as_deref(), *success, false);
                } else if let Some(name) = name {
                    if let Some(existing) = tool_use_id
                        .as_ref()
                        .and_then(|id| self.rows.iter_mut().find(|r| r.id.as_ref() == Some(id)))
                    {
                        if let Some(source) = source_unix.filter(|stamp| *stamp > 0) {
                            existing.started_unix =
                                existing.started_unix.min(source.min(observed_unix()));
                        }
                        if existing.summary.is_none() {
                            existing.summary = input
                                .as_deref()
                                .and_then(|input| safe_tool_summary(name, input, cwd));
                        }
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
                    let codex_spawn = name
                        .rsplit('.')
                        .next()
                        .is_some_and(|key| key.eq_ignore_ascii_case("spawn_agent"));
                    self.rows.push_back(Row {
                        id: tool_use_id.clone(),
                        class: class(name),
                        summary: input
                            .as_deref()
                            .and_then(|input| safe_tool_summary(name, input, cwd)),
                        started: super::panel_now(),
                        started_unix: source_unix
                            .filter(|stamp| *stamp > 0)
                            .unwrap_or_else(observed_unix)
                            .min(observed_unix()),
                        result: None,
                        elapsed: None,
                        codex_spawn,
                        launched: false,
                        background: codex_spawn
                            || matches!(class(name).as_str(), "Task" | "Bash")
                                && input
                                    .as_deref()
                                    .map(super::status_events::tool_input_value)
                                    .and_then(|value| {
                                        value.get("run_in_background").and_then(Value::as_bool)
                                    })
                                    .unwrap_or(false),
                    });
                }
            }
            StatusEvent::SubagentStart {
                tool_use_id: Some(id),
                background: true,
                ..
            } => {
                if let Some(row) = self
                    .rows
                    .iter_mut()
                    .find(|r| r.class == "Task" && r.id.as_ref() == Some(id))
                {
                    row.background = true;
                }
            }
            StatusEvent::SubagentEnd {
                tool_use_id: Some(id),
                success,
                ack_only: false,
                ..
            } => {
                if self
                    .rows
                    .iter()
                    .any(|r| r.class == "Task" && r.id.as_ref() == Some(id))
                {
                    self.finish(Some(id), *success, true);
                }
            }
            StatusEvent::BackgroundTaskStart {
                tool_use_id: id, ..
            } => {
                if let Some(row) = self
                    .rows
                    .iter_mut()
                    .find(|r| r.class == "Bash" && r.id.as_ref() == Some(id))
                {
                    row.background = true;
                }
            }
            StatusEvent::BackgroundTaskEnd {
                tool_use_id: id,
                success,
            } => {
                if self
                    .rows
                    .iter()
                    .any(|r| r.class == "Bash" && r.id.as_ref() == Some(id))
                {
                    self.finish(Some(id), *success, true);
                }
            }
            StatusEvent::TurnCompleted { .. } => self.completed = true,
            _ => {}
        }
    }

    fn finish(&mut self, id: Option<&str>, success: bool, terminal: bool) {
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
        let Some(index) = index else {
            return;
        };
        let row = &mut self.rows[index];
        if row.result.is_some() {
            return;
        }
        if row.background && success && !terminal {
            if row.codex_spawn {
                row.launched = true;
            }
            return;
        }
        row.result = Some(success);
        row.elapsed = Some(
            super::panel_now()
                .saturating_duration_since(row.started)
                .as_secs(),
        );
        self.failed = self.failed.saturating_add(usize::from(!success));
    }

    pub(super) fn render(&self, limit: usize) -> String {
        let mut selected = self
            .rows
            .iter()
            .rev()
            .filter(|r| r.result.is_none() && !r.launched)
            .take(limit.min(DISPLAY_LIMIT))
            .collect::<Vec<_>>();
        for row in self
            .rows
            .iter()
            .rev()
            .filter(|r| r.result.is_some() || r.launched)
        {
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
            self.rows
                .iter()
                .filter(|r| r.result.is_none() && !r.launched)
                .count()
        };
        let mut lines = vec![format!(
            "🔧 도구 {} · 실행 {running} · 실패 {}",
            self.total, self.failed
        )];
        for row in &selected {
            let glyph = match row.result {
                Some(true) => "✓",
                Some(false) => "✗",
                None if row.launched => "↗",
                None if self.completed => "?",
                None => "▶",
            };
            let summary = row
                .summary
                .as_deref()
                .map(|s| format!("  {s}"))
                .unwrap_or_default();
            // Discord refreshes running ages without changing heartbeat render text.
            let time = match row.elapsed {
                None if row.launched => "  시작됨".to_owned(),
                Some(elapsed) if row.class == "Bash" || elapsed > 0 => format!("  {elapsed}초"),
                None => format!("  시작 <t:{}:R>", row.started_unix),
                _ => String::new(),
            };
            lines.push(format!(
                "{glyph} {}{summary}{time}",
                row.class.replace('_', "\\_")
            ));
        }
        if self.total > selected.len() {
            lines.push(format!("… 외 {}개", self.total - selected.len()));
        }
        lines.join("\n")
    }
}

fn observed_unix() -> i64 {
    let now = chrono::Utc::now().timestamp();
    #[cfg(test)]
    let now = now.saturating_add(super::test_clock::offset().as_secs() as i64);
    now
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
                super::tool_summary::safe_section(&section).or_else(|| {
                    let title = section.lines().next()?;
                    ["Plan", "Tasks", "Subagents", "Workflow"]
                        .contains(&title)
                        .then(|| format!("{title}\n… [가림]"))
                })
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
        // Reduce oversized activity lists by removing complete oldest rows.
        let mut lines = sections[0].lines().map(str::to_owned).collect::<Vec<_>>();
        if let Some(index) = lines.iter().rposition(|line| {
            ["▶ ", "↗ ", "✓ ", "✗ ", "? "]
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
        } else if let Some(index) = (1..sections.len())
            .filter(|index| {
                let lines: Vec<_> = sections[*index].lines().collect();
                lines.len() > 2 || lines.len() == 2 && lines[1] != "…"
            })
            .max_by_key(|index| sections[*index].len())
        {
            // Retain every section title while reducing its longest content list.
            let mut lines: Vec<_> = sections[index].lines().collect();
            if lines.last() == Some(&"…") {
                lines.pop();
            }
            lines.pop();
            lines.push("…");
            sections[index] = lines.join("\n");
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

#[cfg(test)]
#[path = "tool_activity_accuracy_tests.rs"]
mod accuracy_tests;
