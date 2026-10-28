//! The resume-only start capability is installed with the migration effect root.

use super::*;
use crate::services::tui_prompt_dedupe::binding_context::{
    BINDING_HEADER, BindingContext, CapturedContext, decode_binding_header,
};
use crate::services::tui_prompt_dedupe::binding_events::{self, Committed, HookSignal};

pub(crate) struct Capability {
    pub(crate) context: BindingContext,
}

impl Capability {
    pub(crate) fn observe(
        &self,
        provider: &str,
        event: &str,
        command: Option<&str>,
        payload_session: Option<&str>,
        payload: &Value,
        headers: &HeaderMap,
    ) -> Option<observation_ingress::IngressOutcome> {
        if provider != "claude" || HookEventKind::from_path(event) != HookEventKind::SessionStart {
            return None;
        }
        let expected = self.context.expected_native_session_id.as_deref();
        if command != expected
            && command != Some(self.context.tmux_session.as_str())
            && payload_session != expected
        {
            return None;
        }
        let refused = observation_ingress::IngressOutcome::NotDurable(
            adoption_retry::NotDurableReason::ExecutionChanged,
        );
        let envelope = headers
            .get(BINDING_HEADER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| decode_binding_header(value).ok());
        if !matches!(envelope.map(|envelope| envelope.context), Some(CapturedContext::Captured(context)) if context == self.context)
        {
            return Some(refused);
        }
        crate::services::tmux_common::with_tmux_source_authority(
            &self.context.tmux_session,
            |authority| {
                if binding_events::claude_resume_start::context_identity(&self.context).is_err() {
                    return Some(refused);
                }
                if crate::services::tui_prompt_dedupe::herdr_hook_admitted_under_source_authority(
                    authority,
                    &self.context.execution_nonce,
                ) && matches!(
                    binding_events::claude_resume_start::start_recorded(&self.context),
                    Ok(true)
                ) {
                    return None;
                }
                if binding_events::claude_resume_start::context_source(&self.context).is_err() {
                    return Some(refused);
                }
                if payload_session != expected
                    || command.is_none()
                    || payload["source"] != "resume"
                    || payload
                        .get("transcript_path")
                        .and_then(Value::as_str)
                        .map(std::path::Path::new)
                        != self
                            .context
                            .claude_resume
                            .as_ref()
                            .map(|resume| resume.source.path.as_path())
                {
                    return Some(refused);
                }
                let mut hook = HookSignal::from_payload("session_start", payload);
                hook.published_at = headers
                    .get(super::relay_receipts::RELAY_PUBLISHED_AT_HEADER)
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                    .map(|value| value.with_timezone(&chrono::Utc));
                Some(
                    match binding_events::claude_resume_start::record_start(&self.context, &hook) {
                        Ok(Committed::Appended) => observation_ingress::IngressOutcome::Durable(
                            adoption_retry::DurableKind::Adopted,
                        ),
                        Ok(Committed::Unchanged) => observation_ingress::IngressOutcome::Durable(
                            adoption_retry::DurableKind::AlreadyRecorded,
                        ),
                        _ => observation_ingress::IngressOutcome::NotDurable(
                            adoption_retry::NotDurableReason::Append,
                        ),
                    },
                )
            },
        )
    }
}

#[path = "claude_resume_start_tests.rs"]
mod tests;
