//! V2 owner foundation, reachable through the mailbox only in tests until admission is wired.
use super::v2_types::*;
use super::{HeadPhase, PendingQueueItem, QueueIdentity};
use std::path::PathBuf;

pub(crate) mod actor;
mod attempt;
mod commit;
pub(crate) mod control;
mod migration;
pub(crate) mod projection;
mod retained;
pub(crate) mod source_admission;
mod sources;
pub(crate) use attempt::AttemptRef;
pub(crate) use commit::Fault;
use source_admission::AdmitError;
pub(crate) use source_admission::Ticket;

pub(crate) struct Owner {
    pub(crate) snapshot: SnapshotV2,
    path: PathBuf,
    bytes: Option<String>,
    pending: Option<(String, SnapshotV2)>,
    pub(crate) fault: Fault,
    /// Ticket admissions committed while their caller saw Pending, with the outcome owed to it.
    unacknowledged: Vec<(String, source_admission::Admitted)>,
}

impl Owner {
    pub(crate) fn adopt(path: PathBuf, namespace: String) -> Result<Self, String> {
        let bytes = match std::fs::read_to_string(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.to_string()),
        };
        let snapshot = match bytes.as_deref() {
            Some(raw) => sources::restore(raw, &namespace)?,
            None => SnapshotV2 {
                version: 2,
                admission_receipts: vec![],
                admission_sequence: 0,
                admission_binds: Vec::new(),
                items: vec![],
                head_injection: None,
                retained_inputs: vec![],
                terminal_identities: vec![],
                namespace: Some(namespace),
            },
        };
        let mut owner = Self {
            snapshot,
            path,
            bytes,
            pending: None,
            fault: Fault::None,
            unacknowledged: Vec::new(),
        };
        // A restored bootstrap is possibly sent; missing deadline never proves NotSent.
        let mut restored = owner.snapshot.clone();
        if owner.bytes.is_some()
            && let Some(head) = restored.head_injection.as_mut()
            && head.phase != HeadPhase::Reserved
            && matches!(head.payload_window, Some(PayloadWindow::Bootstrap))
        {
            head.phase = HeadPhase::Unknown;
        }
        owner.commit(restored)?;
        owner.project_slots();
        Ok(owner)
    }

    fn enqueue(&mut self, item: &PendingQueueItem) -> Result<bool, AdmitError> {
        if self.pending.is_some() {
            return Err(AdmitError::NotCommitted("owner recovery required".into()));
        }
        if self.fenced(item) {
            return Ok(false);
        }
        if self.snapshot.items.len() >= 30 {
            return Err(AdmitError::Full);
        }
        let mut next = self.snapshot.clone();
        next.prune_binds();
        next.items.push(item.clone());
        self.admit_commit(next, None)?;
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn enqueue_fixture(&mut self, item: &PendingQueueItem) -> Result<bool, AdmitError> {
        self.enqueue(item)
    }

    pub(crate) fn reserve(
        &mut self,
        attempt: AttemptIdentity,
        payload: PreparedPayload,
    ) -> Result<AttemptRef, String> {
        if self.snapshot.head_injection.is_some() {
            return Err("active head".into());
        }
        let original = self.snapshot.items.first().ok_or("empty queue")?.clone();
        let identity = sources::identity(&original);
        let key = AttemptRef {
            identity: identity.clone(),
            attempt: attempt.clone(),
        };
        let mut next = self.snapshot.clone();
        next.head_injection = Some(InputAttempt {
            identity,
            original,
            phase: HeadPhase::Reserved,
            attempt: Some(attempt),
            payload: Some(payload),
            confirmation_deadline_ms: None,
            resolved_source: None,
            payload_window: None,
            resolved_target: None,
            preparation_hold: None,
            legacy_attempt: None,
        });
        next.bind_reserved();
        self.commit(next)?;
        Ok(key)
    }

    pub(crate) fn json(&self) -> Result<serde_json::Value, String> {
        serde_json::to_value(&self.snapshot).map_err(|error| error.to_string())
    }
}

#[cfg(test)]
#[path = "v2/owner_tests.rs"]
mod tests;
