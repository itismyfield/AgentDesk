//! A restarted owner projects its history, then posts the notices it owes. A projection failure is
//! logged only: it holds neither the notices nor the next input, and it confirms nothing.
use crate::services::tui_input::human_frame::history::v2::Registry;
use crate::services::turn_orchestrator::ChannelMailboxHandle;

/// Adopts the channel's queue file unless an owner already holds it, then projects and notifies.
pub(crate) async fn recover(
    handle: &ChannelMailboxHandle,
    (path, namespace): (std::path::PathBuf, String),
    history: Registry,
    pool: Option<&sqlx::PgPool>,
    (provider, channel): (&str, u64),
) -> Result<usize, String> {
    if !super::ingress::installed(handle).await? {
        handle.v2_adopt(path, namespace).await?;
    }
    let synced = handle
        .v2_request(move |slot| {
            slot.as_ref()
                .ok_or("no input owner")?
                .sync_history(&history)
        })
        .await;
    if let Err(error) = synced {
        tracing::warn!(%error, channel, "input owner history projection failed after restart");
    }
    match pool {
        Some(pool) => super::notice::drain(pool, handle, provider, channel).await,
        None => Ok(0),
    }
}
