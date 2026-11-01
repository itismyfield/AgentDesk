use super::SoftInterventionSpec;

impl SoftInterventionSpec {
    pub(super) fn mark_raw(mut self) -> Self {
        self.reply_context =
            super::super::response_format::retry_prompt_context::RetryPromptContext::raw(
                self.reply_context.take(),
            )
            .encode();
        self
    }
}
