//! Dormant input adapter: replay O's parent transcript facts without storing turn state.

use serde_json::Value;
use sha2::{Digest, Sha256};

use super::restart_end::VerifiedEnd;
use crate::services::codex_tui::rollout_index::strict_parent_session;
use crate::services::tui_o::shadow::capture::{
    Fingerprint, SourceCapture, observation::ObservationCapture,
};
use crate::services::tui_o::shadow::identity::{RecordFact, classify, row_key};
use crate::services::tui_o::shadow::seal::{TurnEvent, TurnTracker};
use crate::services::tui_o::shadow::{
    CaptureOutcome, CaptureSource, ShadowProvider, SourceBinding,
};

pub(crate) mod native;
mod queued;
pub(crate) use native::{QueueRecord, classify as classify_queued_record};
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
    /// The open turn holds only `/effort`·`/model` echoes and their output: no model turn runs.
    pub local: bool,
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
    pub open_after: Option<u64>,
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
    /// Whether the last close was an interrupt, kept only when this reader read an opener first, so
    /// every receipt of that turn lies in what it read.
    last_close_aborted: Option<bool>,
    opened: bool,
    local: bool,
    /// Queued inputs received and not yet run to a close.
    queued: queued::QueuedInputs,
    open_after: Option<u64>,
    restart_ends: Vec<VerifiedEnd>,
    prefix: Option<Sha256>,
    prefix_through: u64,
    restart_stale: bool,
    physical_end_id: Option<String>,
    physical_snapshot: Option<Fingerprint>,
    #[cfg(all(test, unix))]
    native_records: Option<Vec<native::Record>>,
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
            last_close_aborted: None,
            opened: false,
            local: false,
            queued: queued::QueuedInputs::default(),
            open_after: None,
            restart_ends: Vec::new(),
            prefix: (offset == 0).then(Sha256::new),
            prefix_through: offset,
            restart_stale: false,
            physical_end_id: None,
            physical_snapshot: None,
            #[cfg(all(test, unix))]
            native_records: None,
        })
    }

    /// Reopens after a typed `Blocked` record; Unknown until the carried turn's closer or an opener.
    pub fn resume(binding: SourceBinding, resume: &Resume) -> Result<Self, String> {
        let mut facts = Self::open_at(binding, resume.after)?;
        facts.awaiting_boundary = true;
        facts.carried = resume.carried.clone();
        facts.open_after = resume.open_after;
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
        facts.open_after = resume.open_after;
        Ok(facts)
    }
    /// Carries a typed block only after the previous observation authenticates the handoff.
    pub fn resume_observation_checked(&self) -> Result<Self, String> {
        let Capture::Observation(old) = &self.capture else {
            return Err("handoff requires an observation capture".into());
        };
        old.validate_handoff().map_err(|e| e.to_string())?;
        let resume = self.resume_point().ok_or("not a typed block")?;
        let mut next = Self::resume_observation(self.binding.clone(), resume)?;
        next.set_restart_ends(self.restart_ends.clone());
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

    /// Whether the last close read was an interrupt; `None` before a close whose turn this reader
    /// read from an opener.
    pub fn last_close_aborted(&self) -> Option<bool> {
        self.last_close_aborted
    }

    /// A queued input was received whose native run has not started, so a run is still owed.
    pub fn pending_receipt(&self) -> bool {
        self.queued.owed() > 0
    }

    /// How many received inputs have not started their native run.
    pub fn owed_inputs(&self) -> usize {
        self.queued.owed()
    }

    pub fn binding(&self) -> &SourceBinding {
        &self.binding
    }

    /// Installs ends whose source, binding provenance and prefix a worker already checked.
    pub(crate) fn set_restart_ends(&mut self, mut ends: Vec<VerifiedEnd>) {
        ends.sort_by(|a, b| {
            (a.cutoff, a.open_after, &a.end_id).cmp(&(b.cutoff, b.open_after, &b.end_id))
        });
        self.restart_ends = ends;
        self.restart_stale = false;
    }

    pub(crate) fn restart_needs_reload(&self) -> bool {
        self.restart_stale
    }

    pub(crate) fn restart_ended(&self) -> bool {
        self.physical_end_id.is_some() && self.state == TurnState::Idle
    }

    pub(crate) fn restart_proof_current(&self) -> bool {
        match (
            &self.physical_snapshot,
            Fingerprint::path(&self.binding.source),
        ) {
            (Some(kept), Ok(now)) => kept == &now,
            _ => false,
        }
    }

    pub(crate) fn restart_proof_grew(&self) -> bool {
        self.physical_snapshot.as_ref().is_some_and(|kept| {
            Fingerprint::path(&self.binding.source)
                .is_ok_and(|now| (now.dev, now.ino) == (kept.dev, kept.ino) && now.len > kept.len)
        })
    }

    /// Worker-only: recheck the applied end's native prefix without replaying the reader.
    pub(crate) fn renew_restart_proof(&mut self) -> bool {
        let Some(index) = self
            .restart_ends
            .iter()
            .position(|end| Some(&end.end_id) == self.physical_end_id.as_ref())
        else {
            return false;
        };
        let Some(renewed) = self.restart_ends[index].renewed() else {
            return false;
        };
        self.physical_snapshot = renewed.native_snapshot().cloned();
        self.restart_ends[index] = renewed;
        self.physical_snapshot.is_some()
    }

    fn apply_restart_ends(&mut self, boundary: u64, snapshot: Option<&Fingerprint>) {
        if self.restart_stale {
            return;
        }
        let Some(native_turn_id) = self.open_turn() else {
            return;
        };
        let proof = self
            .restart_ends
            .iter()
            .find(|end| {
                end.channel == self.binding.channel_id
                    && end.provider == self.binding.provider
                    && end.source == self.binding.source
                    && end.cutoff == boundary
                    && Some(end.open_after) == self.open_after
                    && end.open_after <= end.cutoff
                    && end.native_turn_id.as_ref() == native_turn_id
                    && self.prefix_through == boundary
            })
            .map(|end| {
                (
                    end.native_current(&self.binding.source),
                    end.accepts_prefix(self.prefix.as_ref(), &self.binding.source),
                    end.end_id.clone(),
                )
            });
        let Some((current, prefix, end_id)) = proof else {
            return;
        };
        if !current {
            self.restart_stale = true;
            return;
        }
        if prefix && snapshot.is_some() {
            self.tracker = TurnTracker::starting_at(0);
            self.state = TurnState::Idle;
            self.carried = TurnState::Unknown;
            self.awaiting_boundary = false;
            self.open_after = None;
            self.opened = false;
            self.local = false;
            self.last_close_aborted = None;
            self.queued.closed();
            self.physical_end_id = Some(end_id);
            self.physical_snapshot = snapshot.cloned();
        }
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
        if self.binding.provider == ShadowProvider::Codex {
            strict_parent_session(&self.binding.source.path, &self.binding.source.session_id)
                .map_err(|e| e.to_string())?;
        }
        if self.restart_ends.iter().any(|end| {
            end.channel == self.binding.channel_id
                && end.provider == self.binding.provider
                && end.source == self.binding.source
                && Some(&end.end_id) != self.physical_end_id.as_ref()
                && end.cutoff >= self.capture.captured_through()
                && !end.native_current(&self.binding.source)
        }) {
            self.restart_stale = true;
        }
        let snapshot = if self.restart_ends.is_empty() {
            None
        } else {
            Some(Fingerprint::path(&self.binding.source).map_err(|(_, error)| error)?)
        };
        let batch = match self.capture.poll(max_bytes, end) {
            CaptureOutcome::Batch(batch) => batch,
            CaptureOutcome::Anomaly(anomaly) => return Err(anomaly.detail),
        };
        for record in batch.records {
            self.apply_restart_ends(record.start, snapshot.as_ref());
            // Only contiguous raw records from byte zero authenticate a durable prefix.
            if self.prefix_through != record.start
                || record.end - record.start != record.line.len() as u64 + 1
            {
                self.prefix = None;
            }
            if let Some(hash) = &mut self.prefix {
                hash.update(&record.line);
                hash.update(b"\n");
            }
            self.prefix_through = record.end;
            if record.line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let value: Value = serde_json::from_slice(&record.line).map_err(|e| e.to_string())?;
            if value.get("isSidechain") == Some(&Value::Bool(true)) {
                return Err("sidechain record in parent transcript".into());
            }
            let row = row_key(&value);
            let was_open = matches!(self.state, TurnState::Open { .. });
            let range = (record.start, record.end);
            #[cfg(all(test, unix))]
            let event_start = self.events.len();
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
                let local = local_row(self.binding.provider, &fact);
                let event = self.tracker.observe(&fact, row.as_ref());
                let (next, closed) = match event {
                    TurnEvent::Opened(id) => (
                        Some(TurnState::Open {
                            native_turn_id: Some(id),
                        }),
                        None,
                    ),
                    TurnEvent::Closed(id) => (Some(TurnState::Idle), Some(id)),
                    TurnEvent::StrayIdle | TurnEvent::EdgeTurn => (Some(TurnState::Idle), None),
                    TurnEvent::None => {
                        if let RecordFact::Blocked(reason) = &fact {
                            // Before a boundary the carried turn is still the one to protect.
                            let carried = match self.awaiting_boundary {
                                true => self.carried.clone(),
                                false => self.state.clone(),
                            };
                            let after = record.end;
                            self.resume = Some(Resume {
                                after,
                                carried,
                                open_after: self.open_after,
                            });
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
                let opens = matches!(next, Some(TurnState::Open { .. }));
                if let Some(next) = next {
                    self.mark(next, range, closed, &value);
                }
                self.local = matches!(self.state, TurnState::Open { .. })
                    && local.is_some_and(|setting| if opens { setting } else { self.local });
            }
            let open = matches!(self.state, TurnState::Open { .. });
            let provider = self.binding.provider;
            self.queued.read(provider, &value, open, open && !was_open);
            #[cfg(all(test, unix))]
            if let Some(records) = &mut self.native_records
                && (classify_queued_record(&value).is_some() || self.events.len() > event_start)
            {
                records.push(native::Record {
                    range,
                    value: value.clone(),
                    boundaries: self.events[event_start..].to_vec(),
                });
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
        self.apply_restart_ends(through, snapshot.as_ref());
        let complete = self.capture.caught_up().map_err(|e| e.to_string())?;
        let state = if !complete && self.state == TurnState::Idle {
            TurnState::Unknown
        } else {
            self.state.clone()
        };
        Ok(ChannelFact {
            binding: self.binding.clone(),
            through,
            local: self.local && matches!(state, TurnState::Open { .. }),
            state,
        })
    }

    #[cfg(all(test, unix))]
    pub(crate) fn observe_native(&mut self) -> Result<(), String> {
        if !matches!(&self.capture, Capture::Full(_))
            || self.capture.captured_through() != 0
            || self.prefix.is_none()
            || self.native_records.is_some()
        {
            return Err("native capture must start before the first record".into());
        }
        self.native_records = Some(Vec::new());
        Ok(())
    }

    #[cfg(all(test, unix))]
    pub(crate) fn native_evidence(
        &self,
        registrations: &[native::Registration],
    ) -> Result<Vec<native::Evidence>, String> {
        if !matches!(&self.capture, Capture::Full(_))
            || self.halted.is_some()
            || self.prefix.is_none()
        {
            return Err("native evidence requires an authenticated complete prefix".into());
        }
        native::fold(
            &self.binding,
            self.native_records
                .as_deref()
                .ok_or("native capture not enabled")?,
            registrations,
        )
    }

    fn mark(&mut self, next: TurnState, range: (u64, u64), closed: Option<String>, value: &Value) {
        self.physical_end_id = None;
        self.physical_snapshot = None;
        match &next {
            TurnState::Open { native_turn_id } => {
                self.opened = true;
                self.open_after = Some(range.1);
                self.events.push(Ordered::Opened {
                    range,
                    native_turn_id: native_turn_id.clone(),
                })
            }
            _ if self.state != TurnState::Idle => {
                self.open_after = None;
                let aborted = aborted(self.binding.provider, value);
                let opened = self.opened;
                self.last_close_aborted = opened.then_some(aborted);
                self.queued.closed();
                self.events.push(Ordered::Closed {
                    range,
                    native_turn_id: closed,
                    aborted,
                })
            }
            _ => {}
        }
        self.state = next;
        self.awaiting_boundary = false;
        self.carried = TurnState::Unknown;
    }
}

/// A Claude settings-control echo (`Some(true)`, may open) or local command output (`Some(false)`).
fn local_row(provider: ShadowProvider, fact: &RecordFact) -> Option<bool> {
    use crate::services::tui_prompt_control as control;
    let RecordFact::Prompt(true, text) = fact else {
        return None;
    };
    let kind = (provider == ShadowProvider::Claude)
        .then(|| control::classify_local_only_slash_control(text))??
        .kind;
    match control::OBSERVATION_ONLY_LOCAL_SLASH_COMMANDS.contains(&kind.as_str()) {
        true => Some(true),
        false => (kind == "local-command-stdout").then_some(false),
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
mod receipt_tests;
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
