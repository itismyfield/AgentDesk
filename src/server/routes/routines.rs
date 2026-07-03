use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::services::routines::RoutinePatch;

const PARALLEL_SAFE_MIGRATED_LAUNCHD_SCRIPT_REF: &str = "migrated-launchd/queue-stability-batch.js";

#[derive(Debug, Deserialize)]
pub struct ListRoutinesQuery {
    pub agent_id: Option<String>,
    pub status: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ListRunsQuery {
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct RoutineMetricsQuery {
    pub agent_id: Option<String>,
    pub since: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
pub struct SearchRoutineRunResultsQuery {
    pub q: String,
    pub agent_id: Option<String>,
    pub status: Option<String>,
    pub since: Option<DateTime<Utc>>,
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct AttachRoutineBody {
    pub agent_id: Option<String>,
    pub fallback_agent_id: Option<String>,
    pub max_retries: Option<i32>,
    pub script_ref: String,
    pub name: Option<String>,
    pub execution_strategy: Option<String>,
    pub schedule: Option<String>,
    pub next_due_at: Option<DateTime<Utc>>,
    pub checkpoint: Option<Value>,
    pub discord_thread_id: Option<String>,
    pub timeout_secs: Option<i32>,
}

#[derive(Debug, Deserialize)]
pub struct PatchRoutineBody {
    pub name: Option<String>,
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    fallback_agent_id: PatchField<Option<String>>,
    pub max_retries: Option<i32>,
    pub execution_strategy: Option<String>,
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    schedule: PatchField<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    next_due_at: PatchField<Option<DateTime<Utc>>>,
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    checkpoint: PatchField<Option<Value>>,
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    discord_thread_id: PatchField<Option<String>>,
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    timeout_secs: PatchField<Option<i32>>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ResumeRoutineBody {
    /// PATCH semantics: only update `next_due_at` when the caller explicitly
    /// includes the field. A missing field preserves the existing value so a
    /// bare `{}` body never strands the routine by nulling `next_due_at`
    /// (#2395).
    #[serde(default, deserialize_with = "deserialize_patch_field")]
    next_due_at: PatchField<Option<DateTime<Utc>>>,
}

impl ResumeRoutineBody {
    fn next_due_at_update(&self) -> Option<Option<DateTime<Utc>>> {
        match &self.next_due_at {
            PatchField::Missing => None,
            PatchField::Present(value) => Some(*value),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PatchField<T> {
    Missing,
    Present(T),
}

impl<T> Default for PatchField<T> {
    fn default() -> Self {
        Self::Missing
    }
}

impl<T> PatchField<T> {
    fn as_present(&self) -> Option<&T> {
        match self {
            Self::Missing => None,
            Self::Present(value) => Some(value),
        }
    }

    fn into_option(self) -> Option<T> {
        match self {
            Self::Missing => None,
            Self::Present(value) => Some(value),
        }
    }
}

fn deserialize_patch_field<'de, D, T>(deserializer: D) -> Result<PatchField<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(PatchField::Present)
}

impl PatchRoutineBody {
    fn into_patch(self) -> RoutinePatch {
        RoutinePatch {
            name: self.name,
            fallback_agent_id: self.fallback_agent_id.into_option(),
            max_retries: self.max_retries,
            execution_strategy: self.execution_strategy,
            schedule: self.schedule.into_option(),
            next_due_at: self.next_due_at.into_option(),
            checkpoint: self.checkpoint.into_option(),
            discord_thread_id: self.discord_thread_id.into_option(),
            timeout_secs: self.timeout_secs.into_option(),
        }
    }
}

#[path = "routines/audit.rs"]
mod audit;
#[path = "routines/handlers.rs"]
mod handlers;
#[path = "routines/helpers.rs"]
mod helpers;
#[path = "routines/responses.rs"]
mod responses;

pub use self::handlers::{
    attach_routine, delete_routine, detach_routine, get_routine, kill_routine_session,
    list_routine_runs, list_routines, patch_routine, pause_routine, reset_routine_session,
    resume_routine, routine_metrics, run_routine_now, search_routine_run_results,
};

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{PatchRoutineBody, ResumeRoutineBody};

    #[test]
    fn patch_body_preserves_omitted_nullable_fields() {
        let body: PatchRoutineBody = serde_json::from_value(json!({})).unwrap();
        let patch = body.into_patch();

        assert_eq!(patch.schedule, None);
        assert_eq!(patch.next_due_at, None);
        assert_eq!(patch.checkpoint, None);
        assert_eq!(patch.discord_thread_id, None);
        assert_eq!(patch.timeout_secs, None);
    }

    #[test]
    fn patch_body_preserves_explicit_null_nullable_fields() {
        let body: PatchRoutineBody = serde_json::from_value(json!({
            "schedule": null,
            "next_due_at": null,
            "checkpoint": null,
            "discord_thread_id": null,
            "timeout_secs": null
        }))
        .unwrap();
        let patch = body.into_patch();

        assert_eq!(patch.schedule, Some(None));
        assert_eq!(patch.next_due_at, Some(None));
        assert_eq!(patch.checkpoint, Some(None));
        assert_eq!(patch.discord_thread_id, Some(None));
        assert_eq!(patch.timeout_secs, Some(None));
    }

    #[test]
    fn patch_body_preserves_present_nullable_values() {
        let body: PatchRoutineBody = serde_json::from_value(json!({
            "schedule": "@every 1h",
            "next_due_at": "2026-04-29T00:00:00Z",
            "checkpoint": {"cursor": "abc"},
            "discord_thread_id": "1234567890",
            "timeout_secs": 60
        }))
        .unwrap();
        let patch = body.into_patch();

        assert_eq!(patch.schedule, Some(Some("@every 1h".to_string())));
        assert!(patch.next_due_at.flatten().is_some());
        assert_eq!(patch.checkpoint, Some(Some(json!({"cursor": "abc"}))));
        assert_eq!(
            patch.discord_thread_id,
            Some(Some("1234567890".to_string()))
        );
        assert_eq!(patch.timeout_secs, Some(Some(60)));
    }

    /// #2395 — `POST /api/routines/:id/resume` with an empty body must NOT
    /// touch `next_due_at`. Previously a `{}` body deserialized to
    /// `next_due_at: None` and the SQL UPDATE wrote `next_due_at = NULL`,
    /// stranding the routine until dcserver restart.
    #[test]
    fn resume_body_omitted_next_due_at_is_preserved() {
        let body: ResumeRoutineBody = serde_json::from_value(json!({})).unwrap();
        assert_eq!(
            body.next_due_at_update(),
            None,
            "missing next_due_at must map to None (no SQL SET) so the existing column value is preserved"
        );
    }

    /// #2395 — explicit `"next_due_at": null` is the documented way to clear
    /// the next-fire timestamp (manual-only routines), and must still be
    /// distinguishable from a missing field.
    #[test]
    fn resume_body_explicit_null_clears_next_due_at() {
        let body: ResumeRoutineBody = serde_json::from_value(json!({
            "next_due_at": null,
        }))
        .unwrap();
        assert_eq!(body.next_due_at_update(), Some(None));
    }

    /// #2395 — present timestamp flows through to the store as
    /// `Some(Some(ts))`, producing a real SQL `SET next_due_at = $1`.
    #[test]
    fn resume_body_present_next_due_at_is_applied() {
        let body: ResumeRoutineBody = serde_json::from_value(json!({
            "next_due_at": "2026-04-29T00:00:00Z",
        }))
        .unwrap();
        let update = body.next_due_at_update().expect("field must be present");
        let ts = update.expect("timestamp must be Some");
        assert_eq!(ts.to_rfc3339(), "2026-04-29T00:00:00+00:00");
    }
}
