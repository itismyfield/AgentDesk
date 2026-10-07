use super::{BindingEvent, codex};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A log line: the event plus whether its source passed the Claude source check and when its hook
/// was published. Both sit beside the event so readers of `BindingEvent` see the same record.
#[derive(Deserialize)]
pub(super) struct Logged {
    #[serde(flatten)]
    pub(super) event: BindingEvent,
    #[serde(default)]
    pub(super) verified: bool,
    #[serde(default)]
    pub(super) published_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub(super) codex_ownership: Option<codex::Ownership>,
}

#[derive(Serialize)]
pub(super) struct LoggedRef<'a> {
    #[serde(flatten)]
    pub(super) event: &'a BindingEvent,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(super) verified: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) published_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) codex_ownership: Option<&'a codex::Ownership>,
}
