use super::*;

#[allow(clippy::too_many_arguments)]
pub(in crate::services::discord) async fn start_reserved_headless_turn_with_owner(
    ctx: &serenity::Context,
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
    request_owner: UserId,
    shared: &Arc<SharedData>,
    token: &str,
    source: Option<&str>,
    metadata: Option<serde_json::Value>,
    channel_name_hint: Option<String>,
    tmux_session_label: Option<String>,
    is_dm_hint: Option<bool>,
    reservation: HeadlessTurnReservation,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    Box::pin(start_reserved_headless_turn_admitted(
        ctx,
        channel_id,
        prompt,
        request_owner_name,
        request_owner,
        shared,
        token,
        source,
        metadata,
        channel_name_hint,
        tmux_session_label,
        is_dm_hint,
        reservation,
        None,
    ))
    .await
}

/// An owner front kept with a start's provenance starts as its legacy start would, under the
/// owner's claim, so session key, role, workspace and fresh boundary resolve the same way.
pub(in crate::services::discord) async fn start_owner_headless(
    (ctx, shared, token): (&serenity::Context, &Arc<SharedData>, &str),
    channel: ChannelId,
    (prompt, context): (&str, &serde_json::Value),
    owner: super::super::OwnerTurn,
) -> Result<HeadlessTurnStartOutcome, HeadlessTurnStartError> {
    let text = |key: &str| context[key].as_str().map(str::to_owned);
    let source = text("source");
    let name = text("request_owner_name").or(source.clone());
    let request_owner = UserId::new(context["request_owner"].as_u64().unwrap_or(1));
    let metadata = Some(context["metadata"].clone()).filter(|m| !m.is_null());
    let reservation = super::super::super::turn_start::owner_headless_turn(MessageId::new(
        owner.front().1.message_id,
    ));
    Box::pin(start_reserved_headless_turn_admitted(
        ctx,
        channel,
        prompt,
        name.as_deref().unwrap_or("system"),
        request_owner,
        shared,
        token,
        source.as_deref(),
        metadata,
        text("channel_name_hint"),
        text("synthetic_label"),
        context["is_dm"].as_bool(),
        reservation,
        Some(owner),
    ))
    .await
}

pub(in crate::services::discord) async fn start_headless_turn(
    (ctx, shared, token): (&serenity::Context, &Arc<SharedData>, &str),
    channel_id: ChannelId,
    prompt: &str,
    request_owner_name: &str,
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
