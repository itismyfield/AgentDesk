//! Provider-neutral process, codec, session, and tool-policy substrate for line-delimited JSON CLIs.

pub mod codec;
pub mod dialects;
pub mod policy;
pub mod request;
pub mod runner;
pub mod session;

use std::sync::mpsc::Sender;

use crate::services::agent_protocol::StreamMessage;
use crate::services::provider::StreamJsonDialectId;

pub use policy::{AgentTool, ConfiguredToolPolicy, ToolPolicy};
pub use request::ProviderTurnRequest;
pub use session::ProviderSessionToken;

pub fn execute_streaming(
    dialect: StreamJsonDialectId,
    request: ProviderTurnRequest,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    match dialect {
        StreamJsonDialectId::Grok => {
            dialects::execute(dialects::StreamJsonDialect::Grok, request, sender)
        }
    }
}