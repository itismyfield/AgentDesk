use std::future::Future;

use serde_json::Value;
use sqlx::PgPool;

use crate::domain::agent_handoff::AgentHandoffChannelKind;

pub(crate) trait AgentHandoffPort {
    fn start_js_handoff_turn(
        &self,
        pg_pool: &PgPool,
        from_agent_id: &str,
        to_agent_id: &str,
        prompt: &str,
        channel_kind: AgentHandoffChannelKind,
        prefix: bool,
    ) -> impl Future<Output = Result<Value, (u16, String)>> + Send;
}
