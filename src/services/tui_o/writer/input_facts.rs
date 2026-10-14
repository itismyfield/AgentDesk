//! Dormant input adapter: replay O's parent transcript facts without storing turn state.

use chrono::Utc;
use serde_json::Value;

use crate::services::codex_tui::rollout_index::strict_parent_session;
use crate::services::tui_o::n1_observation as observation;
use crate::services::tui_o::shadow::capture::{SourceCapture, observation::ObservationCapture};
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

/// Where a reader stopped by a typed `Blocked` record may resume, and the state it held there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resume {
    pub after: u64,
    pub carried: TurnState,
}

pub struct InputFacts {
    binding: SourceBinding,
    capture: Capture,
    boundaries: Vec<Boundary>,
    tracker: TurnTracker,
    state: TurnState,
    halted: Option<String>,
    events: Vec<Ordered>,
    saw_turn_evidence: bool,
    resume: Option<Resume>,
    /// A resumed reader's carried state; only the carried turn's own closer may end it.
    carried: TurnState,
    awaiting_boundary: bool,
    /// Offset of the record being read, or of the capture end before the first one.
    reading: u64,
}

impl InputFacts {
    /// A restart reconstructs from the same parent source, never from a saved turn cursor.
    pub fn open(binding: SourceBinding) -> Result<Self, String> {
        Self::open_at(binding, 0)
    }

    /// Reads from a complete-line `offset`; a turn open there stays Unknown until it closes.
    pub fn open_at(binding: SourceBinding, offset: u64) -> Result<Self, String> {
        Self::open_capture(binding, offset, false)
    }

    fn open_capture(
        binding: SourceBinding,
        offset: u64,
        observation: bool,
    ) -> Result<Self, String> {
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
        let capture = if observation {
            Capture::Observation(
                ObservationCapture::open(binding.source.clone(), offset)
                    .map_err(|e| e.to_string())?,
            )
        } else {
            Capture::Full(
                SourceCapture::open(binding.source.clone(), offset).map_err(|e| e.to_string())?,
            )
        };
        Ok(Self {
            binding,
            capture,
            boundaries: Vec::new(),
            tracker: TurnTracker::starting_at(offset),
            state: TurnState::Unknown,
            halted: None,
            events: Vec::new(),
            saw_turn_evidence: false,
            resume: None,
            carried: TurnState::Unknown,
            awaiting_boundary: false,
            reading: offset,
        })
    }

    /// Reopens after a typed `Blocked` record; Unknown until the carried turn's closer or an opener.
    pub fn resume(binding: SourceBinding, resume: &Resume) -> Result<Self, String> {
        let mut facts = Self::open_at(binding, resume.after)?;
        facts.awaiting_boundary = true;
        facts.carried = resume.carried.clone();
        Ok(facts)
    }

    /// Read-only observation: no prefix hash or durable checkpoint validation.
    pub fn open_observation_at(binding: SourceBinding, offset: u64) -> Result<Self, String> {
        Self::open_capture(binding, offset, true)
    }
    pub fn resume_observation(binding: SourceBinding, resume: &Resume) -> Result<Self, String> {
        let mut facts = Self::open_observation_at(binding, resume.after)?;
        facts.awaiting_boundary = true;
        facts.carried = resume.carried.clone();
        Ok(facts)
    }
    /// Carries a typed block only after the previous observation authenticates the handoff.
    pub fn resume_observation_checked(&self) -> Result<Self, String> {
        let Capture::Observation(old) = &self.capture else {
            return Err("handoff requires an observation capture".into());
        };
        old.validate_handoff().map_err(|e| e.to_string())?;
        let resume = self.resume_point().ok_or("not a typed block")?;
        let next = Self::resume_observation(self.binding.clone(), resume)?;
        old.validate_handoff().map_err(|e| e.to_string())?;
        Ok(next)
    }
    pub fn boundaries(&self) -> &[Boundary] {
        &self.boundaries
    }
    pub fn observation_progress(&self) -> (u64, u64) {
        match &self.capture {
            Capture::Full(c) => (c.read_through(), c.read_through()),
            Capture::Observation(c) => (c.read_through(), c.forward_bytes()),
        }
    }
    pub fn source_len(&self) -> Result<u64, String> {
        self.capture.file_len().map_err(|e| e.to_string())
    }
    pub fn poll_observation_until(
        &mut self,
        max: u64,
        end: u64,
        budget: u64,
    ) -> Result<ChannelFact, String> {
        if !matches!(self.capture, Capture::Observation(_)) {
            return Err("bounded poll requires an observation capture".into());
        }
        self.poll_limited(max, Some((end, budget)))
    }

    /// Set only when a typed `Blocked` record halted this reader.
    pub fn resume_point(&self) -> Option<&Resume> {
        self.resume.as_ref()
    }

    /// Whether any record fact other than a row that does not itself open a turn was read.
    pub fn saw_turn_evidence(&self) -> bool {
        self.saw_turn_evidence
    }

    /// A resumed reader that has not yet seen the carried turn's closer or a new opener.
    pub fn awaiting_boundary(&self) -> bool {
        self.awaiting_boundary
    }

    pub fn binding(&self) -> &SourceBinding {
        &self.binding
    }

    /// The last poll's ordered facts.
    pub fn events(&self) -> &[Ordered] {
        &self.events
    }

    /// Whether the captured prefix reaches the file's current end.
    pub fn caught_up(&self) -> Result<bool, String> {
        self.capture.caught_up().map_err(|e| e.to_string())
    }

    /// Only a fully captured prefix at the file's current end can supply Idle.
    pub fn poll(&mut self, max_bytes: u64) -> Result<ChannelFact, String> {
        self.poll_limited(max_bytes, None)
    }

    fn poll_limited(
        &mut self,
        max_bytes: u64,
        end: Option<(u64, u64)>,
    ) -> Result<ChannelFact, String> {
        if let Some(error) = &self.halted {
            return Err(error.clone());
        }
        match self.poll_inner(max_bytes, end) {
            Ok(fact) => Ok(fact),
            Err(error) => {
                // A turn this reader held open loses its outcome here; read it before it is reset.
                if let Some(open) = self.open_turn() {
                    let native = open.map(String::as_str);
                    observation::turn_unknown(&self.binding, self.reading, native, "reader_halted");
                }
                self.state = TurnState::Unknown;
                self.halted = Some(error.clone());
                Err(error)
            }
        }
    }

    /// The turn this reader treats as running, with its native id when one is known.
    fn open_turn(&self) -> Option<Option<&String>> {
        match (&self.state, &self.carried) {
            (TurnState::Open { native_turn_id }, _) => Some(native_turn_id.as_ref()),
            (_, TurnState::Open { native_turn_id }) if self.awaiting_boundary => {
                Some(native_turn_id.as_ref())
            }
            _ => None,
        }
    }

    fn poll_inner(
        &mut self,
        max_bytes: u64,
        end: Option<(u64, u64)>,
    ) -> Result<ChannelFact, String> {
        self.events.clear();
        self.boundaries.clear();
        self.reading = self.capture.captured_through();
        if self.binding.provider == ShadowProvider::Codex {
            strict_parent_session(&self.binding.source.path, &self.binding.source.session_id)
                .map_err(|e| e.to_string())?;
        }
        let batch = match self.capture.poll(max_bytes, end) {
            CaptureOutcome::Batch(batch) => batch,
            CaptureOutcome::Anomaly(anomaly) => return Err(anomaly.detail),
        };
        for record in batch.records {
            self.reading = record.start;
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
                self.saw_turn_evidence |= !matches!(fact, RecordFact::Prompt(false, _));
                // A foreign closer cannot release this channel's running parent turn.
                if let (RecordFact::Idle(Some(close)), Some(open)) =
                    (&fact, self.open_turn().flatten())
                    && open != close
                {
                    continue;
                }
                if matches!(self.capture, Capture::Observation(_)) {
                    let kind = match &fact {
                        RecordFact::TurnStart(_) => Some(BoundaryKind::Start),
                        RecordFact::Idle(_) => Some(BoundaryKind::Close),
                        _ => None,
                    };
                    if let Some(kind) = kind {
                        let id = match &fact {
                            RecordFact::TurnStart(id) | RecordFact::Idle(id) => id.clone(),
                            _ => None,
                        };
                        self.boundaries.push(Boundary {
                            kind,
                            native_turn_id: id,
                            range,
                        });
                    }
                }
                let event = self.tracker.observe(&fact, row.as_ref(), range, Utc::now());
                self.observe_unresolved(&event, &fact, range.0);
                let (next, closed) = match event {
                    TurnEvent::Opened(id) => (
                        Some(TurnState::Open {
                            native_turn_id: Some(id),
                        }),
                        None,
                    ),
                    TurnEvent::Closed(turn) => {
                        observation::turn_closed(
                            &self.binding,
                            &turn,
                            aborted(self.binding.provider, &value),
                        );
                        (Some(TurnState::Idle), turn.native_turn_id)
                    }
                    TurnEvent::StrayIdle | TurnEvent::EdgeTurn => (Some(TurnState::Idle), None),
                    TurnEvent::None => {
                        if let RecordFact::Blocked(reason) = &fact {
                            // Before a boundary the carried turn is still the one to protect.
                            let carried = match self.awaiting_boundary {
                                true => self.carried.clone(),
                                false => self.state.clone(),
                            };
                            let after = record.end;
                            self.resume = Some(Resume { after, carried });
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
        let complete = self.capture.caught_up().map_err(|e| e.to_string())?;
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

    /// A closer without its own turn's opener cannot be counted completed or aborted.
    fn observe_unresolved(&self, event: &TurnEvent, fact: &RecordFact, at: u64) {
        let RecordFact::Idle(close) = fact else {
            return;
        };
        let reason = match (event, self.open_turn()) {
            (TurnEvent::EdgeTurn, _) => "edge_turn",
            // Normal repeated closers with nothing open stay silent.
            (TurnEvent::StrayIdle | TurnEvent::None, Some(_)) => "unattributed_close",
            _ => return,
        };
        let native = close.as_ref().or(self.open_turn().flatten());
        observation::turn_unknown(&self.binding, at, native.map(String::as_str), reason);
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
        self.awaiting_boundary = false;
        self.carried = TurnState::Unknown;
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
mod resume_tests;
#[cfg(test)]
mod tests;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundaryKind {
    Start,
    Close,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Boundary {
    pub kind: BoundaryKind,
    pub native_turn_id: Option<String>,
    pub range: (u64, u64),
}

enum Capture {
    Full(SourceCapture),
    Observation(ObservationCapture),
}
impl Capture {
    fn captured_through(&self) -> u64 {
        match self {
            Self::Full(c) => c.captured_through(),
            Self::Observation(c) => c.captured_through(),
        }
    }
    fn file_len(&self) -> std::io::Result<u64> {
        match self {
            Self::Full(c) => c.file_len(),
            Self::Observation(c) => c.file_len(),
        }
    }
    fn caught_up(&self) -> std::io::Result<bool> {
        match self {
            Self::Full(c) => Ok(c.captured_through() == c.file_len()?),
            Self::Observation(c) => c.caught_up(),
        }
    }
    fn poll(&mut self, max: u64, end: Option<(u64, u64)>) -> CaptureOutcome {
        match self {
            Self::Full(c) => c.poll(max),
            Self::Observation(c) => match end {
                Some((end, budget)) => c.poll_until(max, end, budget),
                None => c.poll(max),
            },
        }
    }
}

#[cfg(test)]
mod observation_tests;
