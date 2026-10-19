//! Bounded tool runs. Only native record boundaries close a run, so replay needs no toggle log.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::Value;

use super::pieces::{Derived, PieceWork, TOOL_CALL_CARD, TOOL_CALL_PANEL};
use crate::services::discord::formatting::{safe_tool_summary, tool_class};
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey};

pub struct ToolCardInputs {
    pub known: BTreeSet<UnitKey>,
    pub recorded_cards: BTreeSet<UnitKey>,
    pub switch: Arc<dyn Fn() -> bool + Send + Sync>,
}

pub(super) struct ToolCardRun {
    inputs: ToolCardInputs,
    rows: Vec<Row>,
    cwd: Option<String>,
}

pub(super) struct ToolMeta {
    id: String,
    label: &'static str,
    summary: Option<String>,
}

struct Row {
    key: UnitKey,
    meta: ToolMeta,
    status: char,
}

impl ToolCardRun {
    pub(super) fn new(inputs: ToolCardInputs) -> Self {
        Self {
            inputs,
            rows: Vec::new(),
            cwd: None,
        }
    }

    pub(super) fn observe(&mut self, value: &Value) {
        let cwd = value
            .get("cwd")
            .or_else(|| value.get("payload")?.get("cwd"));
        if let Some(cwd) = cwd.and_then(Value::as_str) {
            self.cwd = Some(cwd.into());
        }
    }

    pub(super) fn tools(&self, provider: ShadowProvider, value: &Value) -> Vec<ToolMeta> {
        let blocks: Vec<&Value> = match provider {
            ShadowProvider::Claude => value
                .pointer("/message/content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|block| block["type"] == "tool_use")
                .collect(),
            ShadowProvider::Codex => value.get("payload").into_iter().collect(),
        };
        blocks
            .into_iter()
            .map(|block| {
                let name = block["name"].as_str().unwrap_or("");
                let input = ["arguments", "input", "action"]
                    .iter()
                    .find_map(|field| block.get(*field));
                let input = match input {
                    Some(Value::String(text)) => text.clone(),
                    Some(value) => value.to_string(),
                    None => String::new(),
                };
                ToolMeta {
                    id: block
                        .get("call_id")
                        .or_else(|| block.get("id"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    label: tool_class(name),
                    summary: safe_tool_summary(name, &input, self.cwd.as_deref()),
                }
            })
            .collect()
    }

    pub(super) fn tool(&mut self, key: UnitKey, meta: ToolMeta) -> Vec<Derived> {
        let mut out = Vec::new();
        let known = self.inputs.known.contains(&key);
        let row = Row {
            key: key.clone(),
            meta,
            status: '?',
        };
        if self.rows.len() == 14 || units(&render(self.rows.iter().chain([&row]), true)) > 1800 {
            out.extend(self.close());
        }
        // Recorded Card and NonCard dispositions precede today's switch, including global off.
        let card = !known
            && (self.inputs.recorded_cards.contains(&key)
                || !self.rows.is_empty()
                || (self.inputs.switch)());
        if !card {
            out.extend(self.close());
            self.inputs.known.insert(key.clone());
            out.push(Derived::Excluded {
                unit_key: key,
                reason: TOOL_CALL_PANEL.into(),
            });
            return out;
        }
        self.rows.push(row);
        out.push(Derived::Excluded {
            unit_key: key,
            reason: TOOL_CALL_CARD.into(),
        });
        out
    }

    pub(super) fn result(&mut self, id: &str, failed: bool) -> Vec<Derived> {
        if let Some(row) = self.rows.iter_mut().find(|row| row.meta.id == id) {
            row.status = if failed { '✗' } else { '✓' };
        }
        if self.rows.len() >= 8 && self.rows.iter().all(|row| row.status != '?') {
            self.close()
        } else {
            Vec::new()
        }
    }

    pub(super) fn close(&mut self) -> Vec<Derived> {
        let Some(last) = self.rows.last() else {
            return Vec::new();
        };
        let mut unit_key = last.key.clone();
        unit_key.native_key.push_str("#card");
        let mut payload = render(self.rows.iter(), true);
        if units(&payload) > 1800 {
            payload = render(self.rows.iter(), false);
        }
        if units(&payload) > 1800 {
            payload = format!("-# ▸ 도구 {}개", self.rows.len());
        }
        self.rows.clear();
        vec![Derived::Piece(PieceWork {
            unit_key,
            index: 0,
            payload,
        })]
    }
}

fn units(text: &str) -> usize {
    text.encode_utf16().count()
}

fn render<'a>(rows: impl Iterator<Item = &'a Row>, summaries: bool) -> String {
    let rows: Vec<_> = rows.collect();
    let mut lines = Vec::new();
    let mut at = 0;
    while let Some(row) = rows.get(at) {
        let mut count = 1;
        if row.status == '✓' {
            while rows
                .get(at + count)
                .is_some_and(|next| next.status == '✓' && next.meta.label == row.meta.label)
            {
                count += 1;
            }
        }
        let count_label = if count > 1 {
            format!(" ×{count}")
        } else {
            String::new()
        };
        let summary = summaries
            .then_some(row.meta.summary.as_deref())
            .flatten()
            .filter(|text| !text.chars().any(char::is_control) && !text.contains("://"))
            .map(|text| format!("  {text}"))
            .unwrap_or_default();
        lines.push(format!(
            "-# ▸ {}{count_label}{summary}  {}",
            row.meta.label, row.status
        ));
        at += count;
    }
    lines.join("\n")
}

#[cfg(test)]
#[path = "tool_card_known_tests.rs"]
mod known_tests;
