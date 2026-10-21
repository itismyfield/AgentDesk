//! Transcript-only turn summaries; durable opening marks distinguish replay from backfill.

use std::collections::BTreeSet;
use std::hash::{DefaultHasher, Hash, Hasher};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::pieces::{Derived, PieceWork};
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};
use crate::services::turn_summary_line::{Outcome, render};

pub const TURN_SUMMARY_OPEN: &str = "turn_summary_open";

pub struct SummaryInputs {
    pub turn_marks: BTreeSet<String>,
    pub disposed: BTreeSet<u64>,
    pub context_window: Option<u64>,
}

pub(crate) fn unit_hash(key: &UnitKey) -> u64 {
    let mut hash = DefaultHasher::new();
    key.hash(&mut hash);
    hash.finish()
}

pub(super) struct TurnSummaryRun {
    inputs: SummaryInputs,
    provider: ShadowProvider,
    open: Option<Turn>,
}

struct Turn {
    id: Option<String>,
    started: Option<DateTime<Utc>>,
    first: Option<UnitKey>,
    eligible: bool,
    used: Option<u128>,
    failed: bool,
}

impl TurnSummaryRun {
    pub(super) fn new(provider: ShadowProvider, inputs: SummaryInputs) -> Self {
        Self {
            inputs,
            provider,
            open: None,
        }
    }

    pub(super) fn has_pending_projection_probe(&self) -> bool {
        #[cfg(test)]
        if crate::services::claude_tui::source_verify::n2b_mutant("b1b-summary-open") {
            return self.open.is_some();
        }
        self.open
            .as_ref()
            .is_some_and(|turn| turn.first.is_some() && turn.eligible)
    }

    fn open(&mut self, value: &Value, id: Option<String>) {
        self.open = Some(Turn {
            id,
            started: timestamp(value),
            first: None,
            eligible: false,
            used: None,
            failed: false,
        });
    }

    pub(super) fn prompt(&mut self, value: &Value) {
        if self.provider == ShadowProvider::Claude
            && self.open.as_ref().is_none_or(|turn| turn.first.is_none())
        {
            self.open(value, None);
        }
    }

    pub(super) fn start(&mut self, value: &Value, id: Option<String>) {
        if self.provider == ShadowProvider::Codex
            && (self.open.is_none() || self.open.as_ref().is_some_and(|turn| turn.id != id))
        {
            self.open(value, id);
        }
    }

    pub(super) fn assistant(&mut self, value: &Value) {
        if self.provider != ShadowProvider::Claude {
            return;
        }
        if self.open.is_none() {
            if value.pointer("/message/model").and_then(Value::as_str) == Some("<synthetic>")
                && value.get("isApiErrorMessage") != Some(&Value::Bool(true))
            {
                return;
            }
            self.open(value, None);
        }
        let Some(turn) = self.open.as_mut() else {
            return;
        };
        turn.failed = value.get("isApiErrorMessage") == Some(&Value::Bool(true));
        if let Some(usage) = value
            .pointer("/message/usage")
            .filter(|usage| usage.is_object())
        {
            turn.used = Some(
                [
                    "input_tokens",
                    "cache_creation_input_tokens",
                    "cache_read_input_tokens",
                ]
                .iter()
                .map(|field| u128::from(usage.get(*field).and_then(Value::as_u64).unwrap_or(0)))
                .sum(),
            );
        }
    }

    pub(super) fn observe(&mut self, value: &Value) {
        if self.provider == ShadowProvider::Codex
            && let Some(turn) = &mut self.open
        {
            if value.pointer("/payload/type").and_then(Value::as_str) == Some("error") {
                turn.failed = true;
            }
        }
    }

    /// Called only after First sealing, before the native unit's durable disposition.
    pub(super) fn first(&mut self, key: &UnitKey) -> Vec<Derived> {
        if !matches!(key.kind, UnitKind::Body | UnitKind::Tool) {
            return Vec::new();
        }
        let Some(turn) = &mut self.open else {
            return Vec::new();
        };
        if turn.first.is_some() {
            return Vec::new();
        }
        let mut mark = key.clone();
        mark.kind = UnitKind::Tool;
        mark.native_key.push_str("#turn");
        turn.eligible = !self.inputs.disposed.contains(&unit_hash(key))
            || self.inputs.turn_marks.contains(&mark.native_key);
        turn.first = Some(key.clone());
        if !turn.eligible {
            return Vec::new();
        }
        vec![Derived::Excluded {
            unit_key: mark,
            reason: TURN_SUMMARY_OPEN.into(),
        }]
    }

    pub(super) fn close(&mut self, value: &Value, id: Option<&str>) -> Vec<Derived> {
        if self.provider == ShadowProvider::Codex
            && self.open.as_ref().is_some_and(|turn| {
                turn.id
                    .as_deref()
                    .zip(id)
                    .is_some_and(|(ours, closing)| ours != closing)
            })
        {
            return Vec::new();
        }
        let Some(turn) = self.open.take() else {
            return Vec::new();
        };
        let Some(mut key) = turn.first.filter(|_| turn.eligible) else {
            return Vec::new();
        };
        let cancelled = match self.provider {
            ShadowProvider::Claude => value.get("type").and_then(Value::as_str) == Some("user"),
            ShadowProvider::Codex => {
                value.pointer("/payload/type").and_then(Value::as_str) == Some("turn_aborted")
            }
        };
        let failed = turn.failed
            || value.get("is_error") == Some(&Value::Bool(true))
            || value
                .pointer("/payload/error")
                .is_some_and(|error| !error.is_null());
        let outcome = if cancelled {
            Outcome::Cancelled
        } else if failed {
            Outcome::Failed
        } else {
            Outcome::Completed
        };
        let elapsed = turn.started.zip(timestamp(value)).and_then(|(start, end)| {
            if end < start {
                return None;
            }
            u64::try_from(end.signed_duration_since(start).num_milliseconds()).ok()
        });
        let percent = turn
            .used
            .zip(self.inputs.context_window.filter(|window| *window > 0))
            .map(|(used, window)| {
                ((used.min(u128::from(window)) * 100) / u128::from(window)) as u8
            });
        key.kind = UnitKind::Tool;
        key.native_key.push_str("#turn_end");
        vec![Derived::Piece(PieceWork {
            unit_key: key,
            index: 0,
            payload: render(outcome, elapsed, percent),
        })]
    }
}

fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value.get("timestamp")?.as_str()?.parse().ok()
}

#[cfg(test)]
#[path = "turn_summary_boundary_tests.rs"]
mod turn_summary_boundary_tests;
