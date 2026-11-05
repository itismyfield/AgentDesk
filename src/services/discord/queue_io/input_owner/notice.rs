//! Each retained original owes one outbox notice; a local submit or a missing receipt owes none.
use crate::services::turn_orchestrator::ChannelMailboxHandle;
use serde_json::{Value, json};

/// Posts every owed retained notice and marks the attempted ones; the outbox's persistent dedupe
/// keeps a repeat after a crash, a restart or an attemptless entry to one post.
pub(crate) async fn drain(
    pool: &sqlx::PgPool,
    handle: &ChannelMailboxHandle,
    provider: &str,
    channel: u64,
) -> Result<usize, String> {
    let (namespace, owed) = handle
        .v2_request(|slot| {
            let state = slot.as_ref().ok_or("no input owner")?.json()?;
            let namespace = state["namespace"].as_str().ok_or("missing namespace")?;
            let retained = state["retained_inputs"].as_array().into_iter().flatten();
            let owed: Vec<Value> = retained
                .filter(|entry| entry["notice"]["enqueued"] != true)
                .cloned()
                .collect();
            Ok((namespace.to_string(), owed))
        })
        .await?;
    let told = tell(pool, handle, provider, channel, &namespace, &owed).await;
    // A told proven-unsent input, or a removal still owed, gets its receipt even when a notice failed.
    let channel_id = poise::serenity_prelude::ChannelId::new(channel);
    crate::services::discord::settle_v2_drops(Some(pool), handle, provider, channel_id).await;
    told.map(|()| owed.len())
}

async fn tell(
    pool: &sqlx::PgPool,
    handle: &ChannelMailboxHandle,
    provider: &str,
    channel: u64,
    namespace: &str,
    owed: &[Value],
) -> Result<(), String> {
    let notice = crate::services::discord::queue_io::retained_notice::enqueue;
    for entry in owed {
        notice(pool, provider, channel, namespace, entry).await?;
        if entry["input"]["attempt"].is_null() {
            let identity = serde_json::from_value(entry["input"]["identity"].clone())
                .map_err(|e| e.to_string())?;
            handle
                .v2_request(move |slot| {
                    slot.as_mut()
                        .ok_or("no input owner")?
                        .attemptless_notice_enqueued(identity)
                })
                .await?;
            continue;
        }
        let key =
            json!({"identity": entry["input"]["identity"], "attempt": entry["input"]["attempt"]});
        handle
            .v2_request(move |slot| {
                let key = serde_json::from_value(key).map_err(|e| e.to_string())?;
                slot.as_mut().ok_or("no input owner")?.notice_enqueued(&key)
            })
            .await?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
#[path = "notice_tests.rs"]
mod tests;
