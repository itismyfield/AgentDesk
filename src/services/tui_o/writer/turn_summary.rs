//! Transcript-only turn summaries; durable opening marks distinguish replay from backfill.

use std::collections::{BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde_json::Value;

use super::pieces::{Derived, PieceWork};
use crate::services::codex_tui::rollout_index::summary_context;
use crate::services::tui_o::shadow::{ShadowProvider, UnitKey, UnitKind};
use crate::services::turn_summary_line::{Footer, Model, Outcome, quota, render};

pub const TURN_SUMMARY_OPEN: &str = "turn_summary_open";
pub(super) const TURN_SUMMARY_SUFFIX: &str = "#turn_end";

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
    /// Claude turns are per transcript, so another transcript's unclosed turn cannot absorb this one.
    source: Option<PathBuf>,
    parked: HashMap<PathBuf, Turn>,
    current_source: Option<PathBuf>,
    pending_codex_model: Option<(String, summary_context::CodexModel)>,
}

struct Turn {
    source: Option<PathBuf>,
    id: Option<String>,
    session_id: Option<String>,
    started: Option<DateTime<Utc>>,
    first: Option<UnitKey>,
    eligible: bool,
    used: Option<u128>,
    context_window: Option<u64>,
    codex_model: Option<summary_context::CodexModel>,
    failed: bool,
}

impl TurnSummaryRun {
    pub(super) fn new(provider: ShadowProvider, inputs: SummaryInputs) -> Self {
        Self {
            inputs,
            provider,
            open: None,
            source: None,
            parked: HashMap::new(),
            current_source: None,
            pending_codex_model: None,
        }
    }

    /// Records that follow come from the transcript at `path` until the next call.
    pub(super) fn enter(&mut self, path: &Path) {
        if self.current_source.as_deref() != Some(path) {
            self.pending_codex_model = None;
        }
        self.current_source = Some(path.to_owned());
        if self.provider != ShadowProvider::Claude || self.source.as_deref() == Some(path) {
            return;
        }
        // The first named transcript keeps what was derived before any transcript was named.
        if let Some(previous) = self.source.replace(path.to_owned()) {
            let resumed = self.parked.remove(path);
            if let Some(turn) = std::mem::replace(&mut self.open, resumed) {
                self.parked.insert(previous, turn);
            }
        }
    }

    pub(super) fn has_pending_projection_probe(&self) -> bool {
        self.open
            .as_ref()
            .is_some_and(|turn| turn.first.is_some() && turn.eligible)
    }

    fn open(&mut self, value: &Value, id: Option<String>) {
        let codex_model = self
            .pending_codex_model
            .take()
            .filter(|(candidate, _)| id.as_ref() == Some(candidate))
            .map(|(_, model)| model);
        self.open = Some(Turn {
            source: self.current_source.clone(),
            id,
            session_id: value
                .get("sessionId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            started: timestamp(value),
            first: None,
            eligible: false,
            used: None,
            context_window: value
                .pointer("/payload/model_context_window")
                .and_then(Value::as_u64),
            codex_model,
            failed: false,
        });
    }

    pub(super) fn physical_end(&mut self, path: &Path) {
        if self
            .open
            .as_ref()
            .is_some_and(|turn| turn.source.as_deref() == Some(path))
        {
            self.open = None;
        }
        self.parked.remove(path);
        if self.current_source.as_deref() == Some(path) {
            self.pending_codex_model = None;
        }
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
        if let Some(session) = value.get("sessionId").and_then(Value::as_str) {
            turn.session_id = Some(session.to_owned());
        }
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
        if self.provider != ShadowProvider::Codex {
            return;
        }
        if let Some((id, model, effort)) = summary_context::context(value)
            && (model.is_some() || effort.is_some())
        {
            let model = summary_context::CodexModel {
                model: model.map(str::to_owned),
                effort: effort.map(str::to_owned),
            };
            if let Some(turn) = &mut self.open
                && turn.id.as_deref() == Some(id)
                && turn.source == self.current_source
            {
                if let Some(existing) = &mut turn.codex_model {
                    existing.model = model.model.or(existing.model.take());
                    existing.effort = model.effort.or(existing.effort.take());
                } else {
                    turn.codex_model = Some(model);
                }
            } else {
                self.pending_codex_model = Some((id.to_owned(), model));
            }
        }
        let Some(turn) = &mut self.open else {
            return;
        };
        match value.pointer("/payload/type").and_then(Value::as_str) {
            Some("error") => turn.failed = true,
            Some("token_count") if turn.source == self.current_source => {
                let payload = &value["payload"];
                if payload["turn_id"]
                    .as_str()
                    .is_some_and(|id| turn.id.as_deref() != Some(id))
                {
                    return;
                }
                let info = &payload["info"];
                if let Some(input) = info["last_token_usage"]["input_tokens"].as_u64() {
                    turn.used = Some(u128::from(input));
                }
                if let Some(window) = info["model_context_window"]
                    .as_u64()
                    .filter(|window| *window > 0)
                {
                    turn.context_window = Some(window);
                }
            }
            _ => {}
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

    pub(super) fn close(
        &mut self,
        value: &Value,
        id: Option<&str>,
        logical_session: impl Fn() -> Option<String>,
    ) -> Vec<Derived> {
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
        let ended = timestamp(value).filter(|end| turn.started.is_none_or(|start| start <= *end));
        let elapsed = turn.started.zip(ended).and_then(|(start, end)| {
            u64::try_from(end.signed_duration_since(start).num_milliseconds()).ok()
        });
        let percent = turn
            .used
            .zip(
                turn.context_window
                    .or(self.inputs.context_window)
                    .filter(|window| *window > 0),
            )
            .map(|(used, window)| {
                ((used.min(u128::from(window)) * 100) / u128::from(window)) as u8
            });
        key.kind = UnitKind::Tool;
        key.native_key.push_str(TURN_SUMMARY_SUFFIX);
        let provider = match self.provider {
            ShadowProvider::Claude => "claude",
            ShadowProvider::Codex => "codex",
        };
        let session_id = turn
            .session_id
            .as_deref()
            .or_else(|| value.get("sessionId").and_then(Value::as_str));
        let logical_session = (self.provider == ShadowProvider::Claude
            && outcome == Outcome::Completed
            && session_id.is_some())
        .then(logical_session)
        .flatten();
        let snapshot = quota::snapshot(
            provider,
            key.channel_id,
            turn.started.map(quota::Selected::Before),
            session_id,
            logical_session.as_deref(),
            outcome == Outcome::Completed,
        );
        let model = if let Some(codex) = &turn.codex_model {
            Model {
                name: codex.model.as_deref(),
                effort: codex.effort.as_deref(),
                fast: false,
            }
        } else {
            snapshot.model_between(turn.started.zip(ended))
        };
        vec![Derived::Piece(PieceWork {
            output_origin: None,
            unit_key: key,
            index: 0,
            payload: render(
                outcome,
                Footer {
                    elapsed_ms: elapsed,
                    started: turn.started,
                    ended,
                    model,
                    context_pct: percent,
                    quota: snapshot.quota,
                },
            ),
            notice_origin: None,
        })]
    }
}

fn timestamp(value: &Value) -> Option<DateTime<Utc>> {
    value.get("timestamp")?.as_str()?.parse().ok()
}

#[cfg(test)]
#[path = "turn_summary_boundary_tests.rs"]
mod turn_summary_boundary_tests;
