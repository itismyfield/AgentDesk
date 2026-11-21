//! A readiness hold gives the owner's unattempted front one fixed deadline; there the owner refuses
//! it without an attempt unless a reserve won first. Both are actor turns, so exactly one wins.
use super::*;
use crate::db::turns::TerminalOutcome;

/// How long a held front waits for a ready read before the owner refuses it unsent.
const READINESS_HOLD_MS: u64 = 10_000;

impl ReadinessHold {
    /// Matches only the surviving prefix of the first held unit, excluding its front.
    pub(super) fn matching_followers<'a>(
        &'a self,
        items: impl Iterator<Item = &'a PendingQueueItem>,
    ) -> impl Iterator<Item = &'a PendingQueueItem> {
        items
            .zip(self.members.iter().flatten().skip(1))
            .take_while(|(item, saved)| refused::refused_member(item) == **saved)
            .map(|(item, _)| item)
    }
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
        if let Some(hold) = self
            .snapshot
            .hold_for_member(&refused::refused_member(&front.1))
        {
            return Ok(hold.clone());
        }
        let mut next = self.snapshot.clone();
        let followers = self.followers();
        let members = std::iter::once(&front.1).chain(followers.iter());
        let hold = next.remember_readiness_hold(members, cause, now);
        self.commit(next)?;
        Ok(hold)
    }

    /// Holds the queued original while its pane identity is re-read, under the same fixed
    /// deadline a readiness hold has, counted from its first identity hold. An earlier readiness
    /// due is replaced, so it cannot end the re-read early; a later identity hold keeps the due.
    #[cfg(unix)]
    pub(crate) fn hold_identity(
        &mut self,
        front: &Front,
        cause: &str,
        now: u64,
    ) -> Result<ReadinessHold, String> {
        self.hold_front(front)?;
        let member = refused::refused_member(&front.1);
        let held = self.snapshot.hold_for_member(&member);
        if let Some(hold) = held.filter(|hold| hold.identity_held_at_ms.is_some()) {
            return Ok(hold.clone());
        }
        let mut next = self.snapshot.clone();
        if let Some(old) = held {
            next.readiness_holds.retain(|saved| saved != old);
        }
        let followers = self.followers();
        let members = std::iter::once(&front.1).chain(followers.iter());
        next.remember_readiness_hold(members, cause, now);
        let hold = next
            .readiness_holds
            .last_mut()
            .ok_or("identity hold not recorded")?;
        hold.identity_held_at_ms = Some(now);
        let hold = hold.clone();
        self.commit(next)?;
        Ok(hold)
    }

    /// Resolving identity lets any subsequent composer hold record its own deadline.
    #[cfg(unix)]
    pub(crate) fn finish_identity_hold(&mut self, front: &Front) -> Result<(), String> {
        let member = refused::refused_member(&front.1);
        let Some(hold) = self.snapshot.hold_for_member(&member) else {
            return Ok(());
        };
        if hold.identity_held_at_ms.is_none() {
            return Ok(());
        }
        if !self
            .front()
            .is_some_and(|current| is_front(&current, front))
        {
            return Err("not the owner's unattempted front".into());
        }
        let mut next = self.snapshot.clone();
        next.readiness_holds.retain(|saved| saved != hold);
        self.commit(next)
    }

    /// The timed hold of an unattempted front, an identity hold included.
    pub(crate) fn front_hold(&self) -> Option<&ReadinessHold> {
        if self.snapshot.head_injection.is_some() {
            return None;
        }
        self.snapshot.readiness_hold(self.snapshot.items.first()?)
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
        let unit: Vec<PendingQueueItem> = std::iter::once(&self.snapshot.items[0])
            .chain(expected.matching_followers(self.snapshot.items.iter().skip(1)))
            .cloned()
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
    pub(super) fn head_outlived_due(&self, now: u64) -> bool {
        let Some(head) = &self.snapshot.head_injection else {
            return false;
        };
        self.snapshot
            .readiness_hold(&head.original)
            .is_some_and(|hold| now >= hold.due_at_ms)
    }

    /// The readiness due of `key`'s original while `key` is the attempted head.
    pub(crate) fn attempt_readiness_hold(&self, key: &AttemptRef) -> Option<&ReadinessHold> {
        let head = self
            .snapshot
            .head_injection
            .as_ref()
            .filter(|head| key.matches(head))?;
        self.snapshot.readiness_hold(&head.original)
    }

    /// Approves `key`'s first key (C-s included) unless its original's readiness due came first;
    /// the clock is read only when that original has a due.
    pub(crate) fn approve_first_key(
        &mut self,
        key: &AttemptRef,
        now: &dyn Fn() -> u64,
    ) -> Result<(), String> {
        let Some(hold) = self.attempt_readiness_hold(key) else {
            return Ok(());
        };
        if self.first_key_approved.as_ref() == Some(key) {
            return Ok(());
        }
        if now() >= hold.due_at_ms {
            self.due_refused = Some(key.clone());
            return Err("readiness_deadline".into());
        }
        self.first_key_approved = Some(key.clone());
        Ok(())
    }

    /// Whether `key`'s leaf, ended with no key, outlived its original's due: refused at its first
    /// key, or past the due when it gave up without reaching one.
    pub(crate) fn leaf_outlived_due(&mut self, key: &AttemptRef, now: &dyn Fn() -> u64) -> bool {
        let refused = self.due_refused.take_if(|refused| refused == key).is_some();
        refused
            || self
                .attempt_readiness_hold(key)
                .is_some_and(|hold| now() >= hold.due_at_ms)
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
    pub(super) fn readiness_hold(&self, original: &PendingQueueItem) -> Option<&ReadinessHold> {
        self.hold_for_member(&refused::refused_member(original))
    }

    fn hold_for_member(&self, member: &RefusedMember) -> Option<&ReadinessHold> {
        self.readiness_holds
            .iter()
            .find(|hold| hold.identity == member.identity && hold.fingerprint == member.fingerprint)
    }

    /// A retry keeps the first deadline and the first unit it covers.
    pub(super) fn remember_readiness_hold<'a>(
        &mut self,
        members: impl Iterator<Item = &'a PendingQueueItem>,
        cause: &str,
        now: u64,
    ) -> ReadinessHold {
        let mut members = members.map(refused::refused_member);
        let original = members.next().expect("held unit has a front");
        if let Some(hold) = self.hold_for_member(&original) {
            return hold.clone();
        }
        let hold = ReadinessHold {
            identity: original.identity.clone(),
            fingerprint: original.fingerprint.clone(),
            members: Some(std::iter::once(original).chain(members).collect()),
            cause: cause.into(),
            started_at_ms: now,
            due_at_ms: now.saturating_add(READINESS_HOLD_MS),
            identity_held_at_ms: None,
        };
        self.readiness_holds.push(hold.clone());
        hold
    }

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
