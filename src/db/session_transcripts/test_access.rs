//! Test-only entry to the private clear-fence capture for tests hosted outside this module.

use super::ChannelClearFence;
use sqlx::PgPool;

pub(crate) async fn capture_channel_clear_fence_for_test(
    pool: Option<&PgPool>,
    channel_id: &str,
) -> ChannelClearFence {
    super::capture_channel_clear_fence(pool, channel_id).await
}
