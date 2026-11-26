use super::*;
use crate::services::discord::routine_presentation::process_thread_policy_kind;
use crate::services::tui_o::cutover;
use crate::services::turn_host::TurnHost;

pub(super) async fn authorize(
    request: &replay_start::Request<'_>,
    discord_parent: Option<u64>,
) -> Result<Option<u64>, HeadlessTurnStartError> {
    let metadata = request.metadata;
    if request.source != Some("routine")
        || !routine_metadata::routine_process_turn(metadata)
        || metadata_silent_flag(metadata)
        || request.dm_hint == Some(true)
    {
        return Ok(None);
    }
    let channel = request.channel.get().to_string();
    let Some(metadata) =
        metadata.filter(|value| value["discord_thread_id"].as_str() == Some(channel.as_str()))
    else {
        return Ok(None);
    };
    let Some(kind) = process_thread_policy_kind(request.provider.as_str()) else {
        return Ok(None);
    };
    let Some(hint) = metadata_parent_channel_id(Some(metadata)).map(|parent| parent.get()) else {
        return Ok(None);
    };
    if !cutover::selects_process_parent(hint, kind)
        && !discord_parent.is_some_and(|parent| cutover::selects_process_parent(parent, kind))
    {
        return Ok(None);
    }
    let refuse = || HeadlessTurnStartError::Conflict("routine result thread output is held".into());
    let parent = discord_parent
        .filter(|parent| *parent == hint)
        .ok_or_else(refuse)?;
    let pool = request.shared.pg_pool.as_ref().ok_or_else(refuse)?;
    let run_id = metadata["routine_run_id"].as_str().ok_or_else(refuse)?;
    let owns = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM routine_runs
         WHERE id = $1 AND turn_id = $2 AND action = 'agent' AND status = 'running'
           AND result_json->>'provider' = $3 AND result_json->>'channel_id' = $4
           AND result_json->>'discord_thread_id' = $4 AND result_json->>'parent_channel_id' = $5
           AND result_json->>'execution_mode' = 'process')",
    )
    .bind(run_id)
    .bind(request.reservation.turn_id(request.channel))
    .bind(request.provider.as_str())
    .bind(channel)
    .bind(parent.to_string())
    .fetch_one(pool)
    .await
    .map_err(|err| HeadlessTurnStartError::Internal(err.to_string()))?;
    if !owns {
        return Err(refuse());
    }
    cutover::resolve_process_thread(request.channel.get(), parent, kind)
        .await
        .map_err(|error| HeadlessTurnStartError::Conflict(error.to_string()))?;
    Ok(Some(parent))
}

pub(super) async fn pin(
    request: &replay_start::Request<'_>,
    parent: Option<u64>,
    (host, tmux): (&TurnHost, Option<&str>),
    state: &mut InflightTurnState,
    token: &Arc<CancelToken>,
) -> Result<(), HeadlessTurnStartError> {
    let Some(parent) = parent else { return Ok(()) };
    if matches!(host, TurnHost::Tmux) && tmux.is_none() && state.pin_process_thread(parent).is_ok()
    {
        return Ok(());
    }
    unwind_unstarted_turn(request.shared, request.channel, token).await;
    Err(HeadlessTurnStartError::Conflict(
        "routine result thread no longer has a direct process turn".into(),
    ))
}
