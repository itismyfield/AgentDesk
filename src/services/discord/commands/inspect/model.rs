use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::db::turns::TurnTokenUsage;
use crate::services::provider::ProviderKind;

#[derive(Debug, Clone)]
pub(super) struct InspectContextConfig {
    pub(super) provider: ProviderKind,
    pub(super) model: Option<String>,
    pub(super) context_window_tokens: u64,
    pub(super) compact_percent: u64,
}

#[derive(Debug, Clone)]
pub(super) struct LatestTurn {
    pub(super) turn_id: String,
    pub(super) channel_id: String,
    pub(super) provider: Option<String>,
    pub(super) session_key: Option<String>,
    pub(super) session_id: Option<String>,
    pub(super) dispatch_id: Option<String>,
    pub(super) finished_at: DateTime<Utc>,
    pub(super) duration_ms: Option<i64>,
    pub(super) input_tokens: u64,
    pub(super) cache_create_tokens: u64,
    pub(super) cache_read_tokens: u64,
}

impl LatestTurn {
    pub(super) fn token_usage(&self) -> TurnTokenUsage {
        TurnTokenUsage {
            input_tokens: self.input_tokens,
            cache_create_tokens: self.cache_create_tokens,
            cache_read_tokens: self.cache_read_tokens,
            output_tokens: 0,
        }
    }

    pub(super) fn context_occupancy_input_tokens(&self) -> u64 {
        self.token_usage().context_occupancy_input_tokens()
    }
}

#[derive(Debug, Clone)]
pub(super) struct LifecycleEventRow {
    pub(super) kind: String,
    pub(super) severity: String,
    pub(super) summary: String,
    pub(super) details_json: Value,
    pub(super) created_at: DateTime<Utc>,
}
