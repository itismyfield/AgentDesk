//! A readiness hold gives the owner's unattempted front one fixed deadline; there the owner refuses
//! it without an attempt unless a reserve won first. Both are actor turns, so exactly one wins.
use super::*;
use crate::db::turns::TerminalOutcome;

/// The original's identity and body digest, as a hold names it.
fn hold_key(item: &PendingQueueItem) -> (QueueIdentity, String) {
    let member = refused::refused_member(item);
    (member.identity, member.fingerprint)
}

impl Owner {
    /// Records the first hold of the unattempted `front`; a later hold of the same original returns
    /// the stored one, so its due never moves.
    pub(crate) fn hold_readiness(
        &mut self,
        front: &Front,
        cause: &str,
        now: u64,
    ) -> Result<ReadinessHold, String> {
        if !self.front().is_some_and(|f| is_front(&f, front)) {
            return Err("not the owner's unattempted front".into());
        }
        if let Some(hold) = self.front_hold() {
            return Ok(hold.clone());
        }
        // How long a readiness hold keeps an unstarted front before the owner refuses it.
        const READINESS_HOLD_MS: u64 = 10_000;
        let (identity, fingerprint) = hold_key(&front.1);
        let hold = ReadinessHold {
            identity,
            fingerprint,
            cause: cause.into(),
            started_at_ms: now,
            due_at_ms: now.saturating_add(READINESS_HOLD_MS),
        };
        let mut next = self.snapshot.clone();
        next.readiness_holds.push(hold.clone());
        self.commit(next)?;
        Ok(hold)
    }

    /// The hold of the queue front while no attempt runs; an attempted front is its leaf's.
    pub(crate) fn front_hold(&self) -> Option<&ReadinessHold> {
        if self.snapshot.head_injection.is_some() {
            return None;
        }
        let (identity, fingerprint) = hold_key(self.snapshot.items.first()?);
        (self.snapshot.readiness_holds.iter())
            .find(|hold| hold.identity == identity && hold.fingerprint == fingerprint)
    }

    /// Refuses the expired front `expected` holds in one commit: its unit leaves, origins fail,
    /// and only sources, fingerprints and one notice stay. `Ok(false)` when no longer that front.
    pub(crate) fn refuse_readiness_front(
        &mut self,
        expected: &ReadinessHold,
        now: u64,
    ) -> Result<bool, String> {
        if self.front_hold() != Some(expected) || !expected.expired(now) {
            return Ok(false);
        }
        // The unit a claim or grant would have sent: the front and the followers it carries.
        let unit: Vec<PendingQueueItem> = std::iter::once(&self.snapshot.items[0])
            .chain(self.followers().iter())
            .zip(&self.snapshot.items)
            .take_while(|(a, b)| same(a, b))
            .map(|(_, item)| item.clone())
            .collect();
        let mut next = self.snapshot.clone();
        next.owe_queued(0..unit.len(), TerminalOutcome::Failed, now);
        for item in &unit {
            next.items.remove(0);
            next.unbind_item(0);
            next.handoff_input_left(&sources::identity(item));
        }
        next.cancel_refused_continuation(&unit);
        next.readiness_holds.retain(|hold| hold != expected);
        next.refused_fronts.push(RefusedFront {
            input: FrontRef {
                identity: expected.identity.clone(),
            },
            members: unit.iter().map(refused::refused_member).collect(),
            reason: format!("readiness_deadline: {}", expected.cause),
            notice: NoticeIntent {
                kind: refused::READINESS_UNPROVEN.into(),
                enqueued: false,
            },
            refused_at_ms: now,
        });
        self.commit(next)?;
        // The withdrawn grant or claim can no longer reserve, so a late writer sends no key.
        self.revoke_front();
        self.held = None;
        Ok(true)
    }

    /// Refuses the current front if its hold expired; `tick` and the boot recovery call it before
    /// any drive looks at the front.
    pub(super) fn refuse_expired_front(&mut self, now: u64) -> Result<(), String> {
        match self.front_hold().filter(|hold| hold.expired(now)).cloned() {
            Some(hold) => self.refuse_readiness_front(&hold, now).map(drop),
            None => Ok(()),
        }
    }

    /// Whether the attempted head's original reached its readiness due; its first key then never
    /// starts and the leaf settles it through its own not-sent proof.
    pub(crate) fn head_outlived_due(&self, now: u64) -> bool {
        let Some(head) = &self.snapshot.head_injection else {
            return false;
        };
        let (identity, fingerprint) = hold_key(&head.original);
        (self.snapshot.readiness_holds.iter()).any(|hold| {
            hold.identity == identity && hold.fingerprint == fingerprint && now >= hold.due_at_ms
        })
    }

    /// Marks an attemptless front refusal's notice sent; `None` when none names `identity`.
    pub(super) fn front_refusal_told(
        &mut self,
        identity: &QueueIdentity,
    ) -> Option<Result<(), String>> {
        let mut next = self.snapshot.clone();
        let entry = (next.refused_fronts.iter_mut()).find(|r| r.input.identity == *identity)?;
        if entry.notice.enqueued {
            return Some(Ok(()));
        }
        entry.notice.enqueued = true;
        Some(self.commit(next))
    }
}

impl SnapshotV2 {
    /// Drops holds whose original the owner no longer queues, attempts or retains.
    pub(super) fn prune_readiness_holds(&mut self) {
        let attempts =
            (self.head_injection.iter()).chain(self.retained_inputs.iter().map(|r| &r.input));
        let live: Vec<QueueIdentity> = (self.items.iter())
            .chain(attempts.flat_map(InputAttempt::members))
            .map(sources::identity)
            .collect();
        self.readiness_holds
            .retain(|hold| live.contains(&hold.identity));
    }
}

#[cfg(all(test, unix))]
#[path = "readiness_hold_tests.rs"]
mod tests;
