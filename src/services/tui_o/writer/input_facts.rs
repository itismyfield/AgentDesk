//! Dormant input adapter: replay O's parent transcript facts without storing turn state.

use chrono::Utc;
use serde_json::Value;

use crate::services::codex_tui::rollout_index::rollout_is_subagent;
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

pub struct InputFacts {
    binding: SourceBinding,
    capture: SourceCapture,
    tracker: TurnTracker,
    state: TurnState,
    halted: Option<String>,
}

impl InputFacts {
    /// A restart reconstructs from the same parent source, never from a saved turn cursor.
    pub fn open(binding: SourceBinding) -> Result<Self, String> {
        let child_path = binding
            .source
            .path
            .components()
            .any(|part| part.as_os_str() == "subagents");
        let child = match binding.provider {
            ShadowProvider::Claude => child_path,
            ShadowProvider::Codex => rollout_is_subagent(&binding.source.path),
        };
        if child {
            return Err("input facts require a parent transcript".into());
        }
        let capture = SourceCapture::open(binding.source.clone(), 0).map_err(|e| e.to_string())?;
        Ok(Self {
            binding,
            capture,
            tracker: TurnTracker::starting_at(0),
            state: TurnState::Unknown,
            halted: None,
        })
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
                let event = self.tracker.observe(
                    &fact,
                    row.as_ref(),
                    (record.start, record.end),
                    Utc::now(),
                );
                match event {
                    TurnEvent::Opened(id) => {
                        self.state = TurnState::Open {
                            native_turn_id: Some(id),
                        }
                    }
                    TurnEvent::Closed(_) | TurnEvent::StrayIdle | TurnEvent::EdgeTurn => {
                        self.state = TurnState::Idle
                    }
                    TurnEvent::None => {
                        if let RecordFact::Blocked(reason) = &fact {
                            return Err(reason.clone());
                        }
                        let anonymous_open = matches!(fact, RecordFact::TurnStart(_))
                            || (self.state == TurnState::Idle
                                && matches!(fact, RecordFact::Assistant))
                            || (row.is_none() && matches!(fact, RecordFact::Prompt(true, _)));
                        if anonymous_open {
                            self.state = TurnState::Open {
                                native_turn_id: None,
                            };
                        } else if matches!(fact, RecordFact::Idle(_)) {
                            self.state = TurnState::Idle;
                        }
                    }
                }
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
}

#[cfg(test)]
mod tests;
