use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Source {
    pub(crate) target: Value,
    pub(crate) provider: String,
    pub(crate) session_id: String,
    pub(crate) transcript: PathBuf,
    pub(crate) file: [u64; 2],
    pub(crate) offset: u64,
    pub(crate) binding_watermark: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Episode {
    pub(crate) user_message_id: u64,
    pub(crate) turn_nonce: String,
    pub(crate) generation: u64,
    #[serde(flatten)]
    pub(crate) extra: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Ticket {
    pub(crate) key: Value,
    pub(crate) channel_id: u64,
    pub(crate) source: Option<Source>,
    pub(crate) owning_episode: Option<Episode>,
    #[serde(flatten)]
    pub(crate) extra: serde_json::Map<String, Value>,
}
