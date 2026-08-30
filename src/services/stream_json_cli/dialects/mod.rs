//! Provider-neutral Stream-JSON CLI dialect dispatch.

pub mod agy;
pub mod grok;

use std::sync::mpsc::Sender;

use crate::services::agent_protocol::StreamMessage;

use super::request::ProviderTurnRequest;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamJsonDialect {
    Grok,
    Antigravity,
}

pub fn execute(
    dialect: StreamJsonDialect,
    request: ProviderTurnRequest,
    sender: Sender<StreamMessage>,
) -> Result<(), String> {
    match dialect {
        StreamJsonDialect::Grok => grok::execute(request, sender),
        StreamJsonDialect::Antigravity => agy::execute(request, sender),
    }
}
