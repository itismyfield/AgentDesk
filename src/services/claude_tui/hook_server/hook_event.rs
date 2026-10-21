use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;

use super::HookEventKind;
use crate::services::tui_prompt_dedupe::binding_context::{
    BINDING_HEADER, BindingContext, CapturedContext, decode_binding_header,
};

#[derive(Clone, Debug, Serialize)]
pub struct HookEvent {
    pub provider: String,
    pub session_id: String,
    pub kind: HookEventKind,
    pub received_at: DateTime<Utc>,
    pub payload: Value,
    #[serde(skip)]
    pub(crate) fanout: Option<HookFanout>,
    #[serde(skip)]
    pub(crate) original_context: Option<BindingContext>,
}

#[derive(Clone, Debug)]
pub(crate) struct HookFanout {
    pub(crate) origin_session_id: String,
    pub(crate) primary_discarded: bool,
}

#[cfg(test)]
impl Default for HookEvent {
    fn default() -> Self {
        Self {
            provider: String::new(),
            session_id: String::new(),
            kind: HookEventKind::Unknown(String::new()),
            received_at: Utc::now(),
            payload: Value::Null,
            fanout: None,
            original_context: None,
        }
    }
}

/// Preserve the sender's context after ingress; routing and current markers cannot replace it.
pub(super) fn original_context(headers: &axum::http::HeaderMap) -> Option<BindingContext> {
    let envelope = decode_binding_header(headers.get(BINDING_HEADER)?.to_str().ok()?).ok()?;
    match envelope.context {
        CapturedContext::Captured(context) => Some(context),
        CapturedContext::Absent(_) => None,
    }
}
