//! Completes only a positively ended predecessor, without sending any terminal mutation.
use super::http_force::Expected;
use super::*;

pub(in crate::services::discord) async fn expected_for_restart(
    shared: &Arc<SharedData>,
    record: &HostedExecution,
    dispatch: Option<String>,
) -> Result<Expected, TerminationResult> {
    let provider = ProviderKind::from_str(&record.owner.provider)
        .ok_or_else(|| TerminationResult::Refused("unknown provider".into()))?;
    let channel = record
        .owner
        .channel_id
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .map(ChannelId::new)
        .ok_or_else(|| TerminationResult::Refused("unknown channel".into()))?;
    let captured = capture_turn(shared, &provider, channel)
        .await
        .map_err(TerminationResult::Refused)?;
    Ok(Expected {
        owner: record.owner.clone(),
        captured_dispatch: captured.as_ref().and_then(|turn| turn.dispatch_id.as_ref())
            == dispatch.as_ref(),
        dispatch,
    })
}

/// `None` is a positively confirmed live execution; only the command may then request termination.
pub(in crate::services::discord) async fn recover_ended(
    request: OperatorTerminate,
    expected: Expected,
    shared: Arc<SharedData>,
    pool: &PgPool,
) -> Result<Option<RetiredExclusion>, TerminationResult> {
    use TerminationResult::{Indeterminate, Refused};
    if !terminate_enabled() {
        return Err(TerminationResult::Disabled);
    }
    let row = load(pool, &request.session_key).await?;
    let HostedRecord::Known(record) = &row.record else {
        return Err(Refused("unknown restart execution".into()));
    };
    if record.owner != expected.owner
        || record.execution_nonce != request.execution_nonce
        || record.owner.discord_token_hash != shared.token_hash
        || Some(record.owner.owner_node.clone()) != crate::config::session_hosts::local_node()
        || record
            .location
            .as_ref()
            .is_none_or(|at| at.execution_node != record.owner.owner_node)
    {
        return Err(Refused("restart execution identity differs".into()));
    }
    let provider = ProviderKind::from_str(&record.owner.provider)
        .ok_or_else(|| Refused("unknown provider".into()))?;
    let channel = record
        .owner
        .channel_id
        .parse::<u64>()
        .ok()
        .filter(|id| *id != 0)
        .map(ChannelId::new)
        .ok_or_else(|| Refused("unknown channel".into()))?;
    let retired = record.state == HostedState::Retired;
    let target = if retired {
        None
    } else {
        let target = herdr_endpoints()
            .target(record)
            .ok_or_else(|| Refused("missing launch evidence".into()))?;
        match execution_end(target.clone(), record.clone()).await {
            ExecutionEnd::Ended => {}
            ExecutionEnd::ProviderAlive => return Err(TerminationResult::ProviderSurvivedClose),
            ExecutionEnd::Unconfirmed => {
                let read = target.clone();
                return match tokio::task::spawn_blocking(move || read.execution_confirmed()).await {
                    Ok(true) => Ok(None),
                    _ => Err(Indeterminate("restart execution end unconfirmed".into())),
                };
            }
        }
        Some(target)
    };
    let turn = capture_turn(&shared, &provider, channel)
        .await
        .map_err(Refused)?;
    let transition = shared
        .acquire_session_transition(channel)
        .await
        .map_err(|_| Refused("transition busy".into()))?;
    let actor = shared.mailbox_peek(channel);
    let same_turn = match (&turn, &actor) {
        (None, None) => true,
        (None, Some(now)) => matches!(now.cancel_token().await, Ok(None)),
        (Some(turn), Some(now)) => {
            now.same_actor(&turn.mailbox)
                && matches!(now.cancel_token().await, Ok(Some(active)) if Arc::ptr_eq(&active, &turn.token))
        }
        (Some(_), None) => false,
    };
    if !same_turn
        || expected.captured_dispatch
            && turn.as_ref().and_then(|turn| turn.dispatch_id.as_ref())
                != expected.dispatch.as_ref()
    {
        return Err(Refused("restart turn changed under transition".into()));
    }
    let fence = match &target {
        Some(target) => Arc::new(
            target
                .termination_fence()
                .map_err(|error| Refused(format!("restart fence: {error:?}")))?,
        ),
        None => {
            let probe = herdr_endpoints()
                .retired_probe(record)
                .ok_or_else(|| Refused("missing retired endpoint".into()))?;
            let (fence, proof) = tokio::task::spawn_blocking(move || probe.confirm())
                .await
                .map_err(|error| Indeterminate(error.to_string()))?
                .map_err(|error| Indeterminate(format!("retired restart proof: {error:?}")))?;
            if !proof.matches(record) {
                return Err(Refused("retired execution changed".into()));
            }
            Arc::new(fence)
        }
    };
    let unsent = UnsentClose((!retired).then(|| fence.clone()));
    #[cfg(test)]
    run_probe(&AFTER_FENCE).await;
    if load(pool, &request.session_key).await.as_ref() != Ok(&row)
        || !http_force::dispatch_holds(pool, &row, Some(&expected)).await
    {
        return Err(Refused("restart row changed under fence".into()));
    }
    if let Some(target) = target {
        if execution_end(target, record.clone()).await != ExecutionEnd::Ended {
            return Err(Indeterminate("restart end changed under fence".into()));
        }
        if load(pool, &request.session_key).await.as_ref() != Ok(&row)
            || !http_force::dispatch_holds(pool, &row, Some(&expected)).await
        {
            return Err(Refused("restart row changed after end proof".into()));
        }
        unsent.disarm();
        retire_confirmed(pool, &row, record).await?;
    }
    let token = turn.as_ref().map(|turn| turn.token.clone());
    let held = settle_and_release(
        &shared,
        &provider,
        &record.execution_nonce,
        turn,
        (transition, fence),
    )
    .await?;
    Ok(Some(RetiredExclusion::new(
        (held.0, held.1, actor, token),
        row,
        expected
            .captured_dispatch
            .then_some(expected.dispatch)
            .flatten(),
        provider,
        channel,
    )))
}

#[cfg(all(test, unix))]
#[path = "restart_recovery_tests.rs"]
mod tests;
