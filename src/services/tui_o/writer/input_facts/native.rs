//! Claude native input record classification and inactive source-bound membership evidence.
#[cfg(all(any(test, feature = "test-support"), unix))]
use self::classify as classify_queued_record;
#[cfg(all(any(test, feature = "test-support"), unix))]
use super::Ordered;
#[cfg(unix)]
use super::{ShadowProvider, SourceBinding};
#[cfg(unix)]
use crate::services::tui_input::human_frame::history::v2::native::frame_hashes;
#[cfg(unix)]
use crate::services::tui_input::human_frame::history::v2::native::{Disposition, Ticket};
use serde_json::Value;
#[cfg(unix)]
mod receipt;
#[cfg(unix)]
pub mod terminal;
#[cfg(unix)]
pub(crate) use receipt::MembershipReceipt;

/// What one record says about the queue.
pub enum QueueRecord {
    Enqueue(String),
    /// Removed without running (`remove` other than mid-turn absorption).
    Withdraw(String),
    /// Handed to another agent (`remove`, `delivered_to_agent`); Claude writes no dequeue for it.
    Delivered(String),
    /// Every queued input pulled back to the prompt.
    WithdrawAll,
    Dequeue,
    /// Taken into the turn (`remove`, `absorbed_mid_turn`); Claude writes no dequeue for it, and its
    /// `queued_command` row may come before it, after it, or not at all.
    Absorbed(String),
    /// A human prompt command (`queued_command`, `prompt` mode, human origin).
    Human(String),
    /// Any other `queued_command`, such as a background task's notification drained mid-turn.
    Command(String),
    /// A user prompt row, or a meta row (a notification opening its own turn).
    Prompt(String),
}

pub fn classify(value: &Value) -> Option<QueueRecord> {
    let text = |key: &str| value.get(key).and_then(body);
    match value["type"].as_str()? {
        "queue-operation" => match value["operation"].as_str()? {
            "enqueue" => Some(QueueRecord::Enqueue(text("content")?)),
            "remove" if value["reason"] == "absorbed_mid_turn" => {
                Some(QueueRecord::Absorbed(text("content")?))
            }
            "remove" if value["reason"] == "delivered_to_agent" => {
                Some(QueueRecord::Delivered(text("content")?))
            }
            "remove" => Some(QueueRecord::Withdraw(text("content")?)),
            "popAll" => Some(QueueRecord::WithdrawAll),
            "dequeue" => Some(QueueRecord::Dequeue),
            _ => None,
        },
        "attachment" if value["attachment"]["type"] == "queued_command" => {
            let command = &value["attachment"];
            let human = command["commandMode"] == "prompt" && command["origin"]["kind"] == "human";
            let prompt = body(&command["prompt"])?;
            match human {
                true => Some(QueueRecord::Human(prompt)),
                false => Some(QueueRecord::Command(prompt)),
            }
        }
        "user" => body(&value["message"]["content"]).map(QueueRecord::Prompt),
        _ => None,
    }
}

/// A body as Claude records it: a string, or its text blocks joined.
fn body(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.trim().to_string()),
        Value::Array(blocks) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|block| block["type"] == "text")
                .filter_map(|block| block["text"].as_str())
                .collect();
            (!texts.is_empty()).then(|| texts.join("\n").trim().to_string())
        }
        _ => None,
    }
}

#[cfg(unix)]
pub use crate::services::tui_input::human_frame::history::v2::native::{
    EvidenceRegistration as Registration, registrations_fingerprint,
};

#[cfg(unix)]
use crate::services::tui_input::human_frame::history::v2::native::same_registered_file;

#[cfg(unix)]
impl Registration {
    pub fn matches(&self, binding: &SourceBinding) -> bool {
        self.ticket.matches_binding(binding, &self.devices)
    }
}

#[cfg(unix)]
impl Ticket {
    pub fn matches_binding(&self, binding: &SourceBinding, devices: &[u64]) -> bool {
        binding.provider == super::ShadowProvider::Claude
            && self.channel_id == binding.channel_id
            && self.source.as_ref().is_some_and(|s| {
                s.provider == "claude"
                    && s.session_id == binding.source.session_id
                    && s.transcript == binding.source.path
                    && same_registered_file(
                        s.file,
                        [binding.source.dev, binding.source.ino],
                        devices,
                    )
                    && s.target == self.key["attempt"]["target"]
                    && s.target["execution_nonce"]
                        .as_str()
                        .is_some_and(|n| !n.is_empty())
            })
    }
}

/// A candidate batch is read-only; its stamp is rechecked before any future actor mutation.
#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Observation {
    pub binding: SourceBinding,
    pub generation: u64,
    pub binding_seq: u64,
    pub revision: u64,
    pub through: u64,
    pub registrations_fingerprint: String,
    pub devices: Vec<u64>,
    /// A verified raw EOF closer at `through`; it never moves `through` itself.
    pub terminal: Option<terminal::RawWitness>,
    pub evidence: Vec<Evidence>,
    pub receipts: Vec<MembershipReceipt>,
}

#[cfg(all(any(test, feature = "test-support"), unix))]
pub(super) struct Record {
    pub(super) range: (u64, u64),
    pub(super) value: Value,
    pub(super) boundaries: Vec<Ordered>,
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Run {
    pub start: u64,
    pub(crate) native_id: Option<String>,
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Evidence {
    pub namespace: String,
    pub ticket: Ticket,
    pub run: Option<Run>,
    pub range: (u64, u64),
    pub membership_range: Option<(u64, u64)>,
    pub membership_row: Option<String>,
    pub closer_range: Option<(u64, u64)>,
    pub disposition: Disposition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) supersedes: Option<terminal::Supersession>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<terminal::RawWitness>,
}

#[cfg(unix)]
pub use crate::services::tui_input::human_frame::history::v2::native::barrier::Barrier;
#[cfg(unix)]
#[derive(Default, Debug, PartialEq, Eq)]
pub struct Analysis {
    pub evidence: Vec<Evidence>,
    pub barriers: Vec<Barrier>,
}

#[cfg(all(any(test, feature = "test-support"), unix))]
pub(super) use test_support::fold;

/// The reference fold tests compare against, shared with upper-crate finalizer tests (KRX-TS).
#[cfg(all(any(test, feature = "test-support"), unix))]
mod test_support;

#[cfg(all(test, unix))]
pub(crate) mod tests;

#[cfg(unix)]
pub type Projection = projection::Projection;

#[cfg(unix)]
mod projection {
    mod membership;
    use super::*;
    use crate::services::tui_o::shadow::capture::{
        MAX_PARTIAL_BYTES, PrefixConsumer, SourceCapture,
    };
    use crate::services::tui_o::shadow::identity::{RecordFact, classify, row_key};
    use crate::services::tui_o::shadow::seal::{TurnEvent, TurnTracker};
    use std::io;

    mod notice_absorption;
    #[cfg(test)]
    use std::io::Write;

    #[derive(Clone)]
    enum Event {
        Enqueue(Vec<String>),
        Withdraw(Vec<String>),
        Delivered(Vec<String>),
        WithdrawAll,
        Dequeue,
        Absorbed(Vec<String>),
        Human(Vec<String>),
        Command(Vec<String>),
        Prompt(Vec<String>),
    }
    enum Edge {
        Opened {
            range: (u64, u64),
            native_turn_id: Option<String>,
        },
        Closed {
            range: (u64, u64),
            aborted: bool,
        },
    }
    struct Fact {
        range: (u64, u64),
        row: Option<String>,
        meta: bool,
        event: Option<Event>,
        boundaries: Vec<Edge>,
        receipt: Option<receipt::Fact>,
        notice_absorption: Option<Box<notice_absorption::Removal>>,
        interrupt: Option<Vec<String>>,
    }
    /// Source-owned input hashes and native ranges, without output bodies or raw JSON records.
    pub struct Projection {
        binding: SourceBinding,
        facts: Vec<Fact>,
        tracker: TurnTracker,
        state: super::super::TurnState,
        through: u64,
    }
    struct Decoder {
        projection: Projection,
        partial: Vec<u8>,
        error: Option<String>,
    }
    impl Projection {
        /// Only the existing reopen hash pass can publish an authenticated seed.
        pub fn reopen(
            binding: SourceBinding,
            through: u64,
            hash: &str,
        ) -> io::Result<(SourceCapture, Result<Self, String>)> {
            let decoder = Decoder {
                projection: Self {
                    binding: binding.clone(),
                    facts: Vec::new(),
                    tracker: TurnTracker::starting_at(0),
                    state: super::super::TurnState::Unknown,
                    through: 0,
                },
                partial: Vec::new(),
                error: None,
            };
            let (capture, decoder) =
                SourceCapture::reopen_with_prefix(binding.source.clone(), through, hash, decoder)?;
            let result = if let Some(error) = decoder.error {
                Err(error)
            } else if !decoder.partial.is_empty() || decoder.projection.through != through {
                Err("native prefix ends inside a record or has incomplete coverage".into())
            } else if decoder.projection.binding.provider != super::super::ShadowProvider::Claude {
                Err("native prefix requires Claude".into())
            } else {
                Ok(decoder.projection)
            };
            Ok((capture, result))
        }
        #[cfg(test)]
        pub(crate) fn evidence(
            &self,
            registrations: &[Registration],
        ) -> Result<Vec<Evidence>, String> {
            self.analyze(registrations).map(|a| a.evidence)
        }
        pub fn analyze(&self, registrations: &[Registration]) -> Result<Analysis, String> {
            self.analyze_closed(registrations, None)
        }
        /// `raw` adds one closing edge after the last complete record; it is never a record.
        pub fn analyze_closed(
            &self,
            registrations: &[Registration],
            raw: Option<&terminal::RawWitness>,
        ) -> Result<Analysis, String> {
            let open = self.state != super::super::TurnState::Idle;
            let raw = raw.filter(|raw| open && raw.tail.through == self.through);
            let closer = raw.map(|raw| Fact {
                range: raw.range(),
                row: None,
                meta: false,
                event: None,
                boundaries: vec![Edge::Closed {
                    range: raw.range(),
                    aborted: raw.aborted,
                }],
                receipt: None,
                notice_absorption: None,
                interrupt: None,
            });
            let facts = (self.facts.as_slice(), closer.as_ref());
            let mut analysis = reduce(
                &self.binding,
                facts,
                registrations,
                self.through,
                None,
                None,
            )?;
            if let Some(raw) = raw {
                terminal::attach(&mut analysis.evidence, raw);
            }
            Ok(analysis)
        }
        pub(crate) fn receipts(&self, registrations: &[Registration]) -> Vec<MembershipReceipt> {
            receipt::candidates(
                &self.binding,
                &self
                    .facts
                    .iter()
                    .filter_map(|f| f.receipt.as_ref())
                    .collect::<Vec<_>>(),
                registrations,
            )
            .into_iter()
            .filter(|receipt| {
                let (namespace, ticket) = receipt.identity();
                let (_, row, range, _) = receipt.record();
                let mut scoped = registrations.to_vec();
                for registration in &mut scoped {
                    registration.candidate =
                        registration.namespace == namespace && registration.ticket == *ticket;
                    registration.barrier = None;
                    if registration.candidate {
                        // Confirmation cannot require the receipt being confirmed.
                        registration.receipts.clear();
                    }
                }
                let mut matched = false;
                reduce(
                    &self.binding,
                    (&self.facts, None),
                    &scoped,
                    self.through,
                    Some(&mut |registration, admitted_range, admitted_row| {
                        matched |= registration.namespace == namespace
                            && registration.ticket == *ticket
                            && admitted_range == range
                            && admitted_row == Some(row);
                    }),
                    None,
                )
                .is_ok()
                    && matched
            })
            .collect()
        }
        pub fn idle(&self) -> bool {
            self.state == super::super::TurnState::Idle
        }
        pub(crate) fn append(
            &mut self,
            record: &crate::services::tui_o::shadow::CapturedRecord,
        ) -> Result<(), String> {
            if record.end <= self.through {
                return Ok(());
            }
            if record.start != self.through
                || record.end != record.start + record.line.len() as u64 + 1
            {
                return Err("native capture has a gap or overlapping record".into());
            }
            self.read(&record.line)
        }
        pub(crate) fn through(&self) -> u64 {
            self.through
        }
        fn read(&mut self, line: &[u8]) -> Result<(), String> {
            let range = (self.through, self.through + line.len() as u64 + 1);
            self.through = range.1;
            if line.iter().all(u8::is_ascii_whitespace) {
                return Ok(());
            }
            let value: Value = serde_json::from_slice(line).map_err(|e| e.to_string())?;
            terminal::gate(&self.binding, &value)?;
            let row = row_key(&value);
            let mut boundaries = Vec::new();
            let facts = classify(self.binding.provider, &value);
            let strict = terminal::strict(&facts);
            for fact in facts {
                if let (
                    RecordFact::Idle(Some(close)),
                    super::super::TurnState::Open {
                        native_turn_id: Some(open),
                    },
                ) = (&fact, &self.state)
                    && open != close
                {
                    continue;
                }
                let next = match self.tracker.observe(&fact, row.as_ref()) {
                    TurnEvent::Opened(id) => Some(super::super::TurnState::Open {
                        native_turn_id: Some(id),
                    }),
                    TurnEvent::Closed(_) | TurnEvent::StrayIdle | TurnEvent::EdgeTurn => {
                        Some(super::super::TurnState::Idle)
                    }
                    TurnEvent::None => {
                        if let RecordFact::Blocked(reason) = &fact {
                            return Err(reason.clone());
                        }
                        if matches!(fact, RecordFact::TurnStart(_))
                            || (self.state == super::super::TurnState::Idle
                                && matches!(fact, RecordFact::Assistant))
                            || (row.is_none() && matches!(fact, RecordFact::Prompt(true, _)))
                        {
                            Some(super::super::TurnState::Open {
                                native_turn_id: None,
                            })
                        } else {
                            matches!(fact, RecordFact::Idle(_))
                                .then_some(super::super::TurnState::Idle)
                        }
                    }
                };
                if let Some(next) = next {
                    match &next {
                        super::super::TurnState::Open { native_turn_id } => {
                            boundaries.push(Edge::Opened {
                                range,
                                native_turn_id: native_turn_id.clone(),
                            })
                        }
                        _ if self.state != super::super::TurnState::Idle => {
                            boundaries.push(Edge::Closed {
                                range,
                                aborted: value["type"] == "user",
                            })
                        }
                        _ => {}
                    }
                    self.state = next;
                }
            }
            let queued = (!strict).then(|| super::classify(&value)).flatten();
            let event = queued.map(|event| match event {
                QueueRecord::Enqueue(b) => Event::Enqueue(frame_hashes(&b)),
                QueueRecord::Withdraw(b) => Event::Withdraw(frame_hashes(&b)),
                QueueRecord::Delivered(b) => Event::Delivered(frame_hashes(&b)),
                QueueRecord::WithdrawAll => Event::WithdrawAll,
                QueueRecord::Dequeue => Event::Dequeue,
                QueueRecord::Absorbed(b) => Event::Absorbed(frame_hashes(&b)),
                QueueRecord::Human(b) => Event::Human(frame_hashes(&b)),
                QueueRecord::Command(b) => Event::Command(frame_hashes(&b)),
                QueueRecord::Prompt(b) => Event::Prompt(frame_hashes(&b)),
            });
            if event.is_some() || !boundaries.is_empty() {
                self.facts.push(Fact {
                    range,
                    row: value["uuid"].as_str().map(str::to_string),
                    receipt: (!strict)
                        .then(|| receipt::read(&value, line, range))
                        .flatten(),
                    notice_absorption: notice_absorption::removal(&value, line),
                    interrupt: strict.then(|| terminal::interrupt_hashes(&value)).flatten(),
                    event,
                    boundaries,
                    meta: value["isMeta"] == true
                        || crate::services::tui_o::shadow::identity::record_is_compact_summary(
                            &value,
                        ),
                });
            }
            Ok(())
        }
    }
    impl PrefixConsumer for Decoder {
        fn prefix_bytes(&mut self, bytes: &[u8]) {
            if self.error.is_some() {
                return;
            }
            for part in bytes.split_inclusive(|b| *b == b'\n') {
                if self.partial.len() + part.len() > MAX_PARTIAL_BYTES {
                    self.error = Some("native prefix record exceeds the partial cap".into());
                    self.partial.clear();
                    break;
                }
                self.partial.extend_from_slice(part);
                if self.partial.last() == Some(&b'\n') {
                    self.partial.pop();
                    if let Err(error) = self.projection.read(&self.partial) {
                        self.error = Some(error);
                        self.partial.clear();
                        break;
                    }
                    self.partial.clear();
                }
            }
        }
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Stage {
        Waiting,
        Taken,
        Attached,
        Running,
        Removed,
    }
    struct Input {
        body: Vec<String>,
        owner: Option<usize>,
        start: u64,
        stage: Stage,
        fifo: bool,
        command: bool,
        absorbed: bool,
        pending: Option<PendingMembership>,
    }

    #[derive(Clone)]
    struct PendingMembership {
        range: (u64, u64),
        row: Option<String>,
    }

    #[derive(Clone)]
    struct Member {
        run: Run,
        range: (u64, u64),
        row: Option<String>,
    }

    fn same_body(a: &[String], b: &[String]) -> bool {
        b.iter().any(|hash| a.contains(hash))
    }

    type MembershipRecorder<'a> = &'a mut dyn FnMut(&Registration, (u64, u64), Option<&str>);

    fn reduce(
        binding: &SourceBinding,
        (records, closer): (&[Fact], Option<&Fact>),
        registrations: &[Registration],
        through: u64,
        mut membership: Option<MembershipRecorder<'_>>,
        mut notices: Option<&mut notice_absorption::NoticeFold<'_>>,
    ) -> Result<Analysis, String> {
        if binding.provider != super::ShadowProvider::Claude {
            return Ok(Analysis::default());
        }
        let interrupts: Vec<_> = records
            .iter()
            .filter_map(|f| Some((f.range, f.row.as_deref()?, f.interrupt.as_deref()?)))
            .collect();
        let registered: Vec<_> = registrations
            .iter()
            .filter(|r| {
                (r.candidate || terminal::superseding(r, &interrupts)) && r.matches(binding)
            })
            .collect();
        let windows: Vec<_> = registered
            .iter()
            .map(|r| notice_absorption::Window::new(r, notices.as_deref()))
            .collect();
        let mut inputs: Vec<Input> = vec![];
        let mut run: Option<Run> = None;
        let mut member: Vec<Option<Member>> = vec![None; registered.len()];
        let mut results: Vec<Option<Evidence>> = vec![None; registered.len()];
        let mut ambiguous = vec![false; registered.len()];
        let mut barriers = vec![None; registered.len()];
        let mut unmatched_dequeue = false;
        for record in records.iter().chain(closer) {
            if let Some(Event::Prompt(hashes)) = &record.event
                && !record.meta
            {
                for (i, owned) in member.iter().enumerate() {
                    if let Some(owned) = owned
                        && owned.range.1 <= record.range.0
                        && results[i].is_none()
                        && barriers[i].is_none()
                        && !ambiguous[i]
                    {
                        match terminal::foreign_prompt(
                            registrations,
                            binding,
                            registered[i],
                            (owned.range, owned.row.as_deref()),
                            (record.range, record.row.as_deref(), hashes),
                        ) {
                            Ok(Some(barrier)) => barriers[i] = Some(barrier),
                            Err(()) => ambiguous[i] = true,
                            Ok(None) => {}
                        }
                    }
                }
            }
            let mut opened_members = Vec::new();
            for boundary in &record.boundaries {
                match boundary {
                    Edge::Opened {
                        range,
                        native_turn_id,
                    } => {
                        let opened = Run {
                            start: range.0,
                            native_id: native_turn_id.clone(),
                        };
                        run = Some(opened.clone());
                        for input in &mut inputs {
                            if !matches!(input.stage, Stage::Taken | Stage::Attached) {
                                continue;
                            }
                            let Some(owner) = input.owner else { continue };
                            let registration = registered[owner];
                            if !windows[owner].accepts_row(registration, record)
                                || member[owner].is_some()
                            {
                                continue;
                            }
                            let Some(pending) = &input.pending else {
                                continue;
                            };
                            let prompt_matches = record.event.as_ref()
                            .is_some_and(|event| matches!(event, Event::Prompt(body) if same_body(body, &input.body)));
                            member[owner] = Some(Member {
                                run: opened.clone(),
                                range: if prompt_matches {
                                    record.range
                                } else {
                                    pending.range
                                },
                                row: if prompt_matches {
                                    record.row.as_deref().map(str::to_string)
                                } else {
                                    pending.row.clone()
                                },
                            });
                            if let (Some(record_member), Some(owned)) =
                                (membership.as_mut(), &member[owner])
                            {
                                record_member(registration, owned.range, owned.row.as_deref());
                            }
                            input.stage = Stage::Running;
                            opened_members.push(owner);
                        }
                    }
                    Edge::Closed { range, aborted, .. } => {
                        if let Some(closing) = run.take() {
                            for (i, membership) in member.iter().enumerate() {
                                if let Some(owned) = membership
                                    && owned.run == closing
                                    && !ambiguous[i]
                                    && barriers[i].is_none()
                                {
                                    results[i] = Some(Evidence {
                                        namespace: registered[i].namespace.clone(),
                                        ticket: registered[i].ticket.clone(),
                                        run: Some(owned.run.clone()),
                                        range: (owned.range.1, range.1),
                                        membership_range: Some(owned.range),
                                        membership_row: owned.row.clone(),
                                        closer_range: Some(*range),
                                        disposition: if *aborted {
                                            Disposition::Cancelled
                                        } else {
                                            Disposition::Executed
                                        },
                                        supersedes: None,
                                        raw: None,
                                    });
                                }
                            }
                        }
                    }
                }
            }
            let eligible = |i: usize| windows[i].contains(record.range.0);
            let candidate = |body: &[String], receipt: bool| {
                let candidates: Vec<_> = registered
                    .iter()
                    .enumerate()
                    .filter(|(i, r)| {
                        eligible(*i)
                            && body.contains(&r.frame_hash)
                            && (!receipt
                                || record.row.as_deref().is_some_and(|uuid| {
                                    r.receipts.iter().any(|known| known == uuid)
                                }))
                    })
                    .map(|(i, _)| i)
                    .collect();
                if candidates.len() == 1 {
                    candidates.first().copied()
                } else {
                    None
                }
            };
            let Some(event) = &record.event else {
                continue;
            };
            match event {
                Event::Enqueue(body) => {
                    let owner = candidate(body, false)
                        .filter(|&i| windows[i].unique_enqueue(records, body));
                    inputs.push(Input {
                        body: body.clone(),
                        owner,
                        start: record.range.0,
                        stage: Stage::Waiting,
                        fifo: true,
                        command: false,
                        absorbed: false,
                        pending: None,
                    });
                }
                Event::Dequeue => {
                    if let Some(input) = inputs.iter_mut().find(|input| input.fifo) {
                        input.fifo = false;
                        if input.stage == Stage::Waiting {
                            input.stage = Stage::Taken;
                            input.pending = Some(PendingMembership {
                                range: record.range,
                                row: None,
                            });
                        }
                    } else {
                        unmatched_dequeue = true;
                    }
                }
                Event::WithdrawAll => {
                    for input in &mut inputs {
                        if matches!(input.stage, Stage::Waiting | Stage::Taken | Stage::Attached) {
                            settle_removed(
                                input,
                                record.range.1,
                                &registered,
                                &mut results,
                                Disposition::WithdrawnNotExecuted,
                            );
                            input.fifo = false;
                        }
                    }
                }
                Event::Withdraw(body) | Event::Delivered(body) => {
                    let delivered = matches!(event, Event::Delivered(_));
                    let matches: Vec<_> = inputs
                        .iter()
                        .enumerate()
                        .filter(|(_, i)| {
                            same_body(&i.body, body)
                                && matches!(
                                    i.stage,
                                    Stage::Waiting | Stage::Taken | Stage::Attached
                                )
                        })
                        .map(|(at, _)| at)
                        .collect();
                    if let [at] = matches.as_slice() {
                        settle_removed(
                            &mut inputs[*at],
                            record.range.1,
                            &registered,
                            &mut results,
                            if delivered {
                                Disposition::TransferredUnverified
                            } else {
                                Disposition::WithdrawnNotExecuted
                            },
                        );
                        if delivered {
                            inputs[*at].fifo = false;
                        }
                    }
                }
                Event::Absorbed(body)
                | Event::Human(body)
                | Event::Command(body)
                | Event::Prompt(body) => {
                    let absorption = matches!(event, Event::Absorbed(_));
                    let command = matches!(event, Event::Human(_) | Event::Command(_));
                    let receipt = candidate(body, true);
                    if receipt.is_none() && (matches!(event, Event::Command(_)) || record.meta) {
                        continue;
                    }
                    let matches: Vec<_> = inputs
                        .iter()
                        .enumerate()
                        .filter(|(_, i)| {
                            same_body(&i.body, body)
                                && i.stage != Stage::Removed
                                && if absorption {
                                    !i.absorbed
                                } else if command {
                                    !i.command
                                } else {
                                    matches!(i.stage, Stage::Taken | Stage::Attached)
                                }
                        })
                        .map(|(at, _)| at)
                        .collect();
                    let at = if let [at] = matches.as_slice() {
                        Some(*at)
                    } else {
                        None
                    };
                    let owner = receipt
                        .or_else(|| {
                            at.and_then(|at| inputs[at].owner)
                                .filter(|i| windows[*i].accepts_row(registered[*i], record))
                        })
                        .or_else(|| {
                            (matches.is_empty()
                                && !absorption
                                && !unmatched_dequeue
                                && !inputs.iter().any(|input| same_body(&input.body, body)))
                            .then(|| {
                                candidate(body, false)
                                    .filter(|i| registered[*i].receipts.is_empty())
                            })
                            .flatten()
                        });
                    if let Some(at) = at {
                        if absorption
                            && owner.is_none()
                            && run.is_some()
                            && inputs[at].owner.is_none()
                            && let Some(notices) = notices.as_deref_mut()
                            && !registered
                                .iter()
                                .enumerate()
                                .any(|(i, r)| eligible(i) && body.contains(&r.frame_hash))
                        {
                            notices.unowned.insert(record.range, inputs[at].start);
                        }
                        if inputs[at]
                            .owner
                            .is_some_and(|i| !windows[i].accepts_row(registered[i], record))
                        {
                            continue;
                        }
                        let input = &mut inputs[at];
                        input.command |= command;
                        input.absorbed |= absorption;
                        input.fifo = false;
                        input.pending = Some(PendingMembership {
                            range: record.range,
                            row: record.row.as_deref().map(str::to_string),
                        });
                        input.stage = if run.is_some() {
                            Stage::Running
                        } else {
                            Stage::Attached
                        };
                    }
                    if let Some(record_member) = membership.as_mut() {
                        let receipt_owner = owner.or_else(|| {
                            if !matches!(event, Event::Prompt(_)) {
                                return None;
                            }
                            let candidates: Vec<_> = inputs
                                .iter()
                                .filter(|i| {
                                    same_body(&i.body, body)
                                        && i.stage == Stage::Running
                                        && i.command
                                })
                                .filter_map(|i| i.owner)
                                .collect();
                            let [owner] = candidates.as_slice() else {
                                return None;
                            };
                            Some(*owner)
                        });
                        if let Some(owner) = receipt_owner
                            && member[owner]
                                .as_ref()
                                .is_none_or(|m| run.as_ref() == Some(&m.run))
                        {
                            record_member(registered[owner], record.range, record.row.as_deref());
                        }
                    }
                    if let (Some(owner), Some(current)) = (owner, &run) {
                        if let Some(old) = &member[owner] {
                            if old.run != *current
                                || (at.is_none() && !opened_members.contains(&owner))
                            {
                                ambiguous[owner] = true;
                                results[owner] = None;
                            }
                        } else {
                            member[owner] = Some(Member {
                                run: current.clone(),
                                range: record.range,
                                row: record.row.as_deref().map(str::to_string),
                            });
                        }
                    }
                }
            }
        }
        if unmatched_dequeue {
            return Err("native FIFO is missing its enqueue".into());
        }
        let fingerprint = registrations_fingerprint(registrations)?;
        Ok(Analysis {
            evidence: results
                .into_iter()
                .enumerate()
                .filter_map(|(i, result)| {
                    (!ambiguous[i] && barriers[i].is_none())
                        .then_some(result)
                        .flatten()
                        .and_then(|evidence| terminal::seal(registered[i], evidence))
                })
                .collect(),
            barriers: barriers
                .into_iter()
                .enumerate()
                .filter_map(|(i, proof)| proof.filter(|_| registered[i].candidate))
                .map(|mut proof| {
                    proof.through = through;
                    proof.registrations_fingerprint = fingerprint.clone();
                    proof
                })
                .collect(),
        })
    }

    fn settle_removed(
        input: &mut Input,
        end: u64,
        registered: &[&Registration],
        results: &mut [Option<Evidence>],
        disposition: Disposition,
    ) {
        if let Some(owner) = input.owner {
            results[owner] = Some(Evidence {
                namespace: registered[owner].namespace.clone(),
                ticket: registered[owner].ticket.clone(),
                run: None,
                range: (input.start, end),
                membership_range: None,
                membership_row: None,
                closer_range: None,
                disposition,
                supersedes: None,
                raw: None,
            });
        }
        input.stage = Stage::Removed;
    }
    #[cfg(test)]
    #[test]
    fn native_8148_projection_storage_excludes_output_payloads() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        for _ in 0..128 {
            writeln!(
                file,
                "{}",
                serde_json::json!({"type":"system","subtype":"diagnostic","blob":"x".repeat(4096)})
            )
            .unwrap();
        }
        for row in [
            serde_json::json!({"type":"queue-operation","operation":"enqueue","content":"private input"}),
            serde_json::json!({"type":"queue-operation","operation":"dequeue"}),
            serde_json::json!({"type":"user","uuid":"ours","message":{"content":"private input"}}),
            serde_json::json!({"type":"system","subtype":"turn_duration"}),
        ] {
            writeln!(file, "{row}").unwrap();
        }
        let bytes = std::fs::read(file.path()).unwrap();
        let (dev, ino) = crate::services::tui_o::shadow::capture::file_identity(
            &file.as_file().metadata().unwrap(),
        );
        let binding = SourceBinding {
            channel_id: 81489890,
            provider: ShadowProvider::Claude,
            source: crate::services::tui_o::shadow::SourceId {
                session_id: "parent".into(),
                path: file.path().into(),
                dev,
                ino,
            },
        };
        use sha2::Digest;
        let (_, seed) = Projection::reopen(
            binding,
            bytes.len() as u64,
            &hex::encode(sha2::Sha256::digest(&bytes)),
        )
        .unwrap();
        let seed = seed.unwrap();
        assert_eq!(seed.facts.len(), 4);
        assert_eq!(
            seed.facts.iter().map(|f| f.boundaries.len()).sum::<usize>(),
            2
        );
        assert!(
            matches!(&seed.facts[0].event, Some(Event::Enqueue(hashes)) if *hashes == frame_hashes("private input"))
        );
    }
}
