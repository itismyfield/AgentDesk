//! A restart's greeting waits behind the unit already in flight and ahead of every other input.
use super::*;
use crate::db::turns::TerminalOutcome;

pub(crate) const SOURCE: &str = "agentdesk_restart_seed";

/// Where a restart's greeting stands, read from its ticket's receipt and binds alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SeedStatus {
    /// The owner never committed this ticket.
    Absent,
    Queued,
    Attempt,
    Settled,
    /// Its start failed or its delivery is uncertain; the owner's notice says so.
    Retained,
    /// Committed once and no longer held: withdrawn, cleared or expired. Not evidence of a clear.
    Gone,
}

impl ChannelMailboxHandle {
    pub(crate) fn restart_seed_input(context: Option<&serde_json::Value>) -> bool {
        context
            .is_some_and(|context| context["origin"] == "headless" && context["source"] == SOURCE)
    }
}

impl SnapshotV2 {
    fn seed_bind(&self, index: usize) -> Option<&AdmissionBind> {
        self.admission_binds.iter().find(|bind| {
            matches!(bind.at, BindAt::Queued { index: at, .. } if at == index)
                && ChannelMailboxHandle::restart_seed_input(
                    bind.context.as_ref().and_then(|c| c.execution.as_ref()),
                )
        })
    }

    /// The queued greeting, if one waits: at most one is ever queued.
    fn queued_seed(&self) -> Option<usize> {
        (0..self.items.len()).find(|index| self.seed_bind(*index).is_some())
    }
}

impl Owner {
    /// Whether the unattempted front is a restart's greeting, which nothing may overtake.
    pub(in super::super) fn seed_front(&self) -> bool {
        self.snapshot.seed_bind(0).is_some()
    }

    /// The members the owner already sends or a turn already claimed; the greeting follows them.
    fn protected_unit(&self) -> usize {
        match &self.snapshot.head_injection {
            Some(head) => head.members().count(),
            None if self.front_claim.is_some() => 1 + self.claimed_followers.len(),
            None => 0,
        }
    }

    /// A second restart uses the greeting already queued: its ticket gets a receipt and a bind on
    /// that slot in one commit, and no second greeting is admitted.
    pub(super) fn join_seed(&mut self, ticket: &Ticket) -> Result<Option<Admitted>, AdmitError> {
        if self.effort_blocks(None) {
            return Err(AdmitError::NotCommitted("effort restart pending".into()));
        }
        let Some(index) = self.snapshot.queued_seed() else {
            return Ok(None);
        };
        if self.pending.is_some() {
            return Err(AdmitError::NotCommitted("owner recovery required".into()));
        }
        let mut next = self.snapshot.clone();
        let mut alias = next.seed_bind(index).expect("queued seed bind").clone();
        let whole = alias
            .receipt
            .rsplit_once(':')
            .map_or("", |(_, digest)| digest);
        let joined = format!("{}{whole}", ticket.prefix());
        alias.receipt = joined;
        if next.admission_receipts.contains(&alias.receipt) {
            return Ok(Some(Admitted::Duplicate));
        }
        let receipt = alias.receipt.clone();
        next.admission_binds.push(alias);
        self.admit_commit(next, Some(receipt))?;
        Ok(Some(Admitted::Duplicate))
    }

    /// Moves the greeting just pushed at the tail behind the protected unit, with every bind.
    pub(super) fn place_seed(&self, next: &mut SnapshotV2) -> usize {
        let (at, last) = (self.protected_unit(), next.items.len() - 1);
        let at = at.min(last);
        next.items[at..].rotate_right(1);
        for bind in &mut next.admission_binds {
            if let BindAt::Queued { index, .. } = &mut bind.at {
                if *index == last {
                    *index = at;
                } else if *index >= at {
                    *index += 1;
                }
            }
        }
        at
    }

    /// The front right changes hands only when the greeting became the front itself.
    pub(super) fn seed_placed(&mut self, at: usize) {
        if at == 0 {
            self.revoke_front();
            self.held = None;
        }
    }

    pub(crate) fn seed_status(&self, ticket: &Ticket) -> SeedStatus {
        let prefix = ticket.prefix();
        let snapshot = &self.snapshot;
        let Some(receipt) = (snapshot.admission_receipts.iter()).find(|r| r.starts_with(&prefix))
        else {
            return SeedStatus::Absent;
        };
        let binds = || {
            snapshot
                .admission_binds
                .iter()
                .filter(|b| b.receipt == *receipt)
        };
        let live = |attempt: &AttemptIdentity| {
            (snapshot.head_injection.as_ref()).is_some_and(|h| h.attempt.as_ref() == Some(attempt))
        };
        if binds().any(|b| {
            matches!(&b.at, BindAt::Queued { index, item }
            if snapshot.queued_digest(*index).as_ref() == Some(item))
        }) {
            SeedStatus::Queued
        } else if binds().any(|b| matches!(&b.at, BindAt::Attempt { attempt, .. } if live(attempt)))
        {
            SeedStatus::Attempt
        } else if binds().any(|b| matches!(b.at, BindAt::Settled { .. })) {
            SeedStatus::Settled
        } else if binds().any(|b| matches!(b.at, BindAt::Opaque | BindAt::Attempt { .. })) {
            SeedStatus::Retained
        } else {
            SeedStatus::Gone
        }
    }

    /// Takes the still queued greeting out with a cancelled receipt; a started one stays.
    pub(crate) fn withdraw_seed(&mut self, ticket: &Ticket) -> Result<bool, String> {
        if self.pending.is_some() && self.recover().is_err() && self.pending.is_some() {
            return Err("restart greeting withdrawal commit is unconfirmed".into());
        }
        let prefix = ticket.prefix();
        let at = self
            .snapshot
            .admission_binds
            .iter()
            .find_map(|b| match b.at {
                BindAt::Queued { index, .. } if b.receipt.starts_with(&prefix) => Some(index),
                _ => None,
            });
        let Some(at) = at.filter(|at| *at >= self.protected_unit()) else {
            return Ok(false);
        };
        let mut next = self.snapshot.clone();
        next.owe_queued([at], TerminalOutcome::Cancelled, origin::now_ms());
        next.items.remove(at);
        next.unbind_item(at);
        self.commit(next)?;
        self.seed_placed(at);
        Ok(true)
    }
}

impl ChannelMailboxHandle {
    pub(crate) async fn v2_seed_status(&self, ticket: String) -> Result<SeedStatus, String> {
        let ticket = Ticket::new(&ticket)?;
        self.v2_request(move |slot| Ok(slot.as_ref().ok_or("V2 not adopted")?.seed_status(&ticket)))
            .await
    }

    pub(crate) async fn v2_withdraw_seed(&self, ticket: String) -> Result<bool, String> {
        let ticket = Ticket::new(&ticket)?;
        self.v2_request(move |slot| {
            slot.as_mut()
                .ok_or("V2 not adopted")?
                .withdraw_seed(&ticket)
        })
        .await
    }
}

#[cfg(all(test, unix))]
#[path = "restart_seed_tests.rs"]
mod tests;
