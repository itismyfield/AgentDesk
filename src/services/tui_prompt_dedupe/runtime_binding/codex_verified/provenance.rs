//! Durable episode identity; submit witnesses and permission never enter this schema.

use crate::services::tui_o::shadow::SourceId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionProofRef {
    pub owner_runtime_root: String,
    pub tmux_session: String,
    pub execution_nonce: String,
    pub proof_seq: u64,
    pub source: SourceId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnEpisodeRef {
    pub channel_id: u64,
    pub user_message_id: u64,
    pub request_owner_id: u64,
    pub turn_nonce: String,
    pub native_turn_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexEpisodeSpan {
    pub execution: ExecutionProofRef,
    pub episode: TurnEpisodeRef,
    pub delivery_channel_id: u64,
    pub offset_authority_channel_id: u64,
    pub generation_mtime_ns: i64,
    pub start: u64,
    pub end: Option<u64>,
}

/// Reserved episode-wide deny identity; the control owner supplies dispositions when connected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CodexEpisodeDeny {
    pub execution: ExecutionProofRef,
    pub episode: TurnEpisodeRef,
}

#[cfg(test)]
pub(crate) mod dormant;
