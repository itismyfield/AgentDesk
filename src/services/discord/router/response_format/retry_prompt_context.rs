use crate::services::turn_orchestrator::retry_prompt_codec::RetryPromptContext;

pub(in crate::services::discord) mod store;

pub(in crate::services::discord) fn render(context: &RetryPromptContext) -> Option<String> {
    if !context.may_select() {
        return context.raw.clone();
    }
    let mut reply = context.raw.clone();
    for block in [
        context
            .selected
            .recovery
            .as_deref()
            .and_then(super::format_session_retry_context),
        context
            .selected
            .feedback
            .as_deref()
            .and_then(super::format_voluntary_feedback_reminder),
        context
            .selected
            .wip
            .as_deref()
            .and_then(super::format_turn_end_wip_warning_injection),
    ] {
        reply = super::merge_reply_contexts(reply, block);
    }
    reply
}

#[cfg(test)]
mod tests;
