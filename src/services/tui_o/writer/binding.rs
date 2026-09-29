//! The binding event log as O reads it: records in seq order and a change notice. O never
//! writes it; the log implements `BindingEvents` and is the one thing passed to the actor.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::services::tui_o::shadow::{ShadowProvider, SourceId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingCause {
    Startup,
    Resume,
    Clear,
    Compact,
    Continuation,
    Fork,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingTarget {
    Source(SourceId),
    /// The transcript file did not exist yet; a later `Resolved` names it.
    Pending {
        payload_session_id: String,
        payload_transcript_path: PathBuf,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingEvidence {
    pub hook_event: String,
    pub received_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingRecord {
    Bound {
        old: Option<SourceId>,
        new: BindingTarget,
        cause: BindingCause,
        parent_hint: Option<SourceId>,
        evidence: BindingEvidence,
    },
    /// Names the file of the `Pending` bind at `resolves_seq`.
    Resolved { resolves_seq: u64, source: SourceId },
    /// A refused late bind, kept for audit; it changes no binding.
    Rejected { detail: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BindingEvent {
    /// Per-channel, starting at 1 and increasing by one.
    pub seq: u64,
    pub channel_id: u64,
    pub provider: ShadowProvider,
    pub tmux_session: String,
    pub execution_nonce: String,
    pub record: BindingRecord,
    pub committed_at: DateTime<Utc>,
}

pub trait BindingEvents: Send + Sync + 'static {
    /// The channel's events with a seq above `after`, in seq order.
    fn binding_events_since(&self, channel: u64, after: u64) -> Result<Vec<BindingEvent>, String>;
    /// The channel's latest committed seq, updated after each append.
    fn subscribe(&self, channel: u64) -> watch::Receiver<u64>;
}
