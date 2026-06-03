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
//!
//! Note: the non-presentation delivery transport types
//! (`DispatchMessagePostError`, `DispatchNotifyDeliveryResult`,
//! `DispatchMessagePostOutcome`, `ReviewFollowupKind`) were relocated to
//! `crate::services::dispatches::discord_delivery::transport` (#3037 bucket 4);
//! they now live beside the delivery logic that owns them.
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

#[cfg(test)]
mod tests {
    use super::*;

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
