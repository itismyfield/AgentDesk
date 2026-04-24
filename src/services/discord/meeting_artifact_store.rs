//! Idempotent meeting artifact repository (issue #1008 first slice).
//!
//! Stores meeting artifacts — transcript chunks, summaries, action items —
//! keyed by `(meeting_id, artifact_kind, idempotency_key)`. On duplicate keys
//! the existing row is returned and no new insert happens, so that provider
//! retries, cancel races, and summary re-runs cannot introduce duplicates.
//!
//! This module is intentionally backing-store-agnostic: the default
//! implementation is an in-memory `Mutex<HashMap>`. A future slice can swap in
//! a DB-backed store that uses a `UNIQUE (meeting_id, kind, idempotency_key)`
//! index — the reducer in [`super::meeting_state_machine`] and the existing
//! `meeting_orchestrator::save_meeting_record` upstreams do not depend on the
//! backing store.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Artifact category. Kept as a string slug (rather than an enum with a fixed
/// set) so future kinds (action-items, decisions, follow-up tasks) can be
/// added without a cross-module change.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MeetingArtifactKind {
    Transcript,
    Summary,
    ActionItem,
    Other(String),
}

impl MeetingArtifactKind {
    pub fn as_slug(&self) -> &str {
        match self {
            MeetingArtifactKind::Transcript => "transcript",
            MeetingArtifactKind::Summary => "summary",
            MeetingArtifactKind::ActionItem => "action_item",
            MeetingArtifactKind::Other(v) => v.as_str(),
        }
    }
}

/// A stored artifact row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeetingArtifact {
    pub meeting_id: String,
    pub kind: MeetingArtifactKind,
    pub idempotency_key: String,
    pub payload: String,
    /// True if this row was written by the current `store_with_key` call,
    /// false if the call observed an existing row and returned it.
    pub created: bool,
}

/// Outcome of a store_with_key call — split so callers can log differently on
/// duplicates vs. first writes without inspecting equality.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreOutcome {
    Inserted(MeetingArtifact),
    Existing(MeetingArtifact),
}

impl StoreOutcome {
    pub fn artifact(&self) -> &MeetingArtifact {
        match self {
            StoreOutcome::Inserted(a) | StoreOutcome::Existing(a) => a,
        }
    }

    pub fn was_inserted(&self) -> bool {
        matches!(self, StoreOutcome::Inserted(_))
    }
}

type ArtifactKey = (String, String, String); // (meeting_id, kind_slug, idempotency_key)

/// Idempotent artifact repository.
///
/// Thread-safe via `Mutex`; cloning shares the same underlying store (this is
/// intentional — multiple handles in the orchestrator and the API route share
/// one logical repo).
#[derive(Clone, Debug, Default)]
pub struct MeetingArtifactRepo {
    inner: Arc<Mutex<HashMap<ArtifactKey, MeetingArtifact>>>,
}

impl MeetingArtifactRepo {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert the artifact if `(meeting_id, kind, idempotency_key)` is unused;
    /// otherwise return the existing row.
    pub fn store_with_key(
        &self,
        meeting_id: &str,
        kind: MeetingArtifactKind,
        idempotency_key: &str,
        payload: &str,
    ) -> StoreOutcome {
        let key = (
            meeting_id.to_string(),
            kind.as_slug().to_string(),
            idempotency_key.to_string(),
        );
        let mut guard = self.inner.lock().expect("artifact repo mutex poisoned");
        if let Some(existing) = guard.get(&key) {
            let mut clone = existing.clone();
            clone.created = false;
            return StoreOutcome::Existing(clone);
        }

        let artifact = MeetingArtifact {
            meeting_id: meeting_id.to_string(),
            kind,
            idempotency_key: idempotency_key.to_string(),
            payload: payload.to_string(),
            created: true,
        };
        guard.insert(key, artifact.clone());
        StoreOutcome::Inserted(artifact)
    }

    pub fn get(
        &self,
        meeting_id: &str,
        kind: &MeetingArtifactKind,
        idempotency_key: &str,
    ) -> Option<MeetingArtifact> {
        let key = (
            meeting_id.to_string(),
            kind.as_slug().to_string(),
            idempotency_key.to_string(),
        );
        let guard = self.inner.lock().expect("artifact repo mutex poisoned");
        guard.get(&key).cloned()
    }

    pub fn list_for_meeting(&self, meeting_id: &str) -> Vec<MeetingArtifact> {
        let guard = self.inner.lock().expect("artifact repo mutex poisoned");
        guard
            .iter()
            .filter(|((mid, _, _), _)| mid == meeting_id)
            .map(|(_, v)| v.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_key_returns_existing_without_overwrite() {
        let repo = MeetingArtifactRepo::new();
        let first =
            repo.store_with_key("mtg-1", MeetingArtifactKind::Summary, "key-a", "payload-v1");
        assert!(first.was_inserted());
        assert_eq!(first.artifact().payload, "payload-v1");

        let second = repo.store_with_key(
            "mtg-1",
            MeetingArtifactKind::Summary,
            "key-a",
            "payload-v2-should-be-ignored",
        );
        assert!(!second.was_inserted());
        // Existing payload preserved; duplicate insert did NOT overwrite.
        assert_eq!(second.artifact().payload, "payload-v1");
        assert_eq!(repo.len(), 1);
    }

    #[test]
    fn different_idempotency_key_inserts_new_row() {
        let repo = MeetingArtifactRepo::new();
        let a = repo.store_with_key(
            "mtg-1",
            MeetingArtifactKind::Transcript,
            "round-1",
            "round-1-text",
        );
        let b = repo.store_with_key(
            "mtg-1",
            MeetingArtifactKind::Transcript,
            "round-2",
            "round-2-text",
        );
        assert!(a.was_inserted());
        assert!(b.was_inserted());
        assert_eq!(repo.len(), 2);
    }

    #[test]
    fn different_meeting_id_inserts_new_row() {
        let repo = MeetingArtifactRepo::new();
        repo.store_with_key("mtg-1", MeetingArtifactKind::Summary, "k", "p1");
        let second = repo.store_with_key("mtg-2", MeetingArtifactKind::Summary, "k", "p2");
        assert!(second.was_inserted());
        assert_eq!(repo.len(), 2);
    }

    #[test]
    fn different_kind_inserts_new_row() {
        let repo = MeetingArtifactRepo::new();
        repo.store_with_key("mtg-1", MeetingArtifactKind::Summary, "k", "summary");
        let second =
            repo.store_with_key("mtg-1", MeetingArtifactKind::ActionItem, "k", "action-item");
        assert!(second.was_inserted());
        assert_eq!(repo.len(), 2);
    }

    #[test]
    fn repo_clone_shares_backing_store() {
        let repo = MeetingArtifactRepo::new();
        let handle = repo.clone();
        repo.store_with_key("mtg-1", MeetingArtifactKind::Summary, "k", "v");
        assert_eq!(handle.len(), 1);
        let dup = handle.store_with_key("mtg-1", MeetingArtifactKind::Summary, "k", "v2");
        assert!(!dup.was_inserted());
        assert_eq!(repo.len(), 1);
    }

    #[test]
    fn list_for_meeting_returns_only_matching_rows() {
        let repo = MeetingArtifactRepo::new();
        repo.store_with_key("mtg-1", MeetingArtifactKind::Summary, "s", "s1");
        repo.store_with_key("mtg-1", MeetingArtifactKind::Transcript, "r1", "t1");
        repo.store_with_key("mtg-2", MeetingArtifactKind::Summary, "s", "s2");
        let rows = repo.list_for_meeting("mtg-1");
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|a| a.meeting_id == "mtg-1"));
    }

    #[test]
    fn get_returns_stored_row() {
        let repo = MeetingArtifactRepo::new();
        repo.store_with_key("mtg-1", MeetingArtifactKind::Summary, "k", "payload");
        let fetched = repo
            .get("mtg-1", &MeetingArtifactKind::Summary, "k")
            .expect("row should exist");
        assert_eq!(fetched.payload, "payload");
    }

    // ─── scenario tests: duplicate start / cancel race / summary retry / provider fail ───

    use super::super::meeting_state_machine::{
        InvalidTransition, MeetingEvent, MeetingState, transition, transition_idempotent_terminal,
    };

    /// (a) Duplicate start: once a slot is claimed (Pending -> Starting), a
    /// second Start must be rejected by the reducer. The artifact repo is not
    /// touched — artifact row count stays at 0.
    #[test]
    fn duplicate_start_rejected_by_reducer_and_produces_no_artifacts() {
        let repo = MeetingArtifactRepo::new();
        let state = transition(MeetingState::Pending, MeetingEvent::Start).unwrap();
        let dup = transition(state, MeetingEvent::Start);
        assert_eq!(
            dup,
            Err(InvalidTransition {
                from: MeetingState::Starting,
                event: MeetingEvent::Start,
            })
        );
        assert_eq!(repo.len(), 0);
    }

    /// (b) Cancel race: two concurrent cancel events against the same meeting
    /// id produce exactly one "cancelled" artifact because the idempotency key
    /// (meeting_id + kind + "cancel") deduplicates.
    #[test]
    fn concurrent_cancels_produce_one_artifact() {
        let repo = MeetingArtifactRepo::new();
        let meeting_id = "mtg-race";

        // First cancel: reducer transitions Running -> Cancelled.
        let s = transition(MeetingState::Running, MeetingEvent::Cancel).unwrap();
        assert_eq!(s, MeetingState::Cancelled);
        let first = repo.store_with_key(
            meeting_id,
            MeetingArtifactKind::Other("cancel_marker".to_string()),
            "cancel",
            "cancelled-by-user",
        );
        assert!(first.was_inserted());

        // Second cancel: idempotent reducer returns Cancelled without error;
        // artifact repo returns the existing row.
        let s2 = transition_idempotent_terminal(s, MeetingEvent::Cancel).unwrap();
        assert_eq!(s2, MeetingState::Cancelled);
        let second = repo.store_with_key(
            meeting_id,
            MeetingArtifactKind::Other("cancel_marker".to_string()),
            "cancel",
            "cancelled-by-user-retry",
        );
        assert!(!second.was_inserted());
        assert_eq!(second.artifact().payload, "cancelled-by-user");
        // Only one artifact row for the cancel_marker kind.
        assert_eq!(repo.list_for_meeting(meeting_id).len(), 1);
    }

    /// (c) Summary retry: running the summary twice with the same idempotency
    /// key produces exactly one summary row. The second call sees the existing
    /// row.
    #[test]
    fn summary_retry_does_not_duplicate_summary_artifact() {
        let repo = MeetingArtifactRepo::new();
        let meeting_id = "mtg-retry";
        let first = repo.store_with_key(
            meeting_id,
            MeetingArtifactKind::Summary,
            "summary:v1",
            "first summary text",
        );
        assert!(first.was_inserted());

        // Simulated retry (provider flake, then re-run) with same key.
        let second = repo.store_with_key(
            meeting_id,
            MeetingArtifactKind::Summary,
            "summary:v1",
            "retried summary text (different content)",
        );
        assert!(!second.was_inserted());
        assert_eq!(second.artifact().payload, "first summary text");
        assert_eq!(
            repo.list_for_meeting(meeting_id)
                .iter()
                .filter(|a| matches!(a.kind, MeetingArtifactKind::Summary))
                .count(),
            1
        );
    }

    /// (d) Provider failure transitions to Failed from any non-terminal state.
    #[test]
    fn provider_fail_transitions_to_failed() {
        for state in [
            MeetingState::Pending,
            MeetingState::Starting,
            MeetingState::Running,
            MeetingState::Summarizing,
        ] {
            assert_eq!(
                transition(state, MeetingEvent::ProviderFailed).unwrap(),
                MeetingState::Failed
            );
        }
    }
}
