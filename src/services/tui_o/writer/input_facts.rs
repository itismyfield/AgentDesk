//! Dormant input adapter: replay O's parent transcript facts without storing turn state.

use chrono::Utc;
use serde_json::Value;

use crate::services::codex_tui::rollout_index::strict_parent_session;
use crate::services::tui_o::shadow::capture::SourceCapture;
use crate::services::tui_o::shadow::identity::{RecordFact, classify, row_key};
use crate::services::tui_o::shadow::seal::{TurnEvent, TurnTracker};
use crate::services::tui_o::shadow::{
    CaptureOutcome, CaptureSource, ShadowProvider, SourceBinding,
};

pub mod reactions;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TurnState {
    Unknown,
    Open { native_turn_id: Option<String> },
    Idle,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChannelFact {
    pub binding: SourceBinding,
    pub through: u64,
    pub state: TurnState,
}

/// One fact of the last poll in record order; nothing here is stored.
#[derive(Clone, Debug, PartialEq)]
pub enum Ordered {
    Opened {
        range: (u64, u64),
        native_turn_id: Option<String>,
    },
    /// `aborted` marks an interrupt, which ends the turn without completing it.
    Closed {
        range: (u64, u64),
        native_turn_id: Option<String>,
        aborted: bool,
    },
    /// A record whose text may hold an input frame, after its own turn facts.
    Input {
        range: (u64, u64),
        record_key: Option<String>,
        record: Value,
    },
}

pub struct InputFacts {
    binding: SourceBinding,
    capture: SourceCapture,
    tracker: TurnTracker,
    state: TurnState,
    halted: Option<String>,
    events: Vec<Ordered>,
}

impl InputFacts {
    /// A restart reconstructs from the same parent source, never from a saved turn cursor.
    pub fn open(binding: SourceBinding) -> Result<Self, String> {
        Self::open_at(binding, 0)
    }

    /// Reads from a complete-line `offset`; a turn open there stays Unknown until it closes.
    pub fn open_at(binding: SourceBinding, offset: u64) -> Result<Self, String> {
        let child_path = binding
            .source
            .path
            .components()
            .any(|part| part.as_os_str() == "subagents");
        let child = match binding.provider {
            ShadowProvider::Claude => child_path,
            ShadowProvider::Codex => {
                strict_parent_session(&binding.source.path, &binding.source.session_id)
                    .map_err(|e| e.to_string())?;
                false
            }
        };
        if child {
            return Err("input facts require a parent transcript".into());
        }
        let capture =
            SourceCapture::open(binding.source.clone(), offset).map_err(|e| e.to_string())?;
        Ok(Self {
            binding,
            capture,
            tracker: TurnTracker::starting_at(offset),
            state: TurnState::Unknown,
            halted: None,
            events: Vec::new(),
        })
    }

    /// The last poll's ordered facts.
    pub fn events(&self) -> &[Ordered] {
        &self.events
    }

    /// Whether the captured prefix reaches the file's current end.
    pub fn caught_up(&self) -> Result<bool, String> {
        let len = self.capture.file_len().map_err(|e| e.to_string())?;
        Ok(self.capture.captured_through() == len)
    }

    /// Only a fully captured prefix at the file's current end can supply Idle.
    pub fn poll(&mut self, max_bytes: u64) -> Result<ChannelFact, String> {
        if let Some(error) = &self.halted {
            return Err(error.clone());
        }
        match self.poll_inner(max_bytes) {
            Ok(fact) => Ok(fact),
            Err(error) => {
                self.state = TurnState::Unknown;
                self.halted = Some(error.clone());
                Err(error)
            }
        }
    }

    fn poll_inner(&mut self, max_bytes: u64) -> Result<ChannelFact, String> {
        self.events.clear();
        if self.binding.provider == ShadowProvider::Codex {
            strict_parent_session(&self.binding.source.path, &self.binding.source.session_id)
                .map_err(|e| e.to_string())?;
        }
        let batch = match self.capture.poll(max_bytes) {
            CaptureOutcome::Batch(batch) => batch,
            CaptureOutcome::Anomaly(anomaly) => return Err(anomaly.detail),
        };
        for record in batch.records {
            if record.line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value: Value = serde_json::from_slice(&record.line).map_err(|e| e.to_string())?;
            if value.get("isSidechain") == Some(&Value::Bool(true)) {
                return Err("sidechain record in parent transcript".into());
            }
            let row = row_key(&value);
            let range = (record.start, record.end);
            for fact in classify(self.binding.provider, &value) {
                // A foreign closer cannot release this channel's running parent turn.
                if let (
                    RecordFact::Idle(Some(close)),
                    TurnState::Open {
                        native_turn_id: Some(open),
                    },
                ) = (&fact, &self.state)
                    && open != close
                {
                    continue;
                }
                let event = self.tracker.observe(&fact, row.as_ref(), range, Utc::now());
                let (next, closed) = match event {
                    TurnEvent::Opened(id) => (
                        Some(TurnState::Open {
                            native_turn_id: Some(id),
                        }),
                        None,
                    ),
                    TurnEvent::Closed(turn) => (Some(TurnState::Idle), turn.native_turn_id),
                    TurnEvent::StrayIdle | TurnEvent::EdgeTurn => (Some(TurnState::Idle), None),
                    TurnEvent::None => {
                        if let RecordFact::Blocked(reason) = &fact {
                            return Err(reason.clone());
                        }
                        let anonymous_open = matches!(fact, RecordFact::TurnStart(_))
                            || (self.state == TurnState::Idle
                                && matches!(fact, RecordFact::Assistant))
                            || (row.is_none() && matches!(fact, RecordFact::Prompt(true, _)));
                        let next = if anonymous_open {
                            Some(TurnState::Open {
                                native_turn_id: None,
                            })
                        } else {
                            matches!(fact, RecordFact::Idle(_)).then_some(TurnState::Idle)
                        };
                        (next, None)
                    }
                };
                if let Some(next) = next {
                    self.mark(next, range, closed, &value);
                }
            }
            if record.line.windows(9).any(|bytes| bytes == b"[adk:tok=") {
                self.events.push(Ordered::Input {
                    range,
                    record_key: row,
                    record: value,
                });
            }
        }
        let through = self.capture.captured_through();
        let complete = through == self.capture.file_len().map_err(|e| e.to_string())?;
        let state = if !complete && self.state == TurnState::Idle {
            TurnState::Unknown
        } else {
            self.state.clone()
        };
        Ok(ChannelFact {
            binding: self.binding.clone(),
            through,
            state,
        })
    }

    fn mark(&mut self, next: TurnState, range: (u64, u64), closed: Option<String>, value: &Value) {
        match &next {
            TurnState::Open { native_turn_id } => self.events.push(Ordered::Opened {
                range,
                native_turn_id: native_turn_id.clone(),
            }),
            _ if self.state != TurnState::Idle => self.events.push(Ordered::Closed {
                range,
                native_turn_id: closed,
                aborted: aborted(self.binding.provider, value),
            }),
            _ => {}
        }
        self.state = next;
    }
}

// Claude yields Idle from a user row only for its interrupt marker; Codex names the abort.
fn aborted(provider: ShadowProvider, value: &Value) -> bool {
    match provider {
        ShadowProvider::Claude => value["type"] == "user",
        ShadowProvider::Codex => {
            value["type"] == "event_msg" && value["payload"]["type"] == "turn_aborted"
        }
    }
}

#[cfg(test)]
mod tests;
