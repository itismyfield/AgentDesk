//! A forward handoff owns no second input copy and never selects the old composer.
use super::*;

use super::ClaudeHandoffJournal as Journal;
#[cfg(unix)]
pub(crate) const MAX_START_FAILURES: u32 = 5;
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
    pub(crate) fn handoff_failed(&mut self, id: &str) -> Result<Option<Journal>, String> {
        let mut next = self.snapshot.clone();
        let Some(journal) = next.unfinished_handoff.as_mut().filter(|j| j.id == id) else {
            return Ok(None);
        };
        journal.failures = journal.failures.saturating_add(1);
        let journal = journal.clone();
        self.commit(next)?;
        Ok(Some(journal))
    }
    pub(crate) fn handoff_notice_acked(&mut self, id: &str) -> Result<(), String> {
        let mut next = self.snapshot.clone();
        if let Some(journal) = next.unfinished_handoff.as_mut().filter(|j| j.id == id) {
            journal.notice_acked = true;
            self.commit(next)?;
        }
        Ok(())
    }
    pub(crate) fn finish_handoff(&mut self, id: &str) -> Result<(), String> {
        let mut next = self.snapshot.clone();
        if next.unfinished_handoff.as_ref().is_some_and(|j| {
            j.id == id
                && !next
                    .items
                    .iter()
                    .any(|item| sources::identity(item) == j.original)
        }) {
            next.unfinished_handoff = None;
            self.commit(next)?;
        }
        Ok(())
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
    /// A hold the journal's own target caused spends the launch budget and its backoff; the last
    /// one retains the granted front as a start failure, `None`, instead of holding it again.
    #[cfg(unix)]
    pub(crate) fn handoff_held(
        &mut self,
        front: &Front,
        id: &str,
    ) -> Result<Option<Journal>, String> {
        let Some(journal) = self.handoff().filter(|j| j.id == id) else {
            return Err("handoff journal changed".into());
        };
        let failures = journal.failures.saturating_add(1);
        if failures >= MAX_START_FAILURES {
            if self.claim_turn(front.1.message_id, front).is_none() {
                return Err("handoff front no longer granted".into());
            }
            let (number, member) = (self.claim_number, std::slice::from_ref(&front.1));
            let kind = StartFailureKind::Opaque;
            let retained =
                self.start_failed(front, number, member, None, kind, "claude_handoff_held");
            if !matches!(retained, Ok(Some(true))) {
                self.revoke_front();
                return Err(retained
                    .err()
                    .unwrap_or("handoff front not retained".into()));
            }
            return Ok(None);
        }
        self.hold_front(front)?;
        let mut next = self.snapshot.clone();
        let journal = next.unfinished_handoff.as_mut().expect("checked journal");
        journal.failures = failures;
        journal.retry_after_ms = origin::now_ms().saturating_add(1_000u64 << failures.min(5));
        let journal = journal.clone();
        self.commit(next)?;
        Ok(Some(journal))
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
        let failures = self
            .handoff()
            .expect("checked journal")
            .failures
            .saturating_add(1);
        if failures >= MAX_START_FAILURES {
            return Ok(false);
        }
        let mut next = self.snapshot.clone();
        let journal = next.unfinished_handoff.as_mut().expect("checked journal");
        journal.failures = failures;
        journal.retry_after_ms = origin::now_ms().saturating_add(1_000u64 << failures.min(5));
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
    #[test]
    fn persistent_close_failure_keeps_one_original_and_no_retained() {
        let (root_guard, dir, mut owner, front) = queued();
        let journal = owner
            .begin_handoff(&front, "owned".into(), "claude/t/h:owned".into())
            .unwrap();
        for _ in 0..6 {
            owner.handoff_failed(&journal.id).unwrap();
            owner.hold_front(&front).unwrap();
            owner.grant_front(&front).unwrap();
        }
        assert_eq!(owner.snapshot.items.len(), 1);
        assert!(owner.snapshot.retained_inputs.is_empty());
        assert_eq!(owner.handoff().unwrap().failures, 6);
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
        for _ in 0..4 {
            owner.handoff_failed(&journal.id).unwrap();
        }
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
