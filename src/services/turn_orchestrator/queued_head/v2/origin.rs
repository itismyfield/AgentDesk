//! The origins of inputs the owner removes unrun, and the terminal receipts it owes for them until
//! the database holds them; a reader outside the actor finds an origin still in a queue file.
use super::super::compat::{SnapshotRead, inspect};
use super::super::v2_types::{AdmissionBind, BindAt, InputAttempt, OwedDrop, SnapshotV2};
use super::Owner;
use crate::db::turns::TerminalOutcome;

/// Wall-clock milliseconds, the instant a removal owes its receipt at.
pub(super) fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

/// The origin an HTTP start or delivery was admitted under, read from its kept provenance.
fn origin_of(bind: &AdmissionBind) -> Option<String> {
    let execution = bind.context.as_ref()?.execution.as_ref()?;
    let origin = execution
        .pointer("/metadata/human_input/origin_id")?
        .as_str()?;
    let origin = origin.trim();
    (!origin.is_empty()).then(|| origin.to_string())
}

impl SnapshotV2 {
    /// Owes a receipt for each queued item at `removed` that carries an origin; runs before the
    /// removal in the same snapshot, so the commit that removes it also owes it.
    pub(super) fn owe_queued(
        &mut self,
        removed: impl IntoIterator<Item = usize>,
        outcome: TerminalOutcome,
        at_ms: u64,
    ) {
        for index in removed {
            let Some(message_id) = self.items.get(index).map(|item| item.message_id) else {
                continue;
            };
            let origins = (self.admission_binds.iter())
                .filter(|b| matches!(b.at, BindAt::Queued { index: at, .. } if at == index))
                .filter_map(origin_of)
                .collect();
            self.owe(message_id, origins, outcome, at_ms);
        }
    }

    /// Owes a receipt for a retained input through the binds its attempt carries.
    pub(super) fn owe_attempt(
        &mut self,
        input: &InputAttempt,
        outcome: TerminalOutcome,
        at_ms: u64,
    ) {
        let Some(attempt) = &input.attempt else {
            return;
        };
        let origins = (self.admission_binds.iter())
            .filter(|b| matches!(&b.at, BindAt::Attempt { attempt: a, .. } if a == attempt))
            .filter_map(origin_of)
            .collect();
        self.owe(input.original.message_id, origins, outcome, at_ms);
    }

    /// Owes receipts for opaque admissions before a clear removes their retained originals.
    pub(super) fn owe_opaque(&mut self, outcome: TerminalOutcome, at_ms: u64) {
        let mut removed = std::collections::BTreeMap::<u64, Vec<String>>::new();
        for bind in &self.admission_binds {
            if !matches!(bind.at, BindAt::Opaque) {
                continue;
            }
            let Some(message_id) = bind
                .context
                .as_ref()
                .and_then(|context| context.original.as_ref())
                .and_then(|original| original["message_id"].as_u64())
            else {
                continue;
            };
            if let Some(origin) = origin_of(bind) {
                let origins = removed.entry(message_id).or_default();
                if !origins.contains(&origin) {
                    origins.push(origin);
                }
            }
        }
        for (message_id, origins) in removed {
            self.owe(message_id, origins, outcome, at_ms);
        }
    }

    fn owe(&mut self, message_id: u64, origins: Vec<String>, outcome: TerminalOutcome, at: u64) {
        let owed = (self.owed_drop_receipts.iter()).any(|o| o.message_id == message_id);
        if origins.is_empty() || owed {
            return;
        }
        self.owed_drop_receipts.push(OwedDrop {
            message_id,
            origins,
            outcome,
            at_ms: at,
        });
    }
}

impl Owner {
    /// The owed receipts as committed; reading leaves them owed.
    pub(crate) fn owed_drops(&self) -> Vec<OwedDrop> {
        self.snapshot.owed_drop_receipts.clone()
    }

    /// Removes exactly the owed receipts the database now holds.
    pub(crate) fn ack_owed_drops(&mut self, done: &[OwedDrop]) -> Result<(), String> {
        let mut next = self.snapshot.clone();
        next.owed_drop_receipts.retain(|owed| !done.contains(owed));
        if next.owed_drop_receipts.len() == self.snapshot.owed_drop_receipts.len() {
            return Ok(());
        }
        self.commit(next)
    }
}

/// Where a queue file still holds an origin: admitted and waiting, or removed with its receipt owed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Waiting {
    Accepted,
    Owed(OwedDrop),
}

/// Reads `channel`'s queue file under every token of `provider`, the empty one included, without
/// its owner; an unreadable file is an error, never an absent origin.
pub(crate) fn waiting_origin(
    provider: &str,
    channel: u64,
    origin: &str,
) -> Result<Option<Waiting>, String> {
    let root = crate::services::discord::runtime_store::discord_pending_queue_root();
    let Some(dir) = root.map(|root| root.join(provider)) else {
        return Ok(None);
    };
    let tokens = match std::fs::read_dir(&dir) {
        Ok(tokens) => tokens,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("read {}: {error}", dir.display())),
    };
    // An empty token hash keeps its file directly under the provider directory.
    let name = format!("{channel}.json");
    let dirs = tokens
        .flatten()
        .map(|token| token.path())
        .filter(|path| path.is_dir());
    for path in std::iter::once(dir.join(&name)).chain(dirs.map(|token| token.join(&name))) {
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("read {}: {error}", path.display())),
        };
        let SnapshotRead::ReadOnlyV2(snapshot) = inspect(&raw)? else {
            continue;
        };
        let names = |origins: &[String]| origins.iter().any(|o| o == origin);
        let mut owed = snapshot.owed_drop_receipts.iter();
        if let Some(owed) = owed.find(|owed| names(&owed.origins)) {
            return Ok(Some(Waiting::Owed(owed.clone())));
        }
        let len = snapshot.items.len();
        let waits = snapshot.admission_binds.iter().any(|b| {
            let live = match b.at {
                BindAt::Queued { index, .. } => index < len,
                BindAt::Settled { .. } => false,
                BindAt::Opaque | BindAt::Attempt { .. } => true,
            };
            live && origin_of(b).as_deref() == Some(origin)
        });
        if waits {
            return Ok(Some(Waiting::Accepted));
        }
    }
    Ok(None)
}

#[cfg(test)]
#[path = "origin_tests.rs"]
mod tests;
