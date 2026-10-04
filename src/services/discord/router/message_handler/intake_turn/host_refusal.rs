use super::*;

/// Refuses a turn on a Herdr-configured channel before it resets, reconciles or clears anything;
/// `Err` when its input has no session to go back to, so the caller fails the request instead.
pub(super) async fn admitted_uploads(
    http: &Arc<serenity::http::Http>,
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
    (channel_id, original_channel_id): (ChannelId, ChannelId),
    (uploads, was_cleared): (
        crate::services::cluster::attachment_transfer::uploads::PendingUploads,
        Option<bool>,
    ),
) -> Result<Option<crate::services::cluster::attachment_transfer::uploads::PendingUploads>, Error> {
    let session_key = || build_adk_session_key(shared, channel_id, provider, None);
    let pool = shared.pg_pool.as_ref();
    let judged = crate::services::turn_host::refusal_before_turn(
        pool,
        provider,
        channel_id.get(),
        session_key,
    );
    let Some(refusal) = judged.await else {
        return Ok(Some(uploads));
    };
    tracing::warn!(channel_id = channel_id.get(), "{refusal}");
    rate_limit_wait(shared, original_channel_id).await;
    if let Err(error) = original_channel_id.say(http, refusal.to_string()).await {
        tracing::warn!(channel_id = channel_id.get(), %error, "herdr refusal notice failed");
    }
    let input = match was_cleared {
        Some(_) => (uploads, was_cleared),
        // Nothing was taken from the session yet, so its own uploads keep leading.
        None => {
            let take = pre_admission_control::take_channel_input_state;
            let (mut taken, cleared) = take(shared, original_channel_id).await;
            taken.extend(uploads);
            (taken, Some(cleared))
        }
    };
    let restore = super::super::super::super::admin_host_guard::return_intake_input;
    if !restore(shared, original_channel_id, input).await {
        return Err(refusal.to_string().into());
    }
    Ok(None)
}
