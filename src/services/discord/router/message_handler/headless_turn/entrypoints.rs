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
    let context = owner_start_context(shared, context).await;
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

/// A kept start's context as its headless start reads it: a scheduled start kept before its
/// provenance had keys of its own holds its metadata flat, and gets its label and hint here.
async fn owner_start_context(
    shared: &Arc<SharedData>,
    context: &serde_json::Value,
) -> serde_json::Value {
    let mut context = context.clone();
    if context["origin"] == "scheduled" && context["metadata"].is_null() {
        context["metadata"] = context.clone();
        let agent = context["agent_id"].as_str().unwrap_or_default();
        let bound = crate::db::agents::load_agent_channel_bindings_pg;
        if let Some(pool) = &shared.pg_pool
            && let Ok(Some(bound)) = bound(pool, agent).await
        {
            let hint = bound.channel_for_provider(Some(shared.provider.as_str()));
            context["channel_name_hint"] = serde_json::json!(hint);
        }
        context["synthetic_label"] = context["scheduled_snapshot_session_label"].clone();
        context["source"] = crate::services::scheduled_messages::OUTBOX_SOURCE.into();
        context["is_dm"] = false.into();
    }
    context
}

/// A start whose pane its channel's name gives, while the lookup of that name failed.
#[derive(Debug)]
pub(in crate::services::discord) struct Unnamed;

/// The tmux pane a kept start's headless turn takes, named as that turn names it, and whether it is
/// the channel's own pane; `None` for a process routine or a provider with no managed pane.
pub(in crate::services::discord) async fn owner_start_pane(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    context: &serde_json::Value,
) -> Result<Option<(String, bool)>, Unnamed> {
    let context = owner_start_context(shared, context).await;
    let metadata = Some(context["metadata"].clone()).filter(|m| !m.is_null());
    let hint = context["channel_name_hint"].as_str().map(str::to_owned);
    let channel_name = start_channel_name(shared, channel, hint).await;
    let label = context["synthetic_label"].as_str();
    let named = channel_name.as_ref().ok().and_then(Option::as_deref);
    let provider = &shared.provider;
    let Some(pane) = routine_tmux_session_name(provider, metadata.as_ref(), label, named) else {
        let process = super::routine_metadata::routine_process_turn(metadata.as_ref());
        let takes_pane = provider.uses_managed_tmux_backend() && !process;
        return match channel_name {
            Err(unnamed) if takes_pane => Err(unnamed),
            _ => Ok(None),
        };
    };
    let own = named.is_some_and(|name| provider.build_tmux_session_name(name) == pane);
    Ok(Some((pane, own)))
}

/// The channel name a headless start names its pane by: the session's, else its hint, else the
/// name Discord gives the channel, else its registered fallback; `Err` when Discord's lookup
/// failed and no fallback names it.
async fn start_channel_name(
    shared: &Arc<SharedData>,
    channel: ChannelId,
    hint: Option<String>,
) -> Result<Option<String>, Unnamed> {
    let named = {
        let data = shared.core.lock().await;
        let session = data.sessions.get(&channel);
        session.and_then(|session| session.channel_name.clone())
    };
    if let Some(name) = named.or(hint) {
        return Ok(Some(name));
    }
    // A guild channel has the name `resolve_channel_category` gives it; any other has none.
    let looked_up = match shared.http.cached_serenity_ctx.get() {
        Some(ctx) => match channel.to_channel(&ctx.http).await {
            Ok(serenity::model::channel::Channel::Guild(channel)) => Ok(Some(channel.name)),
            Ok(_) => Ok(None),
            Err(_) => Err(Unnamed),
        },
        None => Ok(None),
    };
    let registered = crate::services::discord::adk_session::registered_channel_fallback_name;
    match (looked_up, registered(channel, &shared.provider)) {
        (Ok(Some(name)), _) | (_, Some(name)) => Ok(Some(name)),
        (looked_up, None) => looked_up,
    }
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
