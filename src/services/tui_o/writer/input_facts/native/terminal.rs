//! Strict terminal rows close their run; they never read as a prompt, receipt or foreign barrier.
use super::*;
pub(crate) use crate::services::tui_input::human_frame::history::v2::native::barrier::Supersession;
use crate::services::tui_o::shadow::identity::RecordFact;

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
    binding: &SourceBinding,
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
        r.matches(binding)
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
