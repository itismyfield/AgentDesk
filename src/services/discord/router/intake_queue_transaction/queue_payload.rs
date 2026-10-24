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

    /// Readable voice text is claimed durably when the queued turn runs.
    pub(in crate::services::discord::router) fn for_queue(mut self) -> Self {
        if self.voice_announcement.take().is_some() {
            self.merge_consecutive = false;
        }
        self
    }
}
