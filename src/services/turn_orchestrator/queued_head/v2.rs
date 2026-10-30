//! V2 owner foundation, reachable through the mailbox only in tests until admission is wired.
use super::v2_types::*;
use super::{HeadPhase, PendingQueueItem, QueueIdentity};
use sha2::Digest;
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
    /// The front message the drive handed to a turn by exact ticket and original; one claim spends it.
    front_grant: Option<u64>,
}

/// What a turn claim is to the owner: the granted front, a source it holds, or none of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClaimClass {
    OwnerFront,
    OwnerHeld,
    NonOwner,
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
            front_grant: None,
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
        self.place(self.snapshot.clone(), attempt, payload)
    }

    /// Fixes the unit `payload` names over the queue front of `next`, binds every member's ticket
    /// to the one attempt and commits it all at once.
    fn place(
        &mut self,
        mut next: SnapshotV2,
        attempt: AttemptIdentity,
        payload: PreparedPayload,
    ) -> Result<AttemptRef, String> {
        let original = next.items.first().ok_or("empty queue")?.clone();
        let identity = sources::identity(&original);
        let key = AttemptRef {
            identity: identity.clone(),
            attempt: attempt.clone(),
        };
        let head = InputAttempt {
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
        };
        if !front_holds(&next.items, &head) {
            return Err("unit members differ from the queue front".into());
        }
        next.head_injection = Some(head);
        next.bind_reserved();
        self.commit(next)?;
        Ok(key)
    }

    /// Reserves the prepared queue front as one unit: `prepared` gives each original's shown name
    /// and single-input body; the first original it does not know starts the next unit.
    pub(crate) fn reserve_unit(
        &mut self,
        attempt: AttemptIdentity,
        prepared: impl Fn(&PendingQueueItem) -> Option<(String, String)>,
    ) -> Result<AttemptRef, String> {
        let payload = self.unit(&prepared)?;
        self.reserve(attempt, payload)
    }

    /// Rebuilds a reserved unit whose write permit is not handed over yet over the current front,
    /// keeping originals, tickets and order; the old key can no longer arm, so it never writes.
    pub(crate) fn reprepare(
        &mut self,
        key: &AttemptRef,
        nonce: String,
        prepared: impl Fn(&PendingQueueItem) -> Option<(String, String)>,
    ) -> Result<AttemptRef, String> {
        let head = self
            .snapshot
            .head_injection
            .as_ref()
            .ok_or("no reserved unit")?;
        if !key.matches(head) || head.phase != HeadPhase::Reserved {
            return Err("the write permit was already handed over".into());
        }
        if nonce.is_empty() || nonce == key.attempt.nonce {
            return Err("a reprepared unit needs a distinct nonce".into());
        }
        let payload = self.unit(&prepared)?;
        let mut attempt = key.attempt.clone();
        attempt.nonce = nonce;
        let mut next = self.snapshot.clone();
        next.head_injection = None;
        self.place(next, attempt, payload)
    }

    /// The one ordered member list a unit is made of, and the frame and digest rendered from it.
    fn unit(
        &self,
        prepared: &impl Fn(&PendingQueueItem) -> Option<(String, String)>,
    ) -> Result<PreparedPayload, String> {
        let views: Vec<MemberView> = (self.snapshot.items.iter())
            .map_while(|item| {
                let (name, body) = prepared(item)?;
                Some(MemberView {
                    original: item.clone(),
                    name,
                    body,
                })
            })
            .collect();
        if views.is_empty() {
            return Err("no prepared queue head".into());
        }
        let frame = render(&views);
        let digest = sha2::Sha256::digest(frame.as_bytes());
        let members: Vec<u64> = views.iter().map(|view| view.original.message_id).collect();
        Ok(PreparedPayload {
            digest: hex::encode(digest),
            frame,
            attribution: serde_json::json!({ "members": members }),
            followers: views
                .into_iter()
                .skip(1)
                .map(|view| view.original)
                .collect(),
        })
    }

    /// The unattempted queue front and the ticket whose receipt names it.
    pub(crate) fn front(&self) -> Option<(Ticket, PendingQueueItem)> {
        let front = self.snapshot.items.first()?;
        if self.snapshot.head_injection.is_some() {
            return None;
        }
        let receipts = self.snapshot.admission_receipts.iter();
        let tickets =
            receipts.filter_map(|r| Ticket::new(r.strip_prefix("tk:")?.rsplit_once(':')?.0).ok());
        let ticket = tickets
            .into_iter()
            .find(|t| self.ticket_original(t).is_some_and(|o| same(&o, front)))?;
        Some((ticket, front.clone()))
    }

    /// Lets the next claim for exactly this ticket's original at the front start a turn.
    pub(crate) fn grant_front(
        &mut self,
        ticket: &Ticket,
        original: &PendingQueueItem,
    ) -> Result<(), String> {
        match self.front() {
            Some((at, front)) if at == *ticket && same(&front, original) => {
                self.front_grant = Some(front.message_id);
                Ok(())
            }
            _ => Err("not the owner's unattempted front".into()),
        }
    }

    /// A message ID is no execution right: only the granted front starts, and any source the owner
    /// holds, queued, attempted, retained or settled, never starts beside it.
    pub(crate) fn claim_class(&mut self, message: u64) -> ClaimClass {
        let front = self.front().map(|(_, front)| front.message_id);
        if self.front_grant == Some(message) && front == Some(message) {
            self.front_grant = None;
            return ClaimClass::OwnerFront;
        }
        let s = &self.snapshot;
        let attempts = s
            .head_injection
            .iter()
            .chain(s.retained_inputs.iter().map(|r| &r.input));
        let mut held = s
            .items
            .iter()
            .chain(attempts.flat_map(InputAttempt::members));
        let names = |item: &PendingQueueItem| {
            item.message_id == message || item.source_message_ids.contains(&message)
        };
        let settled = (s.terminal_identities.iter()).any(|t| {
            t.identity.primary == message
                || t.identity.sources.iter().any(|x| x.message_id == message)
        });
        if held.any(names) || settled {
            ClaimClass::OwnerHeld
        } else {
            ClaimClass::NonOwner
        }
    }

    pub(crate) fn json(&self) -> Result<serde_json::Value, String> {
        serde_json::to_value(&self.snapshot).map_err(|error| error.to_string())
    }
}

impl InputAttempt {
    /// Every original this attempt carries, the head original first, in arrival order.
    pub(crate) fn members(&self) -> impl Iterator<Item = &PendingQueueItem> {
        let followers = self
            .payload
            .iter()
            .flat_map(|payload| payload.followers.iter());
        std::iter::once(&self.original).chain(followers)
    }
}

/// One queued original as the caller prepared it for a unit: its shown name and prompt body.
struct MemberView {
    original: PendingQueueItem,
    name: String,
    body: String,
}

/// The unit's body without the busy header the leaf adds for the first author: each original's
/// body in arrival order, with an author line only where the author changes after the first.
fn render(views: &[MemberView]) -> String {
    let mut author = None;
    let bodies = views.iter().map(|view| {
        let item = &view.original;
        let who = (item.author_id, item.author_is_bot);
        if author.replace(who).is_none_or(|before| before == who) {
            return view.body.clone();
        }
        let id = item.author_id.to_string();
        let line = crate::services::tui_input::human_frame::idle(&view.name, &id, "");
        format!("{line}\n{}", view.body)
    });
    bodies.collect::<Vec<_>>().join("\n\n")
}

/// Exact equality of two stored originals, as the restore compares them.
pub(super) fn same(a: &PendingQueueItem, b: &PendingQueueItem) -> bool {
    serde_json::to_value(a)
        .ok()
        .is_some_and(|a| serde_json::to_value(b).ok() == Some(a))
}

/// Whether the queue front holds exactly `input`'s members, in order; every consumer that
/// removes, keeps or restores a unit asks this one question.
pub(super) fn front_holds(items: &[PendingQueueItem], input: &InputAttempt) -> bool {
    let members: Vec<_> = input.members().collect();
    let front = items.get(..members.len());
    front.is_some_and(|front| front.iter().zip(&members).all(|(a, b)| same(a, b)))
}

// Owner fixtures require a confirmed parent directory fsync for adoption and commits.
#[cfg(all(test, unix))]
#[path = "v2/owner_tests.rs"]
mod tests;
// Unit fixtures adopt real owner files, which need a confirmed parent directory fsync.
#[cfg(all(test, unix))]
#[path = "v2/unit_tests.rs"]
mod unit_tests;
