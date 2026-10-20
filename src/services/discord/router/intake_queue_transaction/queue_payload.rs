use super::SoftInterventionSpec;

impl SoftInterventionSpec {
    /// Readable voice text is claimed durably when the queued turn runs.
    pub(in crate::services::discord::router) fn for_queue(mut self) -> Self {
        if self.voice_announcement.take().is_some() {
            self.merge_consecutive = false;
        }
        self
    }
}
