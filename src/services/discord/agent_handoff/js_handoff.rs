use serde_json::Value;
use sqlx::PgPool;

use super::start_agent_handoff_turn;
use crate::domain::agent_handoff::AgentHandoffChannelKind;
use crate::engine::ops::turn_ops::handoff_port::AgentHandoffPort;
use crate::services::discord::health::HealthRegistry;

impl AgentHandoffPort for HealthRegistry {
    async fn start_js_handoff_turn(
        &self,
        pg_pool: &PgPool,
        from_agent_id: &str,
        to_agent_id: &str,
        prompt: &str,
        channel_kind: AgentHandoffChannelKind,
        prefix: bool,
    ) -> Result<Value, (u16, String)> {
        start_agent_handoff_turn(
            self,
            pg_pool,
            from_agent_id,
            to_agent_id,
            prompt,
            channel_kind,
            prefix,
            None,
            Some("js:turn.start".to_string()),
            None,
        )
        .await
        .map(|response| response.to_value())
        .map_err(|error| (error.status().as_u16(), error.one_line()))
    }
}
