use super::*;
use std::collections::HashMap;

#[derive(Default)]
pub(super) struct Starts {
    failures: HashMap<String, u8>,
    pub(super) closing: Option<serde_json::Value>,
}
impl Owner {
    fn start_key(&self, item: &PendingQueueItem) -> String {
        let identity = sources::identity(item);
        let receipt = self
            .snapshot
            .items
            .iter()
            .position(|i| sources::identity(i) == identity)
            .and_then(|index| {
                self.snapshot
                    .admission_binds
                    .iter()
                    .find_map(|b| match &b.at {
                        BindAt::Queued { index: at, .. } if *at == index => Some(b.receipt.clone()),
                        _ => None,
                    })
            });
        receipt
            .or_else(|| {
                self.snapshot
                    .admission_binds
                    .iter()
                    .find_map(|b| match &b.at {
                        BindAt::Attempt { identity: at, .. } if *at == identity => {
                            Some(b.receipt.clone())
                        }
                        _ => None,
                    })
            })
            .unwrap_or_else(|| {
                serde_json::to_string(&identity).expect("source identity serializes")
            })
    }
    pub(super) fn reset_start_failures(&mut self, members: &[PendingQueueItem]) {
        for member in members {
            let key = self.start_key(member);
            self.starts.failures.remove(&key);
        }
    }
    pub(super) fn retry_member(&self, front: &PendingQueueItem, member: &PendingQueueItem) -> bool {
        if self
            .handoff()
            .is_some_and(|journal| journal.retry_after_ms > 0)
        {
            return false;
        }
        !self.starts.failures.contains_key(&self.start_key(front))
            || self.starts.failures.contains_key(&self.start_key(member))
    }
    // The snapshot publish and runtime rights change in one actor turn, before bridge teardown.
    #[cfg(unix)]
    pub(crate) fn start_failed(
        &mut self,
        front: &Front,
        number: u64,
        members: &[PendingQueueItem],
        pin: Option<serde_json::Value>,
        kind: StartFailureKind,
        detail: &str,
    ) -> Result<Option<bool>, String> {
        if self.pending.is_some() {
            return Err("start failure commit requires recovery".into());
        }
        if self.snapshot.head_injection.is_some()
            || number != self.claim_number
            || !self.claimed(front.1.message_id, front)
        {
            return Ok(None);
        }
        if members.is_empty()
            || members
                .iter()
                .zip(&self.snapshot.items)
                .any(|(a, b)| !same(a, b))
            || members.len() > self.snapshot.items.len()
        {
            return Err("start failure members changed".into());
        }
        let counts: Vec<_> = members
            .iter()
            .map(|m| {
                let key = self.start_key(m);
                let n = self
                    .starts
                    .failures
                    .get(&key)
                    .copied()
                    .unwrap_or(0)
                    .saturating_add(1)
                    .max(self.handoff().map_or(0, |journal| {
                        journal.failures.saturating_add(1).min(u8::MAX as u32) as u8
                    }));
                (key, n)
            })
            .collect();
        let retain = kind != StartFailureKind::Opaque || counts.iter().any(|(_, n)| *n >= 2);
        let mut next = self.snapshot.clone();
        if retain {
            next.owe_queued(
                0..members.len(),
                crate::db::turns::TerminalOutcome::Failed,
                origin::now_ms(),
            );
            for (index, original) in members.iter().enumerate() {
                next.retained_inputs.push(RetainedInput {
                    input: InputAttempt {
                        identity: sources::identity(original),
                        original: original.clone(),
                        phase: HeadPhase::Reserved,
                        attempt: None,
                        payload: None,
                        confirmation_deadline_ms: None,
                        resolved_source: None,
                        payload_window: None,
                        resolved_target: None,
                        preparation_hold: None,
                        legacy_attempt: None,
                    },
                    reason: RetainedReason::Rejected {
                        reason: format!("start_rejected: {detail}"),
                        start_failure: Some(StartFailureDetail {
                            kind,
                            failures: counts[index].1,
                        }),
                    },
                    disposition: Some(RetainedDisposition::KnownNotSent),
                    notice: NoticeIntent {
                        kind: "retained_retry_required".into(),
                        enqueued: false,
                    },
                });
            }
            for bind in &mut next.admission_binds {
                if let BindAt::Queued { index, .. } = &mut bind.at {
                    if *index < members.len() {
                        bind.at = BindAt::Opaque;
                    } else {
                        *index -= members.len();
                    }
                }
            }
            next.items.drain(..members.len());
            if next.unfinished_handoff.as_ref().is_some_and(|journal| {
                journal.ended
                    || !next
                        .items
                        .iter()
                        .any(|item| sources::identity(item) == journal.original)
            }) {
                next.unfinished_handoff = None;
            }
        }
        self.commit(next)?;
        for (key, n) in counts {
            self.starts.failures.insert(key, n);
        }
        self.starts.closing = pin;
        self.revoke_front();
        self.held = None;
        Ok(Some(retain))
    }
    #[cfg(unix)]
    pub(crate) fn start_cleanup_completed(&mut self, pin: &serde_json::Value) -> bool {
        if !self
            .starts
            .closing
            .as_ref()
            .is_some_and(|closing| closing == pin)
        {
            return false;
        }
        self.starts.closing = None;
        true
    }
    pub(crate) fn attemptless_notice_enqueued(
        &mut self,
        identity: QueueIdentity,
    ) -> Result<(), String> {
        let mut next = self.snapshot.clone();
        let retained = next
            .retained_inputs
            .iter_mut()
            .find(|r| r.input.identity == identity && r.input.attempt.is_none())
            .ok_or("stale attemptless notice")?;
        retained.notice.enqueued = true;
        self.commit(next)
    }
}

#[cfg(all(test, unix))]
#[path = "start_failure_tests.rs"]
mod tests;
