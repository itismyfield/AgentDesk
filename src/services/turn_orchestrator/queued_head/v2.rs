//! V2 owner foundation, reachable through the mailbox only in tests until admission is wired.
use super::v2_types::*;
use super::{HeadPhase, PendingQueueItem, QueueIdentity};
use std::path::PathBuf;

pub(crate) mod actor;
mod attempt;
mod commit;
mod retained;
mod sources;
pub(crate) use attempt::AttemptRef;
pub(crate) use commit::Fault;

pub(crate) struct Owner {
    pub(crate) snapshot: SnapshotV2,
    path: PathBuf,
    bytes: Option<String>,
    pending: Option<(String, SnapshotV2)>,
    pub(crate) fault: Fault,
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
        };
        // Adoption commits conversion before any projection or new effect can occur.
        owner.commit(owner.snapshot.clone())?;
        owner.project_slots();
        Ok(owner)
    }

    pub(crate) fn enqueue(&mut self, item: PendingQueueItem) -> Result<bool, String> {
        if self.fenced(&item) {
            return Ok(false);
        }
        if self.snapshot.items.len() >= 30 {
            return Err("queue full".into());
        }
        let mut next = self.snapshot.clone();
        next.items.push(item);
        self.commit(next)?;
        Ok(true)
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
        });
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
