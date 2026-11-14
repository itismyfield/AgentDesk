//! Claude native input record classification and inactive source-bound membership evidence.
#[cfg(all(test, unix))]
use self::classify as classify_queued_record;
#[cfg(all(test, unix))]
use super::Ordered;
#[cfg(unix)]
use super::{ShadowProvider, SourceBinding};
#[cfg(unix)]
use crate::services::tui_input::human_frame::history::v2::native::frame_hashes;
#[cfg(unix)]
use crate::services::tui_input::human_frame::history::v2::native::{Disposition, Ticket};
use serde_json::Value;

/// What one record says about the queue.
pub(crate) enum QueueRecord {
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

pub(crate) fn classify(value: &Value) -> Option<QueueRecord> {
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
#[derive(Clone, serde::Serialize)]
pub(crate) struct Registration {
    pub(crate) namespace: String,
    pub(crate) ticket: Ticket,
    pub(crate) frame_hash: String,
    pub(crate) receipts: Vec<String>,
}

#[cfg(all(test, unix))]
pub(crate) fn registrations_fingerprint(registrations: &[Registration]) -> Result<String, String> {
    serde_json::to_vec(registrations)
        .map(|bytes| blake3::hash(&bytes).to_hex().to_string())
        .map_err(|e| e.to_string())
}

#[cfg(unix)]
impl Registration {
    pub(crate) fn matches(&self, binding: &SourceBinding) -> bool {
        binding.provider == super::ShadowProvider::Claude
            && self.ticket.channel_id == binding.channel_id
            && self.ticket.source.as_ref().is_some_and(|s| {
                s.provider == "claude"
                    && s.session_id == binding.source.session_id
                    && s.transcript == binding.source.path
                    && s.file == [binding.source.dev, binding.source.ino]
                    && s.target == self.ticket.key["attempt"]["target"]
                    && s.target["execution_nonce"]
                        .as_str()
                        .is_some_and(|n| !n.is_empty())
            })
    }
}

/// A candidate batch is read-only; its stamp is rechecked before any future actor mutation.
#[cfg(all(test, unix))]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct Observation {
    pub(crate) binding: SourceBinding,
    pub(crate) generation: u64,
    pub(crate) binding_seq: u64,
    pub(crate) revision: u64,
    pub(crate) through: u64,
    pub(crate) registrations_fingerprint: String,
    pub(crate) evidence: Vec<Evidence>,
}

#[cfg(all(test, unix))]
pub(super) struct Record {
    pub(super) range: (u64, u64),
    pub(super) value: Value,
    pub(super) boundaries: Vec<Ordered>,
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Run {
    pub(crate) start: u64,
    pub(crate) native_id: Option<String>,
}

#[cfg(unix)]
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Evidence {
    pub(crate) namespace: String,
    pub(crate) ticket: Ticket,
    pub(crate) run: Option<Run>,
    pub(crate) range: (u64, u64),
    pub(crate) membership_range: Option<(u64, u64)>,
    pub(crate) membership_row: Option<String>,
    pub(crate) closer_range: Option<(u64, u64)>,
    pub(crate) disposition: Disposition,
}

#[cfg(all(test, unix))]
pub(super) use tests::fold;

#[cfg(all(test, unix))]
mod tests;

#[cfg(unix)]
pub(crate) use projection::Projection;

#[cfg(unix)]
mod projection {
    use super::*;
    use crate::services::tui_o::shadow::capture::{
        MAX_PARTIAL_BYTES, PrefixConsumer, SourceCapture,
    };
    use crate::services::tui_o::shadow::identity::{RecordFact, classify, row_key};
    use crate::services::tui_o::shadow::seal::{TurnEvent, TurnTracker};
    use std::io;
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
    }
    /// Source-owned input hashes and native ranges, without output bodies or raw JSON records.
    pub(crate) struct Projection {
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
        pub(crate) fn reopen(
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
        pub(crate) fn evidence(
            &self,
            registrations: &[Registration],
        ) -> Result<Vec<Evidence>, String> {
            reduce(&self.binding, &self.facts, registrations)
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
            if value["isSidechain"] == true
                || self
                    .binding
                    .source
                    .path
                    .components()
                    .any(|p| p.as_os_str() == "subagents")
            {
                return Err("native prefix requires a parent source".into());
            }
            if value["sessionId"]
                .as_str()
                .is_some_and(|s| s != self.binding.source.session_id)
            {
                return Err("native record belongs to another source".into());
            }
            let row = row_key(&value);
            let mut boundaries = Vec::new();
            for fact in classify(self.binding.provider, &value) {
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
            let event = super::classify(&value).map(|event| match event {
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
                    event,
                    boundaries,
                    meta: value["isMeta"] == true || value["isCompactSummary"] == true,
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

    fn reduce(
        binding: &SourceBinding,
        records: &[Fact],
        registrations: &[Registration],
    ) -> Result<Vec<Evidence>, String> {
        if binding.provider != super::ShadowProvider::Claude {
            return Ok(vec![]);
        }
        let registered: Vec<_> = registrations
            .iter()
            .filter(|r| r.matches(binding))
            .collect();
        let mut inputs: Vec<Input> = vec![];
        let mut run: Option<Run> = None;
        let mut member: Vec<Option<Member>> = vec![None; registered.len()];
        let mut results: Vec<Option<Evidence>> = vec![None; registered.len()];
        let mut ambiguous = vec![false; registered.len()];
        let mut unmatched_dequeue = false;
        for record in records {
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
                            if !accepts_row(registration, record) || member[owner].is_some() {
                                continue;
                            }
                            let Some(pending) = &input.pending else {
                                continue;
                            };
                            let prompt_matches = record.event.clone()
                            .is_some_and(|event| matches!(event, Event::Prompt(body) if same_body(&body, &input.body)));
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
                                    });
                                }
                            }
                        }
                    }
                }
            }
            let eligible = |i: usize| {
                registered[i]
                    .ticket
                    .source
                    .as_ref()
                    .is_some_and(|s| s.offset <= record.range.0)
            };
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
            let Some(event) = record.event.clone() else {
                continue;
            };
            match event {
                Event::Enqueue(body) => {
                    let owner = candidate(&body, false).filter(|i| {
                    records.iter().filter(|r| r.range.0 >= registered[*i].ticket.source.as_ref().map_or(u64::MAX, |s| s.offset))
                        .filter(|r| matches!(r.event.clone(), Some(Event::Enqueue(ref b)) if same_body(b, &body))).count() == 1
                });
                    inputs.push(Input {
                        body,
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
                    let delivered = matches!(record.event.clone(), Some(Event::Delivered(_)));
                    let matches: Vec<_> = inputs
                        .iter()
                        .enumerate()
                        .filter(|(_, i)| {
                            same_body(&i.body, &body)
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
                    let kind = record.event.clone();
                    let absorption = matches!(kind, Some(Event::Absorbed(_)));
                    let command = matches!(kind, Some(Event::Human(_) | Event::Command(_)));
                    let receipt = candidate(&body, true);
                    if receipt.is_none() && (matches!(kind, Some(Event::Command(_))) || record.meta)
                    {
                        continue;
                    }
                    let matches: Vec<_> = inputs
                        .iter()
                        .enumerate()
                        .filter(|(_, i)| {
                            same_body(&i.body, &body)
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
                                .filter(|i| accepts_row(registered[*i], record))
                        })
                        .or_else(|| {
                            (matches.is_empty()
                                && !absorption
                                && !unmatched_dequeue
                                && !inputs.iter().any(|input| same_body(&input.body, &body)))
                            .then(|| {
                                candidate(&body, false)
                                    .filter(|i| registered[*i].receipts.is_empty())
                            })
                            .flatten()
                        });
                    if let Some(at) = at {
                        if inputs[at]
                            .owner
                            .is_some_and(|i| !accepts_row(registered[i], record))
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
        Ok(results
            .into_iter()
            .enumerate()
            .filter_map(|(i, result)| (!ambiguous[i]).then_some(result).flatten())
            .collect())
    }

    fn accepts_row(registration: &Registration, value: &Fact) -> bool {
        registration.receipts.is_empty()
            || value
                .row
                .as_deref()
                .is_some_and(|uuid| registration.receipts.iter().any(|known| known == uuid))
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
