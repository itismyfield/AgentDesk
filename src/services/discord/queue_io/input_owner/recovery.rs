//! A restarted owner settles a head whose window passed, projects its history, then posts the
//! notices it owes. A projection failure is logged only: it holds nothing and confirms nothing.
use crate::services::tui_input::human_frame::history::v2::Registry;
use crate::services::turn_orchestrator::ChannelMailboxHandle;

/// Adopts the channel's queue file unless its owner already holds it, then settles, projects and
/// notifies.
pub(crate) async fn recover(
    handle: &ChannelMailboxHandle,
    (path, namespace): (std::path::PathBuf, String),
    history: Registry,
    pool: Option<&sqlx::PgPool>,
    (provider, channel, now): (&str, u64, u64),
) -> Result<usize, String> {
    handle.v2_ensure(path, namespace).await?;
    handle
        .v2_request(move |slot| slot.as_mut().ok_or("no input owner")?.tick(now))
        .await?;
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
