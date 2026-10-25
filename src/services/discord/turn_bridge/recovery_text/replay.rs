//! A legacy scheduler reads the canonical fence before IO and again before enqueue.

use crate::services::discord::{SharedData, replay_policy, router};
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
    if router::replay_producers_dormant(&channel.to_string()) {
        return false;
    }
    let Some(pool) = shared.pg_pool.as_ref() else {
        return true;
    };
    match crate::db::replay_disposition::blocked_receipt_for_sources(
        pool,
        provider.as_str(),
        &channel.to_string(),
        &[source.to_string()],
    )
    .await
    {
        Ok(receipt) => receipt.is_some(),
        Err(error) => {
            tracing::warn!(channel, %error, "legacy replay fence unverified");
            true
        }
    }
}
