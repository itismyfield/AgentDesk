//! A non-forced cancel of a live Herdr turn this process started runs as the holder's `/stop`;
//! it reports that answer and leaves the turn, its dispatch and its queue to the provider's end.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::services::discord::health::HealthRegistry;
use crate::services::turn_lifecycle::TurnLifecycleTarget;

/// The response to a non-forced cancel of a live Herdr turn: the holder's answer, never a
/// completion; `None` leaves every other turn, and any forced cancel, to the existing cancel.
pub(super) async fn live_turn(
    force: bool,
    health_registry: Option<&Arc<HealthRegistry>>,
    target: &TurnLifecycleTarget,
    session_key: Option<&str>,
) -> Option<Value> {
    if force {
        return None;
    }
    let (registry, provider) = (health_registry?, target.provider.as_ref()?);
    let channel = target.channel_id?.get();
    let stop = crate::services::discord::zombie_foreground_release::run_herdr_cancel;
    let mut answer = stop(registry, provider, channel).await?;
    // Only an admitted intent shows the token still held the mailbox; nothing else was observed.
    let stopping = matches!(
        answer["intent"].as_str(),
        Some("recorded" | "already_recorded")
    );
    tracing::info!(
        channel_id = channel,
        provider = provider.as_str(),
        %answer,
        "[queue-api] cancel took the Herdr holder stop"
    );
    let fields = json!({
        "ok": true,
        "channel_id": channel.to_string(),
        "session_key": session_key,
        "turn_status": if stopping { "stopping" } else { "refused" },
        "turn_completed_at": null,
        "inflight_cleared": false,
        "dispatch_cancelled": null,
        "mailbox_foreground_free": stopping.then_some(false),
        "queued_remaining": null,
    });
    if let (Some(answer), Value::Object(fields)) = (answer.as_object_mut(), fields) {
        answer.extend(fields);
    }
    Some(answer)
}
