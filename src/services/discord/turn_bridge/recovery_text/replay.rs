//! A legacy scheduler reads the canonical fence before IO and again before enqueue.

use crate::services::discord::{SharedData, replay_policy};
use crate::services::provider::ProviderKind;

pub(in crate::services::discord) async fn blocked(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: u64,
    source: u64,
) -> bool {
    if replay_policy::legacy_retry_barred(channel, source) {
        return true;
    }
    let Some(_pool) = shared.pg_pool.as_ref() else {
        return false;
    };
    loop {
        match lookup(shared, provider, channel, source).await {
            Ok(blocked) => return blocked,
            Err(error) => {
                tracing::warn!(channel, %error, "legacy replay read unverified; retrying without executing");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    }
}

pub(in crate::services::discord) async fn lookup(
    shared: &SharedData,
    provider: &ProviderKind,
    channel: u64,
    source: u64,
) -> Result<bool, sqlx::Error> {
    let Some(pool) = shared.pg_pool.as_ref() else {
        return Ok(false);
    };
    crate::db::replay_disposition::blocked_receipt_for_sources(
        pool,
        provider.as_str(),
        &channel.to_string(),
        &[source.to_string()],
    )
    .await
    .map(|r| r.is_some())
}
