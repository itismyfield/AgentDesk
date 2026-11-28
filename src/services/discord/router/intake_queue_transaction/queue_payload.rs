use super::SoftInterventionSpec;
use crate::services::turn_orchestrator::retry_prompt_codec::RetryPromptContext;

impl SoftInterventionSpec {
    pub(super) fn mark_raw(mut self) -> Self {
        self.reply_context = RetryPromptContext::raw(self.reply_context.take()).encode();
        self
    }
}
