use serde_json::Value;
use sqlx::PgPool;

use crate::db::dispatched_session_canonical_identity::{
    CanonicalSessionIdentity, SessionIdentityKind, upsert_hook_session_with_identity_pg,
};
use crate::db::dispatched_sessions::HookSessionUpsert;
use crate::db::dispatched_sessions::hosted_execution::test_support::TOKEN;

/// Seeds the sessions row `claude/<token>/mac-mini:<name>` for `channel_id`;
/// `raw` is its hosted record, `None` for SQL NULL. Returns the row's key.
pub(in crate::services::discord) async fn seed_session_row(
    pool: &PgPool,
    name: &str,
    channel_id: u64,
    raw: Option<Value>,
) -> String {
    let key = format!("claude/{TOKEN}/mac-mini:{name}");
    seed_session_row_keyed(pool, &key, channel_id, raw).await;
    key
}

/// [`seed_session_row`] under a key the caller built.
pub(in crate::services::discord) async fn seed_session_row_keyed(
    pool: &PgPool,
    key: &str,
    channel_id: u64,
    raw: Option<Value>,
) {
    seed_session_row_hashed(pool, key, channel_id, TOKEN, raw).await;
}

/// [`seed_session_row_keyed`] with the channel identity of bot hash `hash`.
pub(in crate::services::discord) async fn seed_session_row_hashed(
    pool: &PgPool,
    key: &str,
    channel_id: u64,
    hash: &str,
    raw: Option<Value>,
) {
    let channel = channel_id.to_string();
    let params = HookSessionUpsert {
        session_key: key,
        instance_id: Some("test-node"),
        agent_id: None,
        provider: "claude",
        status: "idle",
        session_info: None,
        model: None,
        tokens: None,
        cwd: None,
        active_dispatch_id: None,
        thread_channel_id: None,
        channel_id: Some(&channel),
        claude_session_id: None,
        raw_provider_session_id: None,
        turn_start_nonce: None,
        dispatched_origin: false,
    };
    let identity = CanonicalSessionIdentity {
        kind: SessionIdentityKind::DiscordChannel,
        discord_token_hash: hash,
        channel_id: &channel,
    };
    upsert_hook_session_with_identity_pg(pool, params, Some(identity))
        .await
        .unwrap();
    sqlx::query("UPDATE sessions SET hosted_execution = $2 WHERE session_key = $1")
        .bind(key)
        .bind(raw)
        .execute(pool)
        .await
        .unwrap();
}
