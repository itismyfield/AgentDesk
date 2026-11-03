//! A restarted owner settles a head whose window passed, requeues proven-unsent retained inputs,
//! projects its history, then posts the notices it owes. A projection failure is only logged.
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
    // Requeued before the notice drain, so an input delivered next never gets a late resend ask.
    let told = handle
        .v2_request(|slot| {
            let moved = slot
                .as_mut()
                .ok_or("no input owner")?
                .requeue_known_not_sent()?;
            let told = moved.iter().filter(|entry| entry.notice.enqueued);
            told.map(|entry| serde_json::to_value(&entry.input.identity).map_err(|e| e.to_string()))
                .collect::<Result<Vec<_>, String>>()
        })
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
    let Some(pool) = pool else {
        return Ok(0);
    };
    for identity in &told {
        crate::services::discord::queue_io::retained_notice::requeued(
            pool, provider, channel, identity,
        )
        .await?;
    }
    super::notice::drain(pool, handle, provider, channel).await
}

/// When the owner's head is next due to be retained, for a backstop that wakes once at that time
/// instead of polling; `None` while nothing is due.
pub(crate) async fn settle_due(handle: &ChannelMailboxHandle) -> Result<Option<u64>, String> {
    handle
        .v2_request(|slot| Ok(slot.as_ref().and_then(|owner| owner.settle_due())))
        .await
}
