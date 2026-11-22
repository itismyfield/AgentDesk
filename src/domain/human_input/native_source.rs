use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub target: Value,
    pub provider: String,
    pub session_id: String,
    pub transcript: PathBuf,
    pub file: [u64; 2],
    pub offset: u64,
    pub binding_watermark: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    pub user_message_id: u64,
    pub turn_nonce: String,
    pub generation: u64,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ticket {
    pub key: Value,
    pub channel_id: u64,
    pub source: Option<Source>,
    pub owning_episode: Option<Episode>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}
