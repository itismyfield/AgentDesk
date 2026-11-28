use futures::future::BoxFuture;

use serde_json::Value;
use sqlx::PgPool;

use crate::domain::agent_handoff::AgentHandoffChannelKind;

pub(crate) trait AgentHandoffPort {
    fn start_js_handoff_turn<'a>(
        &'a self,
        pg_pool: &'a PgPool,
        from_agent_id: &'a str,
        to_agent_id: &'a str,
        prompt: &'a str,
        channel_kind: AgentHandoffChannelKind,
        prefix: bool,
    ) -> BoxFuture<'a, Result<Value, (u16, String)>>;
}
