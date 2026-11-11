//! A forward handoff owns no second input copy and never selects the old composer.
use super::*;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Journal {
    pub(crate) id: String,
    pub(crate) logical: String,
    pub(crate) session_key: String,
    pub(crate) original: QueueIdentity,
    pub(crate) pin: Option<serde_json::Value>,
    pub(crate) ended: bool,
    pub(crate) failures: u32,
    pub(crate) notice_acked: bool,
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
    /// A failed wrapper before reserve keeps its original queued; only its old row is retired.
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
