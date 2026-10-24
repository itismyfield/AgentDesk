use super::*;
use crate::services::discord::router::response_format::retry_prompt_context::{
    RetryPromptContext, store,
};
use crate::services::discord::router::turn_start::FormattedSessionRetryContext;

pub(super) struct Prepared {
    pub context: RetryPromptContext,
    pub recovery: Option<FormattedSessionRetryContext>,
    pub rendered: Option<String>,
}

pub(super) fn ingress(reply: Option<String>, queued: bool, worker: bool) -> RetryPromptContext {
    if queued || worker {
        RetryPromptContext::restore(reply)
    } else {
        // Fresh user bytes are always raw, including text resembling our wire envelope.
        RetryPromptContext::raw(reply)
    }
}

pub(super) async fn prepare(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    channel: ChannelId,
    mut context: RetryPromptContext,
) -> Prepared {
    let mut recovery = None;

    if context.may_select() {
        if context.selected.recovery.is_none() {
            context.selected.recovery = store::peek(
                shared.pg_pool.as_ref(),
                &store::key("session_retry_context", provider, channel.get()),
            );
        }
        if let Some(raw_context) = context.selected.recovery.as_ref() {
            let audit_record = if let Some(pool) = shared.pg_pool.as_ref() {
                crate::services::observability::recovery_audit::fetch_recovery_audit(
                    pool, channel.to_string(), 1,
                )
                .await
                .ok()
                .and_then(|records| records.into_iter().find(|record| {
                    record.content_sha256 == crate::services::observability::recovery_audit::recovery_context_sha256(raw_context)
                }))
            } else {
                None
            };
            let formatted_context =
                super::super::super::response_format::format_session_retry_context(raw_context);
            if let Some(formatted_context) = formatted_context {
                recovery = Some(FormattedSessionRetryContext {
                    raw_context: raw_context.clone(),
                    formatted_context,
                    audit_record,
                });
            }
        }
        if context.selected.feedback.is_none() {
            let (feedback, _) = take_and_merge_feedback_reminder(shared, provider, channel, None);
            context.selected.feedback = feedback;
        }
        if context.selected.wip.is_none() {
            let (wip, _) = take_and_merge_wip_warning(shared, provider, channel, None);
            context.selected.wip = wip;
        }
    }
    let rendered = context.render_with(merge_reply_contexts);
    Prepared {
        context,
        recovery,
        rendered,
    }
}

impl Prepared {
    pub(super) fn snapshot(&self, prompt: &str) -> Option<String> {
        let mut context = self.context.clone();
        context.expected_prompt = Some(prompt.to_owned());
        context.encode()
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod wiring_tests;
