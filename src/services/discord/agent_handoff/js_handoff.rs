use futures::future::BoxFuture;
use serde_json::Value;
use sqlx::PgPool;

use super::start_agent_handoff_turn;
use crate::domain::agent_handoff::AgentHandoffChannelKind;
use crate::engine::ops::turn_ops::handoff_port::AgentHandoffPort;
use crate::services::discord::health::HealthRegistry;

impl AgentHandoffPort for HealthRegistry {
    fn start_js_handoff_turn<'a>(
        &'a self,
        pg_pool: &'a PgPool,
        from_agent_id: &'a str,
        to_agent_id: &'a str,
        prompt: &'a str,
        channel_kind: AgentHandoffChannelKind,
        prefix: bool,
    ) -> BoxFuture<'a, Result<Value, (u16, String)>> {
        Box::pin(async move {
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
        })
    }
}
