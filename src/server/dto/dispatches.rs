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

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub(crate) struct DispatchMessagePostOutcome {
    pub(crate) message_id: String,
    pub(crate) delivery: DispatchNotifyDeliveryResult,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct DispatchListItem {
    pub(crate) id: String,
    pub(crate) kanban_card_id: Option<String>,
    pub(crate) from_agent_id: Option<String>,
    pub(crate) to_agent_id: Option<String>,
    pub(crate) dispatch_type: Option<String>,
    pub(crate) status: String,
    pub(crate) title: Option<String>,
    pub(crate) context: Option<serde_json::Value>,
    pub(crate) result: Option<serde_json::Value>,
    pub(crate) context_file: Option<serde_json::Value>,
    pub(crate) result_file: Option<serde_json::Value>,
    pub(crate) result_summary: Option<String>,
    pub(crate) parent_dispatch_id: Option<String>,
    pub(crate) chain_depth: i64,
    pub(crate) created_at: String,
    pub(crate) dispatched_at: Option<String>,
    pub(crate) updated_at: String,
    pub(crate) completed_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct DispatchListResponse {
    pub(crate) dispatches: Vec<DispatchListItem>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct DispatchResponse {
    pub(crate) dispatch: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct DispatchDeliveryEventsResponse {
    pub(crate) dispatch_id: String,
    pub(crate) events: Vec<crate::db::dispatches::delivery_events::DispatchDeliveryEvent>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct DispatchCreateResponse {
    pub(crate) dispatch: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) counter_model_resolution_reason: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub(crate) struct DispatchErrorResponse {
    pub(crate) error: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) dispatch_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize)]
#[serde(untagged)]
pub(crate) enum DispatchRouteResponse {
    List(DispatchListResponse),
    Dispatch(DispatchResponse),
    DeliveryEvents(DispatchDeliveryEventsResponse),
    Create(DispatchCreateResponse),
    Error(DispatchErrorResponse),
}

impl DispatchRouteResponse {
    pub(crate) fn list(dispatches: Vec<DispatchListItem>) -> Self {
        Self::List(DispatchListResponse { dispatches })
    }

    pub(crate) fn dispatch(dispatch: serde_json::Value) -> Self {
        Self::Dispatch(DispatchResponse { dispatch })
    }

    pub(crate) fn delivery_events(
        dispatch_id: impl Into<String>,
        events: Vec<crate::db::dispatches::delivery_events::DispatchDeliveryEvent>,
    ) -> Self {
        Self::DeliveryEvents(DispatchDeliveryEventsResponse {
            dispatch_id: dispatch_id.into(),
            events,
        })
    }

    pub(crate) fn created_dispatch(dispatch: serde_json::Value) -> Self {
        let counter_model_resolution_reason = dispatch
            .get("context")
            .and_then(|context| context.get("counter_model_resolution_reason"))
            .cloned();
        Self::Create(DispatchCreateResponse {
            dispatch,
            counter_model_resolution_reason,
        })
    }

    pub(crate) fn error(error: impl Into<String>) -> Self {
        Self::Error(DispatchErrorResponse {
            error: error.into(),
            dispatch_id: None,
        })
    }

    pub(crate) fn dispatch_error(error: impl Into<String>, dispatch_id: impl Into<String>) -> Self {
        Self::Error(DispatchErrorResponse {
            error: error.into(),
            dispatch_id: Some(dispatch_id.into()),
        })
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip_delivery(delivery: DispatchNotifyDeliveryResult) -> DispatchNotifyDeliveryResult {
        let value = serde_json::to_value(&delivery).expect("delivery DTO serializes");
        serde_json::from_value(value).expect("delivery DTO deserializes")
    }

    #[test]
    fn delivery_result_dto_pins_success_path() {
        let delivery = roundtrip_delivery(DispatchNotifyDeliveryResult::success(
            "dispatch-ok",
            "notify",
            "sent",
        ));

        assert_eq!(delivery.status, "success");
        assert_eq!(delivery.dispatch_id, "dispatch-ok");
        assert_eq!(delivery.action, "notify");
        assert_eq!(delivery.detail.as_deref(), Some("sent"));
        assert_eq!(delivery.fallback_kind, None);
    }

    #[test]
    fn delivery_result_dto_pins_duplicate_path() {
        let delivery = roundtrip_delivery(DispatchNotifyDeliveryResult::duplicate(
            "dispatch-dup",
            "already sent",
        ));

        assert_eq!(delivery.status, "duplicate");
        assert_eq!(delivery.dispatch_id, "dispatch-dup");
        assert_eq!(
            delivery.correlation_id.as_deref(),
            Some("dispatch:dispatch-dup")
        );
        assert_eq!(
            delivery.semantic_event_id.as_deref(),
            Some("dispatch:dispatch-dup:notify")
        );
    }

    #[test]
    fn delivery_result_dto_pins_fallback_path() {
        let delivery = roundtrip_delivery(
            DispatchNotifyDeliveryResult::success("dispatch-fallback", "notify", "minimal sent")
                .with_thread_creation_fallback("thread failed"),
        );

        assert_eq!(delivery.status, "fallback");
        assert_eq!(delivery.dispatch_id, "dispatch-fallback");
        assert_eq!(
            delivery.fallback_kind.as_deref(),
            Some("ThreadCreationParentChannel")
        );
        assert_eq!(
            delivery.detail.as_deref(),
            Some("thread failed; minimal sent")
        );
    }

    #[test]
    fn delivery_result_dto_pins_permanent_failure_path() {
        let delivery = roundtrip_delivery(DispatchNotifyDeliveryResult::permanent_failure(
            "dispatch-fail",
            "notify",
            "discord rejected",
        ));

        assert_eq!(delivery.status, "permanent_failure");
        assert_eq!(delivery.dispatch_id, "dispatch-fail");
        assert_eq!(delivery.action, "notify");
        assert_eq!(delivery.detail.as_deref(), Some("discord rejected"));
    }

    #[test]
    fn dispatch_route_response_omits_absent_counter_model_reason() {
        let response =
            serde_json::to_value(DispatchRouteResponse::created_dispatch(serde_json::json!({
                "id": "dispatch-1",
                "context": {}
            })))
            .expect("route response serializes");

        assert_eq!(response["dispatch"]["id"], "dispatch-1");
        assert!(response.get("counter_model_resolution_reason").is_none());
    }

    #[test]
    fn dispatch_route_response_promotes_counter_model_reason() {
        let response =
            serde_json::to_value(DispatchRouteResponse::created_dispatch(serde_json::json!({
                "id": "dispatch-1",
                "context": {"counter_model_resolution_reason": "agent_main_provider:codex=>claude"}
            })))
            .expect("route response serializes");

        assert_eq!(
            response["counter_model_resolution_reason"],
            "agent_main_provider:codex=>claude"
        );
    }
}
