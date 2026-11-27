//! Strict terminal rows close their run; they never read as a prompt, receipt or foreign barrier.
use super::*;
pub(crate) use crate::services::tui_input::human_frame::history::v2::native::barrier::Supersession;
use crate::services::tui_o::shadow::capture::Fingerprint;
use crate::services::tui_o::shadow::identity::{self, RecordFact};

/// The canonical classifier answers a strict terminator with one unnamed Idle and nothing else.
pub(super) fn strict(facts: &[RecordFact]) -> bool {
    matches!(facts, [RecordFact::Idle(None)])
}

/// The hashes the earlier Prompt reading stored as a barrier's foreign row.
pub(super) fn interrupt_hashes(value: &Value) -> Option<Vec<String>> {
    match classify(value)? {
        QueueRecord::Prompt(body) => Some(frame_hashes(&body)),
        _ => None,
    }
}

/// One strict interrupt row of the authenticated prefix: range, uuid and body hashes.
pub(super) type Interrupt<'a> = ((u64, u64), &'a str, &'a [String]);

/// Whether the stored barrier's foreign row is one of the prefix's strict interrupt rows.
pub(super) fn superseding(registration: &Registration, interrupts: &[Interrupt<'_>]) -> bool {
    registration.barrier.as_ref().is_some_and(|barrier| {
        interrupts.iter().any(|(range, row, hashes)| {
            barrier.foreign_range == [range.0, range.1]
                && barrier.foreign_row == *row
                && barrier.foreign_hashes == *hashes
        })
    })
}

/// A superseding registration yields only the Cancelled result its stored barrier names.
pub(super) fn seal(registration: &Registration, mut evidence: Evidence) -> Option<Evidence> {
    if registration.candidate {
        return Some(evidence);
    }
    let barrier = registration.barrier.as_ref()?;
    let pair = |range: [u64; 2]| Some((range[0], range[1]));
    if evidence.disposition != Disposition::Cancelled
        || evidence.membership_range != pair(barrier.membership_range)
        || evidence.membership_row.as_deref() != Some(barrier.membership_row.as_str())
        || evidence.closer_range != pair(barrier.foreign_range)
    {
        return None;
    }
    evidence.supersedes = Some(Supersession {
        barrier_digest: barrier.digest().ok()?,
        interrupt_row: barrier.foreign_row.clone(),
    });
    Some(evidence)
}

// Receipt identities outlive execution candidacy; unresolved ownership blocks a final decision.
pub(super) fn foreign_prompt(
    registrations: &[Registration],
    (binding, proven): (&SourceBinding, Option<(u64, u64)>),
    owner: &Registration,
    (membership, member_row): ((u64, u64), Option<&str>),
    (range, row, hashes): ((u64, u64), Option<&str>, &[String]),
) -> Result<Option<Barrier>, ()> {
    let Some(row) = row else { return Err(()) };
    let Some(member_row) = member_row else {
        return Err(());
    };
    let target = &owner.ticket.source.as_ref().ok_or(())?.target;
    let same_execution = |r: &&Registration| {
        r.ticket.channel_id == binding.channel_id && r.ticket.key["attempt"]["target"] == *target
    };
    if registrations.iter().filter(same_execution).any(|r| {
        r.matches(binding, proven)
            && r.receipts.iter().any(|known| known == row)
            && hashes.contains(&r.frame_hash)
    }) {
        return Ok(None);
    }
    if registrations.iter().filter(same_execution).any(|r| {
        (r.ticket.source.is_none() || r.receipts.is_empty()) && hashes.contains(&r.frame_hash)
    }) {
        return Err(());
    }
    Ok(Some(Barrier {
        namespace: owner.namespace.clone(),
        ticket: owner.ticket.clone(),
        membership_range: [membership.0, membership.1],
        membership_row: member_row.into(),
        foreign_range: [range.0, range.1],
        foreign_row: row.into(),
        foreign_hashes: hashes.to_vec(),
        through: range.1,
        registrations_fingerprint: String::new(),
    }))
}

/// The complete cursor an EOF proof starts from and the unterminated bytes buffered after it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Tail {
    pub(crate) through: u64,
    pub(crate) prefix_hash: String,
    pub(crate) fingerprint: Fingerprint,
    pub(crate) digest: String,
}

/// One strict closer read whole at the EOF of an ended execution; the capture cursor stays put.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawWitness {
    pub(crate) version: u32,
    pub(crate) tail: Tail,
    pub(crate) aborted: bool,
    pub(crate) execution_nonce: String,
    pub(crate) registrations_fingerprint: String,
}

impl RawWitness {
    pub(crate) fn range(&self) -> (u64, u64) {
        (self.tail.through, self.tail.fingerprint.len)
    }

    /// Whether this is the closer of `evidence`, for the same file and the same execution.
    pub(crate) fn seals(&self, evidence: &Evidence) -> bool {
        let kind = match self.aborted {
            true => Disposition::Cancelled,
            false => Disposition::Executed,
        };
        self.version == 1
            && self.tail.through < self.tail.fingerprint.len
            && self.tail.digest.len() == 64
            && evidence.closer_range == Some(self.range())
            && evidence.disposition == kind
            && evidence.ticket.source.as_ref().is_some_and(|source| {
                source.file[1] == self.tail.fingerprint.ino
                    && source.target["execution_nonce"] == self.execution_nonce
            })
    }
}

/// The parent-source gates a native record passes before it is classified.
pub(super) fn gate(binding: &SourceBinding, value: &Value) -> Result<(), String> {
    let path = &binding.source.path;
    if value["isSidechain"] == true || path.components().any(|p| p.as_os_str() == "subagents") {
        return Err("native prefix requires a parent source".into());
    }
    let session = value["sessionId"].as_str();
    if session.is_some_and(|s| s != binding.source.session_id) {
        return Err("native record belongs to another source".into());
    }
    Ok(())
}

/// Whether an unterminated tail is one whole strict closer of this source: `Ok(aborted)`.
pub(crate) fn raw_closer(binding: &SourceBinding, bytes: &[u8]) -> Result<bool, String> {
    let whole = binding.provider == ShadowProvider::Claude
        && bytes.first() == Some(&b'{')
        && bytes.last() == Some(&b'}')
        && !bytes.contains(&b'\n');
    let value: Value = whole
        .then(|| serde_json::from_slice(bytes).ok())
        .flatten()
        .ok_or("native tail is not one whole record")?;
    gate(binding, &value)?;
    if !strict(&identity::classify(binding.provider, &value)) {
        return Err("native tail is not a strict closer".into());
    }
    Ok(value["type"] == "user")
}

/// Whether the evidence ends inside what the observation authenticated.
pub(crate) fn within(observed: &Observation, evidence: &Evidence) -> bool {
    match &evidence.raw {
        None => evidence.range.1 <= observed.through,
        Some(raw) => {
            observed.terminal.as_ref() == Some(raw)
                && raw.tail.through == observed.through
                && raw.registrations_fingerprint == observed.registrations_fingerprint
                && raw.seals(evidence)
        }
    }
}

/// Marks the results the raw closer ended.
pub(super) fn attach(evidence: &mut [Evidence], raw: &RawWitness) {
    for closed in evidence {
        if closed.closer_range == Some(raw.range()) {
            closed.raw = Some(raw.clone());
        }
    }
}
