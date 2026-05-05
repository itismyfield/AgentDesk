//! Dispatch-route DTOs.
//!
//! #1693 introduced this module when splitting
//! `src/server/routes/dispatches/discord_delivery.rs` into thin handlers +
//! orchestration + repo + DTOs. Delivery response and error shapes live here
//! so route-layer callers can depend on DTOs instead of reaching across into
//! service internals.
//!
//! When new request/response shapes are added for dispatch routes, prefer
//! defining them here directly to keep the route surface declarative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReviewFollowupKind {
    Pass,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DispatchMessagePostErrorKind {
    MessageTooLong,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DispatchMessagePostError {
    kind: DispatchMessagePostErrorKind,
    detail: String,
}

impl DispatchMessagePostError {
    pub(crate) fn new(kind: DispatchMessagePostErrorKind, detail: String) -> Self {
        Self { kind, detail }
    }

    pub(crate) fn kind(&self) -> DispatchMessagePostErrorKind {
        self.kind
    }

    pub(crate) fn is_length_error(&self) -> bool {
        self.kind == DispatchMessagePostErrorKind::MessageTooLong
    }
}

impl std::fmt::Display for DispatchMessagePostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.detail)
    }
}

impl std::error::Error for DispatchMessagePostError {}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub(crate) struct DispatchNotifyDeliveryResult {
    pub(crate) status: String,
    pub(crate) dispatch_id: String,
    pub(crate) action: String,
    pub(crate) correlation_id: Option<String>,
    pub(crate) semantic_event_id: Option<String>,
    pub(crate) target_channel_id: Option<String>,
    pub(crate) message_id: Option<String>,
    pub(crate) fallback_kind: Option<String>,
    pub(crate) detail: Option<String>,
}

impl DispatchNotifyDeliveryResult {
    pub(crate) fn success(
        dispatch_id: impl Into<String>,
        action: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            status: "success".to_string(),
            dispatch_id: dispatch_id.into(),
            action: action.into(),
            correlation_id: None,
            semantic_event_id: None,
            target_channel_id: None,
            message_id: None,
            fallback_kind: None,
            detail: Some(detail.into()),
        }
    }

    pub(crate) fn duplicate(dispatch_id: impl Into<String>, detail: impl Into<String>) -> Self {
        let dispatch_id = dispatch_id.into();
        Self {
            status: "duplicate".to_string(),
            action: "notify".to_string(),
            correlation_id: Some(format!("dispatch:{dispatch_id}")),
            semantic_event_id: Some(format!("dispatch:{dispatch_id}:notify")),
            dispatch_id,
            target_channel_id: None,
            message_id: None,
            fallback_kind: None,
            detail: Some(detail.into()),
        }
    }

    pub(crate) fn permanent_failure(
        dispatch_id: impl Into<String>,
        action: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            status: "permanent_failure".to_string(),
            dispatch_id: dispatch_id.into(),
            action: action.into(),
            correlation_id: None,
            semantic_event_id: None,
            target_channel_id: None,
            message_id: None,
            fallback_kind: None,
            detail: Some(detail.into()),
        }
    }

    pub(crate) fn with_thread_creation_fallback(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        self.status = "fallback".to_string();
        self.fallback_kind = Some(match self.fallback_kind.take() {
            Some(existing) => format!("ThreadCreationParentChannel+{existing}"),
            None => "ThreadCreationParentChannel".to_string(),
        });
        self.detail = Some(match self.detail.take() {
            Some(existing) if !existing.trim().is_empty() => format!("{detail}; {existing}"),
            _ => detail,
        });
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DispatchMessagePostOutcome {
    pub(crate) message_id: String,
    pub(crate) delivery: DispatchNotifyDeliveryResult,
}

// CRUD body re-exports (current canonical home: routes::dispatches::crud).

// #1694: Followup configuration DTO that the outbox followup orchestration
// uses to thread Discord API base URL + bot tokens through. Lives here so
// the route + test layers can construct it without depending on the
// route module internals.
#[derive(Clone, Debug)]
pub(crate) struct DispatchFollowupConfig {
    pub discord_api_base: String,
    pub notify_bot_token: Option<String>,
    pub announce_bot_token: Option<String>,
}

impl DispatchFollowupConfig {
    pub(crate) fn from_runtime() -> Self {
        Self {
            discord_api_base: crate::services::dispatches::discord_delivery::discord_api_base_url(),
            notify_bot_token: crate::credential::read_bot_token("notify"),
            announce_bot_token: crate::credential::read_bot_token("announce"),
        }
    }
}
