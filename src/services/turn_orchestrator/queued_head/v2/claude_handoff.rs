//! A forward handoff owns no second input copy and never selects the old composer.
use super::*;

use super::ClaudeHandoffJournal as Journal;
#[cfg(unix)]
pub(crate) const MAX_START_FAILURES: u32 = 5;
/// Why a front is held under a journal: how it waits, and what its last hold leaves of the journal.
#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Held {
    /// The channel left Herdr: the retained front takes the journal with it.
    Left,
    /// A refusal no later look clears, behind a persisted deadline.
    Refused,
    /// A failed close of the old pane, retried on the drive's own timer.
    Close,
}
impl Journal {
    /// The budget `front` has spent; another front's count is never inherited.
    #[cfg(unix)]
    pub(super) fn spent(&self, front: &QueueIdentity) -> u32 {
        let spender = self.spender.as_ref().unwrap_or(&self.original);
        if spender == front { self.failures } else { 0 }
    }
    #[cfg(unix)]
    fn charge(&mut self, front: QueueIdentity, failures: u32, retry_after_ms: u64) {
        (self.failures, self.retry_after_ms) = (failures, retry_after_ms);
        self.spender = (front != self.original).then_some(front);
    }
}
impl SnapshotV2 {
    /// The waiting inputs left unsent: an ended journal has nothing left to carry and goes; an
    /// unended one still owes its old pane's end, and keeps its id and pin without a budget.
    pub(super) fn handoff_fronts_left(&mut self) {
        match &mut self.unfinished_handoff {
            Some(journal) if !journal.ended => {
                (journal.failures, journal.retry_after_ms, journal.spender) = (0, 0, None);
            }
            journal => *journal = None,
        }
    }
    /// One input left unsent: only the journal's original or the front spending its budget
    /// changes the journal, and an ended one goes only once its original is no longer queued.
    pub(super) fn handoff_input_left(&mut self, gone: &QueueIdentity) {
        let Some(journal) = &self.unfinished_handoff else {
            return;
        };
        if journal.original != *gone && journal.spender.as_ref() != Some(gone) {
            return;
        }
        let queued = (self.items.iter()).any(|item| sources::identity(item) == journal.original);
        if journal.ended && queued {
            let journal = self.unfinished_handoff.as_mut().expect("checked journal");
            (journal.failures, journal.retry_after_ms, journal.spender) = (0, 0, None);
        } else {
            self.handoff_fronts_left();
        }
    }
}
impl Owner {
    pub(crate) fn handoff(&self) -> Option<Journal> {
        self.snapshot.unfinished_handoff.clone()
    }
    /// The grant and current front are checked in the actor, never from an intake's stale probe.
    pub(crate) fn begin_handoff(
        &mut self,
        front: &Front,
        logical: String,
        session_key: String,
    ) -> Result<Journal, String> {
        if !self
            .front()
            .is_some_and(|current| is_front(&current, front))
            || !self
                .front_grant
                .as_ref()
                .is_some_and(|grant| is_front(grant, front))
        {
            return Err("handoff front no longer granted".into());
        }
        if let Some(journal) = self.handoff() {
            if journal.logical != logical || journal.session_key != session_key {
                return Err("handoff target changed".into());
            }
            return Ok(journal);
        }
        let journal = Journal {
            id: uuid::Uuid::new_v4().to_string(),
            logical,
            session_key,
            original: sources::identity(&front.1),
            pin: None,
            ended: false,
            failures: 0,
            notice_acked: false,
            retry_after_ms: 0,
            spender: None,
        };
        let mut next = self.snapshot.clone();
        next.unfinished_handoff = Some(journal.clone());
        self.commit(next)?;
        Ok(journal)
    }
    pub(crate) fn handoff_pin(
        &mut self,
        id: &str,
        front: &Front,
        pin: serde_json::Value,
    ) -> Result<bool, String> {
        if !self
            .front()
            .is_some_and(|current| is_front(&current, front))
        {
            return Ok(false);
        }
        let mut next = self.snapshot.clone();
        let Some(journal) = next
            .unfinished_handoff
            .as_mut()
            .filter(|j| j.id == id && !j.ended)
        else {
            return Ok(false);
        };
        if journal.pin.as_ref().is_some_and(|old| old != &pin) {
            return Err("handoff pane changed".into());
        }
        journal.pin = Some(pin);
        self.commit(next)?;
        Ok(true)
    }
    pub(crate) fn handoff_ended(&mut self, id: &str) -> Result<bool, String> {
        let mut next = self.snapshot.clone();
        let Some(journal) = next.unfinished_handoff.as_mut().filter(|j| j.id == id) else {
            return Ok(false);
        };
        journal.ended = true;
        self.commit(next)?;
        Ok(true)
    }
    pub(crate) fn handoff_notice_acked(&mut self, id: &str) -> Result<(), String> {
        let mut next = self.snapshot.clone();
        if let Some(journal) = next.unfinished_handoff.as_mut().filter(|j| j.id == id) {
            journal.notice_acked = true;
            self.commit(next)?;
        }
        Ok(())
    }
    /// Drops the journal `id` once its original is no longer queued; whether no journal is left.
    pub(crate) fn finish_handoff(&mut self, id: &str) -> Result<bool, String> {
        let mut next = self.snapshot.clone();
        let Some(journal) = &next.unfinished_handoff else {
            return Ok(true);
        };
        let queued = (next.items.iter()).any(|item| sources::identity(item) == journal.original);
        if journal.id != id || queued {
            return Ok(false);
        }
        next.unfinished_handoff = None;
        self.commit(next)?;
        Ok(true)
    }
    /// A channel that left Herdr keeps no journal whose old pane ended or whose original is gone.
    #[cfg(unix)]
    pub(crate) fn abandon_handoff(&mut self, id: &str) -> Result<bool, String> {
        let mut next = self.snapshot.clone();
        let queued =
            |j: &Journal| (next.items.iter()).any(|item| sources::identity(item) == j.original);
        if !(next.unfinished_handoff.as_ref())
            .is_some_and(|j| j.id == id && (j.ended || !queued(j)))
        {
            return Ok(false);
        }
        next.unfinished_handoff = None;
        self.commit(next)?;
        Ok(true)
    }
    /// Retains the granted front as an opaque start failure; `false` while its start budget lasts.
    #[cfg(unix)]
    fn retain_held(&mut self, front: &Front, detail: &str) -> Result<bool, String> {
        if self.claim_turn(front.1.message_id, front).is_none() {
            return Err("handoff front no longer granted".into());
        }
        let (number, member) = (self.claim_number, std::slice::from_ref(&front.1));
        let kind = StartFailureKind::Opaque;
        match self.start_failed(front, number, member, None, kind, detail) {
            Ok(Some(retained)) => Ok(retained),
            failed => {
                self.revoke_front();
                Err(failed.err().unwrap_or("handoff front not retained".into()))
            }
        }
    }
    /// A hold the journal's own target caused spends the front's budget; its last one retains the
    /// granted front as a start failure, `None`, instead of holding it again.
    #[cfg(unix)]
    pub(crate) fn handoff_held(
        &mut self,
        front: &Front,
        id: &str,
        how: Held,
    ) -> Result<Option<Journal>, String> {
        let Some(journal) = self.handoff().filter(|j| j.id == id) else {
            return Err("handoff journal changed".into());
        };
        let me = sources::identity(&front.1);
        let failures = journal.spent(&me).saturating_add(1);
        if failures >= MAX_START_FAILURES {
            if !self.retain_held(front, "claude_handoff_held")? {
                return Err("handoff front not retained".into());
            }
            if how == Held::Left {
                // A failed drop is repeated by the next look of the channel.
                self.abandon_handoff(id).ok();
            }
            return Ok(None);
        }
        self.hold_front(front)?;
        let mut next = self.snapshot.clone();
        let journal = next.unfinished_handoff.as_mut().expect("checked journal");
        let wait = 1_000u64 << failures.min(5);
        let deadline = match how {
            Held::Close => 0,
            _ => origin::now_ms().saturating_add(wait),
        };
        journal.charge(me, failures, deadline);
        let journal = journal.clone();
        self.commit(next)?;
        Ok(Some(journal))
    }
    /// A front no journal covers whose host cannot be judged spends the ordinary start budget:
    /// held once, `false`, then retained.
    #[cfg(unix)]
    pub(crate) fn unjudged_held(&mut self, front: &Front) -> Result<bool, String> {
        if self.retain_held(front, "claude_handoff_unjudged")? {
            return Ok(true);
        }
        self.hold_front(front)?;
        Ok(false)
    }
    /// Persist the retry budget before releasing a failed wrapper; exhaustion uses start_failed.
    #[cfg(unix)]
    pub(crate) fn handoff_start_failed(
        &mut self,
        front: &Front,
        number: u64,
        pin: Option<serde_json::Value>,
    ) -> Result<bool, String> {
        if self.handoff().is_none()
            || self.snapshot.head_injection.is_some()
            || self.claim_number != number
            || !self.claimed(front.1.message_id, front)
        {
            return Ok(false);
        }
        let me = sources::identity(&front.1);
        let journal = self.handoff().expect("checked journal");
        let failures = journal.spent(&me).saturating_add(1);
        if failures >= MAX_START_FAILURES {
            return Ok(false);
        }
        let mut next = self.snapshot.clone();
        let journal = next.unfinished_handoff.as_mut().expect("checked journal");
        let deadline = origin::now_ms().saturating_add(1_000u64 << failures.min(5));
        journal.charge(me, failures, deadline);
        self.commit(next)?;
        self.revoke_front();
        self.held = Some(front.1.message_id);
        self.starts.closing = pin;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn queued() -> (
        crate::config::TestRuntimeRootGuard,
        tempfile::TempDir,
        Owner,
        Front,
    ) {
        let root_guard = crate::config::TestRuntimeRootGuard::new();
        let parent = crate::config::runtime_root()
            .unwrap()
            .join("runtime/discord_pending_queue/handoff");
        std::fs::create_dir_all(&parent).unwrap();
        let dir = tempfile::tempdir_in(parent).unwrap();
        let mut owner = Owner::adopt(dir.path().join("queue.json"), "handoff".into()).unwrap();
        let item: PendingQueueItem = serde_json::from_value(serde_json::json!({"author_id":7,"message_id":8077,"channel_id":8077,"source_message_ids":[8077],"queued_generation":1,"text":"one original"})).unwrap();
        owner.enqueue_fixture(&item).unwrap();
        let front = owner.front().unwrap();
        owner.grant_front(&front).unwrap();
        (root_guard, dir, owner, front)
    }
    #[cfg(unix)]
    #[test]
    fn close_failures_within_the_budget_keep_one_original_and_no_retained() {
        let (root_guard, dir, mut owner, front) = queued();
        let journal = owner
            .begin_handoff(&front, "owned".into(), "claude/t/h:owned".into())
            .unwrap();
        for _ in 0..4 {
            let held = owner.handoff_held(&front, &journal.id, Held::Close);
            assert_eq!(held.unwrap().unwrap().retry_after_ms, 0);
            owner.grant_front(&front).unwrap();
        }
        assert_eq!(owner.snapshot.items.len(), 1);
        assert!(owner.snapshot.retained_inputs.is_empty());
        assert_eq!(owner.handoff().unwrap().failures, 4);
        assert!(!owner.handoff().unwrap().notice_acked);
        owner.handoff_notice_acked(&journal.id).unwrap();
        let restored = Owner::adopt(dir.path().join("queue.json"), "handoff".into()).unwrap();
        assert!(restored.handoff().unwrap().notice_acked);
        assert_eq!(restored.snapshot.items[0].message_id, 8077);
        drop(root_guard);
    }
    #[test]
    fn stale_front_or_cancelled_journal_never_authorizes_close() {
        let (root_guard, dir, mut owner, front) = queued();
        let journal = owner
            .begin_handoff(&front, "owned".into(), "key".into())
            .unwrap();
        owner.clear_waiting().unwrap();
        assert!(
            !owner
                .handoff_pin(&journal.id, &front, serde_json::json!({"pane":"%0"}))
                .unwrap()
        );
        let mut changed = front.clone();
        changed.1.queued_generation += 1;
        assert!(
            owner
                .begin_handoff(&changed, "owned".into(), "key".into())
                .is_err()
        );
        assert!(dir.path().join("queue.json").exists());
        drop(root_guard);
    }
    #[test]
    fn marker_absent_begin_is_durable_and_second_probe_reuses_it() {
        let (root_guard, dir, mut owner, front) = queued();
        let first = owner
            .begin_handoff(&front, "owned".into(), "key".into())
            .unwrap();
        let second = owner
            .begin_handoff(&front, "owned".into(), "key".into())
            .unwrap();
        assert_eq!(first.id, second.id);
        assert!(crate::config::runtime_root().is_some());

        assert_eq!(
            Owner::adopt(dir.path().join("queue.json"), "handoff".into())
                .unwrap()
                .handoff()
                .unwrap()
                .id,
            first.id
        );
        drop(root_guard);
    }
    #[cfg(unix)]
    #[test]
    fn ended_handoff_launch_failure_is_bounded_and_retained_once() {
        let (_root, dir, mut owner, front) = queued();
        let journal = owner
            .begin_handoff(&front, "owned".into(), "key".into())
            .unwrap();
        owner.handoff_ended(&journal.id).unwrap();
        let mut following = front.1.clone();
        following.message_id += 1;
        following.source_message_ids = vec![following.message_id];
        following.author_id += 1;
        owner.enqueue_fixture(&following).unwrap();
        let mut tries = 0;
        for _ in 0..6 {
            let front = owner.front().unwrap();
            owner.grant_front(&front).unwrap();
            owner.claim_turn(front.1.message_id, &front).unwrap();
            let number = owner.claim_number();
            tries += 1;
            let retained = if owner.handoff_start_failed(&front, number, None).unwrap() {
                false
            } else {
                owner
                    .start_failed(
                        &front,
                        number,
                        std::slice::from_ref(&front.1),
                        None,
                        StartFailureKind::Opaque,
                        "launch auth_overlay: forced failure",
                    )
                    .unwrap()
                    == Some(true)
            };
            if retained {
                assert_eq!(
                    owner
                        .start_failed(
                            &front,
                            number,
                            std::slice::from_ref(&front.1),
                            None,
                            StartFailureKind::Opaque,
                            "late duplicate"
                        )
                        .unwrap(),
                    None
                );
                break;
            }
            drop(owner);
            owner = Owner::adopt(dir.path().join("queue.json"), "handoff".into()).unwrap();
        }
        eprintln!(
            "launch attempts={tries} journal_failures={:?} retained={}",
            owner.handoff().map(|j| j.failures),
            owner.snapshot.retained_inputs.len()
        );
        assert!(tries <= 5, "ended journal must not retry indefinitely");
        assert!(owner.handoff().is_none());
        assert_eq!(owner.snapshot.retained_inputs.len(), 1);
        assert_eq!(
            owner.snapshot.retained_inputs[0].input.original.message_id,
            front.1.message_id
        );
        assert_eq!(owner.front().unwrap().1.message_id, following.message_id);
        assert_eq!(owner.snapshot.items.len(), 1);
    }
    #[cfg(unix)]
    #[test]
    fn exhausted_handoff_retention_publish_failure_preserves_original_until_one_commit() {
        let (_root, dir, mut owner, front) = queued();
        let journal = owner
            .begin_handoff(&front, "owned".into(), "key".into())
            .unwrap();
        owner.handoff_ended(&journal.id).unwrap();
        owner.snapshot.unfinished_handoff.as_mut().unwrap().failures = 4;
        owner.claim_turn(front.1.message_id, &front).unwrap();
        let number = owner.claim_number();
        assert!(!owner.handoff_start_failed(&front, number, None).unwrap());
        let before = owner.json().unwrap();
        owner.fault = Fault::Before;
        assert!(
            owner
                .start_failed(
                    &front,
                    number,
                    std::slice::from_ref(&front.1),
                    None,
                    StartFailureKind::Opaque,
                    "failed launch"
                )
                .is_err()
        );
        assert_eq!(owner.json().unwrap(), before);
        owner.fault = Fault::None;
        assert_eq!(
            owner
                .start_failed(
                    &front,
                    number,
                    std::slice::from_ref(&front.1),
                    None,
                    StartFailureKind::Opaque,
                    "failed launch"
                )
                .unwrap(),
            Some(true)
        );
        let restored = Owner::adopt(dir.path().join("queue.json"), "handoff".into()).unwrap();
        assert_eq!(restored.snapshot.retained_inputs.len(), 1);
        assert!(restored.snapshot.items.is_empty());
        assert!(restored.handoff().is_none());
    }
    #[cfg(unix)]
    #[test]
    fn a_front_never_inherits_the_count_another_front_spent_on_an_ended_journal() {
        let (_root, _dir, mut owner, front) = queued();
        let journal = owner
            .begin_handoff(&front, "owned".into(), "key".into())
            .unwrap();
        owner.handoff_ended(&journal.id).unwrap();
        let mut following = front.1.clone();
        following.message_id += 1;
        following.source_message_ids = vec![following.message_id];
        owner.enqueue_fixture(&following).unwrap();
        for failures in 1..=3 {
            let held = owner.handoff_held(&front, &journal.id, Held::Refused);
            assert_eq!(held.unwrap().unwrap().failures, failures);
            owner.grant_front(&front).unwrap();
        }
        // The first front completes locally and leaves the ended journal to the next one.
        owner.claim_turn(front.1.message_id, &front).unwrap();
        let number = owner.claim_number();
        owner.complete_claimed_locally((&front, number), 0).unwrap();
        let next = owner.front().unwrap();
        owner.grant_front(&next).unwrap();
        let held = owner.handoff_held(&next, &journal.id, Held::Refused);
        assert_eq!(held.unwrap().unwrap().failures, 1);
    }
    #[test]
    fn bootstrap_crash_retains_the_original_once_without_an_unfinished_journal() {
        let (root_guard, dir, mut owner, front) = queued();
        let journal = owner
            .begin_handoff(&front, "owned".into(), "claude/test/node:owned".into())
            .unwrap();
        owner.handoff_ended(&journal.id).unwrap();
        let key = owner
            .reserve(
                AttemptIdentity {
                    nonce: "handoff-crash-window".into(),
                    provider: "claude".into(),
                    target: TargetIdentity {
                        host: "herdr".into(),
                        owner: "owned".into(),
                        root: "/test".into(),
                        execution_nonce: "new-herdr".into(),
                    },
                    source: AttemptSource::PreSource {
                        expected_session_id: None,
                        expected_transcript: None,
                        binding_watermark: None,
                    },
                },
                PreparedPayload {
                    frame: "one original".into(),
                    digest: "fixture".into(),
                    attribution: serde_json::json!({}),
                    followers: Vec::new(),
                },
            )
            .unwrap();
        let permit = owner.arm_bootstrap(&key, 10_000).unwrap();
        drop(permit);
        drop(owner);
        // No provider call ran: restart still cannot infer that from the durable Bootstrap.
        let mut restored = Owner::adopt(dir.path().join("queue.json"), "handoff".into()).unwrap();
        assert_eq!(
            restored.snapshot.head_injection.as_ref().unwrap().phase,
            HeadPhase::Unknown
        );
        restored.tick(10_001).unwrap();
        restored.tick(20_000).unwrap();
        assert_eq!(restored.snapshot.retained_inputs.len(), 1);
        assert_eq!(
            restored.snapshot.retained_inputs[0]
                .input
                .original
                .message_id,
            front.1.message_id
        );
        assert!(restored.snapshot.items.is_empty());
        assert!(restored.snapshot.head_injection.is_none());
        assert!(restored.handoff().is_none());
        assert_eq!(
            Owner::adopt(dir.path().join("queue.json"), "handoff".into())
                .unwrap()
                .snapshot
                .retained_inputs
                .len(),
            1
        );
        drop(root_guard);
    }
    #[test]
    fn bound_completion_keeps_a_journal_while_its_original_is_owed() {
        let (root_guard, dir, mut owner, front) = queued();
        assert!(dir.path().join("queue.json").exists());
        let journal = owner
            .begin_handoff(&front, "owned".into(), "key".into())
            .unwrap();
        owner.handoff_ended(&journal.id).unwrap();
        owner.finish_handoff(&journal.id).unwrap();
        assert!(owner.handoff().is_some());
        owner.snapshot.items.clear();
        owner.finish_handoff(&journal.id).unwrap();
        assert!(owner.handoff().is_none());
        drop(root_guard);
    }
}
