use super::*;

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn start_headless_turn(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    shared: &Arc<SharedData>,
    token: &str,
    source: Option<&str>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    start_reserved_headless_turn(
        ctx,
        channel_id,
        prompt,
        request_owner_name,
        shared,
        token,
        source,
        metadata,
        channel_name_hint,
        None,
        None,
        reserve_headless_turn(),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn start_reserved_headless_turn(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    shared: &Arc<SharedData>,
    token: &str,
    source: Option<&str>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    // #5: synthetic tmux-session label for routine turns (see
    // `start_reserved_headless_turn_with_owner`); `None` for all other callers.
    tmux_session_label: Option<String>,
    is_dm_hint: Option<bool>,
    reservation: HeadlessTurnReservation,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    start_reserved_headless_turn_with_owner(
        ctx,
        channel_id,
        prompt,
        request_owner_name,
        UserId::new(1),
        shared,
        token,
        source,
        metadata,
        channel_name_hint,
        tmux_session_label,
        is_dm_hint,
        reservation,
    )
    .await
}

#[allow(dead_code)] // #3034: exported voice entry point, wired-but-dormant (no live dispatch yet).
#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn start_voice_headless_turn(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    request_owner: UserId,
    shared: &Arc<SharedData>,
    token: &str,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    start_reserved_headless_turn_with_owner(
        ctx,
        channel_id,
        prompt,
        request_owner_name,
        request_owner,
        shared,
        token,
        Some(crate::dispatch::Source::Voice.as_str()),
        metadata,
        channel_name_hint,
        None,
        Some(false),
        reserve_headless_turn(),
    )
    .await
}
