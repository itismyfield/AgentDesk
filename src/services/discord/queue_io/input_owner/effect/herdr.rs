//! Compatibility fixture for the common owned Claude operation.
use super::{BusyClaude, Completion, Leaf};
use crate::services::claude_tui::{
    busy_inject,
    host_input::{BusyOperation, InputTarget},
};
use crate::services::session_host::HerdrTarget;
use std::path::PathBuf;

struct BusyClaudeHerdr {
    target: HerdrTarget,
    writer: String,
    session: String,
    transcript: PathBuf,
    channel: u64,
    source: String,
    author: String,
    name: String,
    nonce: String,
    text: String,
}
impl BusyClaudeHerdr {
    fn leaf(self) -> Leaf {
        BusyClaude {
            operation: BusyOperation::new(
                InputTarget::Herdr(self.target),
                "tmux".into(),
                &busy_inject::TIMING,
                false,
                std::sync::Arc::new(crate::services::provider::CancelToken::new()),
                Some(self.writer),
            ),
            session: self.session,
            transcript: self.transcript,
            channel: self.channel,
            source: self.source,
            author: self.author,
            name: self.name,
            nonce: self.nonce,
            text: self.text,
            draft_notice: None,
        }
        .leaf()
    }
}
#[path = "herdr_tests.rs"]
mod tests;
