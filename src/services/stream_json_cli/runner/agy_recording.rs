use crate::services::agent_protocol::StreamMessage;
use crate::services::tui_o::store::agy_spool::{AgySpool, Failure};
use uuid::Uuid;

use super::StreamJsonCodec;

pub(super) struct Recording<'a> {
    pub(super) spool: &'a mut AgySpool,
    pub(super) failure: Failure,
    conversation: Option<Uuid>,
    storage_failed: bool,
}

impl<'a> Recording<'a> {
    pub(super) fn new(spool: &'a mut AgySpool) -> Self {
        Self {
            conversation: spool.resume(),
            spool,
            failure: Failure::Spawn,
            storage_failed: false,
        }
    }

    pub(super) fn message(&mut self, message: &StreamMessage) {
        if self.storage_failed {
            return;
        }
        let result = match message {
            StreamMessage::Init { session_id, .. } => {
                let Ok(id) = Uuid::parse_str(session_id) else {
                    return;
                };
                if self.conversation.is_some_and(|expected| expected != id) {
                    return;
                }
                self.conversation = Some(id);
                self.spool.init(id)
            }
            StreamMessage::Text { content } => self.spool.delta(content),
            _ => return,
        };
        self.record(result);
    }

    pub(super) fn done(&mut self, codec: &mut dyn StreamJsonCodec) {
        if self.storage_failed {
            return;
        }
        let Some((conversation, body)) = codec.captured_body() else {
            return;
        };
        let result = self.spool.done(conversation, &body);
        self.record(result);
    }

    fn record(&mut self, result: std::io::Result<()>) {
        if let Err(error) = result {
            self.storage_failed = true;
            tracing::warn!(%error, "agy spool remains unsealed");
        }
    }
}

impl Drop for Recording<'_> {
    fn drop(&mut self) {
        let result = self.spool.fail(self.failure);
        self.record(result);
    }
}
