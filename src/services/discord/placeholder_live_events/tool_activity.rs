use std::collections::VecDeque;
use std::time::Instant;

use poise::serenity_prelude::ChannelId;
use serde_json::Value;

use super::tool_summary::{class, safe_tool_summary};
use crate::config::ToolActivityDisplay;
use crate::services::agent_protocol::StatusEvent;

const CAPACITY: usize = 64;
const DISPLAY_LIMIT: usize = 6;
/// Record times before 2020-09 are not plausible tool times.
const MIN_SOURCE_UNIX: i64 = 1_600_000_000;
/// Numeric record times at or above this are milliseconds (seconds here would be year 5138).
const MILLIS_FROM: i64 = 100_000_000_000;

/// Codex collaboration calls whose results decide a spawned agent's final state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentCall {
    Spawn,
    Wait,
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    id: Option<String>,
    class: String,
    summary: Option<String>,
    started: Instant,
    started_unix: i64,
    /// Record time of the start, kept apart from the clamped display time for elapsed.
    source_started: Option<i64>,
    result: Option<bool>,
    elapsed: Option<u64>,
    background: bool,
    agent_call: Option<AgentCall>,
    /// Spawn: agent id from the launch ack. Close: the closed target.
    agent: Option<String>,
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
                let source = source_unix.filter(|stamp| *stamp >= MIN_SOURCE_UNIX);
                if let Some(success) = success {
                    self.finish(tool_use_id.as_deref(), *success, false, source);
                    if let Some(facts) = input.as_deref() {
                        self.apply_agent_facts(tool_use_id.as_deref(), *success, facts, source);
                    }
                } else if let Some(name) = name {
                    if let Some(existing) = tool_use_id
                        .as_ref()
                        .and_then(|id| self.rows.iter_mut().find(|r| r.id.as_ref() == Some(id)))
                    {
                        if let Some(source) = source {
                            existing.started_unix =
                                existing.started_unix.min(source.min(observed_unix()));
                            existing.source_started =
                                Some(existing.source_started.map_or(source, |s| s.min(source)));
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
                    let agent_call = name.rsplit('.').next().and_then(|key| {
                        [
                            ("spawn_agent", AgentCall::Spawn),
                            ("wait_agent", AgentCall::Wait),
                            ("close_agent", AgentCall::Close),
                        ]
                        .into_iter()
                        .find_map(|(tool, call)| key.eq_ignore_ascii_case(tool).then_some(call))
                    });
                    let codex_spawn = agent_call == Some(AgentCall::Spawn);
                    let agent = (agent_call == Some(AgentCall::Close))
                        .then(|| input.as_deref().map(super::status_events::tool_input_value))
                        .flatten()
                        .and_then(|value| value.get("target")?.as_str().map(str::to_owned));
                    self.rows.push_back(Row {
                        id: tool_use_id.clone(),
                        class: class(name),
                        summary: input
                            .as_deref()
                            .and_then(|input| safe_tool_summary(name, input, cwd)),
                        started: super::panel_now(),
                        started_unix: source.unwrap_or_else(observed_unix).min(observed_unix()),
                        source_started: source,
                        result: None,
                        elapsed: None,
                        agent_call,
                        agent,
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
                    self.finish(Some(id), *success, true, None);
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
                    self.finish(Some(id), *success, true, None);
                }
            }
            StatusEvent::TurnCompleted { .. } => self.completed = true,
            _ => {}
        }
    }

    fn finish(&mut self, id: Option<&str>, success: bool, terminal: bool, source: Option<i64>) {
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
            if row.agent_call == Some(AgentCall::Spawn) {
                row.launched = true;
            }
            return;
        }
        settle(row, success, source);
        self.failed = self.failed.saturating_add(usize::from(!success));
    }

    /// Applies Codex agent identities and final states carried by a collaboration result:
    /// the spawn ack names the agent, and `wait_agent`/`close_agent` settle its launch row.
    fn apply_agent_facts(
        &mut self,
        id: Option<&str>,
        success: bool,
        facts: &str,
        source: Option<i64>,
    ) {
        let Some(id) = id else { return };
        let Ok(facts) = serde_json::from_str::<Value>(facts) else {
            return;
        };
        let Some(row) = self.rows.iter_mut().find(|r| r.id.as_deref() == Some(id)) else {
            return;
        };
        let outcomes: Vec<(String, bool)> = match row.agent_call {
            Some(AgentCall::Spawn) if row.launched => {
                row.agent = facts["agent_id"].as_str().map(str::to_owned);
                return;
            }
            Some(AgentCall::Wait) => facts["status"]
                .as_object()
                .into_iter()
                .flatten()
                .filter_map(|(agent, status)| {
                    let status = status.as_str()?;
                    (!matches!(status, "pending_init" | "running" | "interrupted"))
                        .then(|| (agent.clone(), agent_status_ok(status)))
                })
                .collect(),
            // A successful close ends the agent whatever state it was in.
            Some(AgentCall::Close) if success => row
                .agent
                .clone()
                .zip(facts["previous_status"].as_str().map(agent_status_ok))
                .into_iter()
                .collect(),
            _ => return,
        };
        for (agent, ok) in outcomes {
            for row in self.rows.iter_mut().filter(|r| {
                r.agent_call == Some(AgentCall::Spawn)
                    && r.result.is_none()
                    && r.agent.as_deref() == Some(agent.as_str())
            }) {
                settle(row, ok, source);
                self.failed = self.failed.saturating_add(usize::from(!ok));
            }
        }
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

/// Codex reports only errored and missing agents as failed; every other final state completed.
fn agent_status_ok(status: &str) -> bool {
    !matches!(status, "errored" | "not_found")
}

/// Elapsed uses the record times when both ends carry one, so replays keep the real duration.
fn settle(row: &mut Row, success: bool, source: Option<i64>) {
    row.result = Some(success);
    row.elapsed = Some(match row.source_started.zip(source) {
        Some((start, end)) => end.saturating_sub(start).max(0) as u64,
        None => super::panel_now()
            .saturating_duration_since(row.started)
            .as_secs(),
    });
}

/// Reads a record timestamp as Unix seconds: RFC 3339 text, or seconds/milliseconds numbers.
/// Values before `MIN_SOURCE_UNIX` are dropped so the observed time is used instead.
pub(super) fn source_unix(stamp: &Value) -> Option<i64> {
    let unix = match stamp.as_i64().or_else(|| stamp.as_f64().map(|n| n as i64)) {
        Some(n) if n >= MILLIS_FROM => n / 1000,
        Some(n) => n,
        None => chrono::DateTime::parse_from_rfc3339(stamp.as_str()?)
            .ok()?
            .timestamp(),
    };
    (unix >= MIN_SOURCE_UNIX).then_some(unix)
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
            let failed = block
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                || code_mode_script_failed(block);
            events.push(observation(
                None,
                agent_facts(block).as_deref(),
                block.get("tool_use_id").and_then(Value::as_str),
                Some(!failed),
            ));
        }
    }
    events
}

/// Code-mode `exec` reports a thrown script only in its first output line.
fn code_mode_script_failed(block: &Value) -> bool {
    block
        .get("content")
        .and_then(Value::as_str)
        .and_then(|content| serde_json::from_str::<Value>(content).ok())
        .is_some_and(|output| {
            output.pointer("/0/type").and_then(Value::as_str) == Some("input_text")
                && output
                    .pointer("/0/text")
                    .and_then(Value::as_str)
                    .and_then(|text| text.lines().next())
                    == Some("Script failed")
        })
}

/// Reduces a Codex collaboration result to agent ids and status names; agent messages are dropped.
fn agent_facts(block: &Value) -> Option<String> {
    let output = serde_json::from_str::<Value>(block.get("content")?.as_str()?).ok()?;
    let status_name = |status: &Value| {
        status
            .as_str()
            .map(str::to_owned)
            .or_else(|| status.as_object()?.keys().next().cloned())
    };
    let mut facts = serde_json::Map::new();
    if let Some(agent) = output.get("agent_id").filter(|id| id.is_string()) {
        facts.insert("agent_id".into(), agent.clone());
    }
    if let Some(statuses) = output.get("status").and_then(Value::as_object) {
        let names = statuses
            .iter()
            .filter_map(|(agent, status)| {
                Some((agent.clone(), Value::String(status_name(status)?)))
            })
            .collect();
        facts.insert("status".into(), Value::Object(names));
    }
    if let Some(status) = output.get("previous_status").and_then(status_name) {
        facts.insert("previous_status".into(), Value::String(status));
    }
    (!facts.is_empty()).then(|| Value::Object(facts).to_string())
}

pub(super) fn fit_panel(sections: Vec<String>) -> String {
    let sections = sections
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
    fit_sections(sections)
}

/// Fits panel sections to the live footer body by dropping whole tool rows, then shortening
/// the longest section, so every section title stays visible.
pub(super) fn fit_sections(mut sections: Vec<String>) -> String {
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
