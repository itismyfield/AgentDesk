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

// ─── Round-table persistence request DTOs ──────────────────────────────────
//
// `MeetingEntryBody` / `UpsertMeetingBody` were previously defined in
// `crate::server::routes::meetings`, which forced the service-layer producers
// (`meeting_orchestrator`, `internal_api`) to reach back into the server layer
// for the request shape they themselves construct (#3037 service→server
// backflow). They are pure serde payloads with no axum / `AppState` coupling,
// so they now live beside the meeting service layer; the server route module
// and the service-layer producers `use` them from here.

/// Single round-table meeting entry posted back from the Discord runtime as
/// part of an [`UpsertMeetingBody`].
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct MeetingEntryBody {
    pub seq: Option<i64>,
    pub round: Option<i64>,
    pub speaker_role_id: Option<String>,
    pub speaker_name: Option<String>,
    pub content: Option<String>,
    pub is_summary: Option<bool>,
}

/// Completed/cancelled meeting payload persisted via
/// `POST /api/round-table-meetings`.
#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct UpsertMeetingBody {
    pub id: String,
    pub channel_id: Option<String>,
    pub agenda: Option<String>,
    pub summary: Option<String>,
    pub selection_reason: Option<String>,
    pub status: Option<String>,
    pub primary_provider: Option<String>,
    pub reviewer_provider: Option<String>,
    pub participant_names: Option<Vec<String>>,
    pub total_rounds: Option<i64>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub thread_id: Option<String>,
    pub entries: Option<Vec<MeetingEntryBody>>,
}

/// Artifact category. Kept as a string slug (rather than an enum with a fixed
/// set) so future kinds (action-items, decisions, follow-up tasks) can be
/// added without a cross-module change.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum MeetingArtifactKind {
    // #3034: typed kinds retained per the doc above (future kinds without a
    // cross-module change); prod currently constructs `Other(..)` only.
    #[allow(dead_code)]
    Transcript,
    #[allow(dead_code)]
    Summary,
    #[allow(dead_code)]
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

// #3034: StoreOutcome accessors — public repo API not yet read by prod (callers
// match the variant directly). Retained as a coherent surface.
#[allow(dead_code)]
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

    // #3034: repo query API (get/list/len/is_empty) — public surface not yet
    // read by prod (only `store_with_key` is wired). Retained as a coherent repo.
    #[allow(dead_code)]
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

    #[allow(dead_code)] // #3034: repo query API, see note above.
    pub fn list_for_meeting(&self, meeting_id: &str) -> Vec<MeetingArtifact> {
        let guard = self.inner.lock().expect("artifact repo mutex poisoned");
        guard
            .iter()
            .filter(|((mid, _, _), _)| mid == meeting_id)
            .map(|(_, v)| v.clone())
            .collect()
    }

    #[allow(dead_code)] // #3034: repo query API, see note above.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.len()).unwrap_or(0)
    }

    #[allow(dead_code)] // #3034: repo query API, see note above.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
