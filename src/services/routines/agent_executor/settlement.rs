//! The delivery clock ends at the owner boundary. The existing completion clock starts with
//! the observer, and approved input stays with the owner's normal evidence settlement.
use super::*;
use crate::services::turn_orchestrator::{ChannelMailboxHandle, RoutineInputSettlement};

pub(super) enum Deadline {
    Proceed,
    Hold,
    Closed(RoutineRunOutcome),
}

pub(super) fn pin_delivery_deadline(
    executor: &RoutineAgentExecutor,
    run: &ClaimedRoutineRun,
    context: &mut Value,
) {
    let seconds = run
        .timeout_secs
        .and_then(|seconds| u64::try_from(seconds).ok())
        .filter(|seconds| *seconds > 0)
        .unwrap_or(executor.default_completion_timeout_secs);
    let now = u64::try_from(Utc::now().timestamp_millis()).unwrap_or_default();
    context[ChannelMailboxHandle::ROUTINE_DELIVERY_DEADLINE] =
        json!(now.saturating_add(seconds.saturating_mul(1000)));
}

async fn settle(
    executor: &RoutineAgentExecutor,
    (run_id, turn_id): (&str, &str),
    (raw, owner_confirmed): (&Value, bool),
) -> Result<RoutineInputSettlement> {
    let mut input = ChannelMailboxHandle::restore_owner_input(raw.clone())?;
    if input.context["run_id"] != run_id || input.context["turn_id"] != turn_id {
        return Err(anyhow!("routine owner run/turn mismatch"));
    }
    if owner_confirmed {
        input.admission_committed = Some(true);
    }
    let handle = owner_input::custody::handle(executor, &input.provider, input.channel).await?;
    input
        .settle_delivery(&handle)
        .await
        .map_err(anyhow::Error::msg)
}

pub(super) async fn delivery_deadline(
    executor: &RoutineAgentExecutor,
    store: &RoutineStore,
    run: &RunningAgentRoutineRun,
    pause: bool,
) -> Result<Deadline> {
    let Some((result, turn_id)) = run.result_json.as_ref().zip(run.turn_id.as_deref()) else {
        return Ok(Deadline::Proceed);
    };
    let Some(raw) = result.get("owner_input").filter(|raw| raw.is_object()) else {
        return Ok(Deadline::Proceed);
    };
    if !raw["bind"].is_null() {
        return Ok(Deadline::Proceed);
    }
    let now = Utc::now();
    let due = raw["context"][ChannelMailboxHandle::ROUTINE_DELIVERY_DEADLINE]
        .as_u64()
        .map_or_else(
            || {
                now.signed_duration_since(owner_input::intent_started_at(run))
                    .num_seconds()
                    >= i64::try_from(executor.timeout_secs_for_run(run)).unwrap_or(i64::MAX)
            },
            |deadline| u64::try_from(now.timestamp_millis()).is_ok_and(|now| now >= deadline),
        );
    if !due || !store.run_forbids_retry_after_start(&run.run_id).await? {
        return Ok(Deadline::Proceed);
    }
    let owner_confirmed = result["status"] == "accepted" || result["status"] == "started";
    let (status, error) = match settle(executor, (&run.run_id, turn_id), (raw, owner_confirmed))
        .await
    {
        Ok(RoutineInputSettlement::Withdrawn) => (
            "channel_input_unavailable",
            "routine input delivery deadline expired before the first key",
        ),
        Ok(RoutineInputSettlement::Unconfirmed) => (
            "completion_unconfirmed",
            "routine input delivery could not be confirmed before its deadline",
        ),
        Ok(RoutineInputSettlement::Submitted) => return Ok(Deadline::Proceed),
        Err(error) => {
            tracing::warn!(run_id=%run.run_id, %error, "routine delivery held until exact settlement");
            return Ok(Deadline::Hold);
        }
    };
    let result = merge_pending_result(run, status, Some(error), None);
    if !store
        .close_owner_delivery(run, raw, (error, result.clone(), pause))
        .await?
    {
        return Ok(Deadline::Hold);
    }
    Ok(Deadline::Closed(RoutineRunOutcome {
        run_id: run.run_id.clone(),
        routine_id: run.routine_id.clone(),
        script_ref: run.script_ref.clone(),
        action: "agent".into(),
        status: "failed".into(),
        result_json: Some(result),
        error: Some(error.into()),
        fresh_context_guaranteed: fresh_context_guaranteed_from_result(run.result_json.as_ref()),
    }))
}

impl RoutineAgentExecutor {
    pub(crate) async fn recover_stale_running_runs(
        &self,
        store: &RoutineStore,
    ) -> Result<Vec<RecoveredRoutineRun>> {
        if let Err(error) = store.record_late_owner_completions().await {
            tracing::warn!(%error, "routine late completion evidence remains pending");
        }
        let mut recovered = store.recover_stale_running_runs().await?;
        let pending = match store.begin_owner_recovery().await {
            Ok(pending) => pending,
            Err(error) => {
                tracing::warn!(%error, "routine owner recovery could not read the pending runs");
                return Ok(recovered);
            }
        };
        for run in pending {
            let run_id = run.identity().0.to_owned();
            let (status, error) = match settle(
                self,
                run.identity(),
                (run.owner_input(), run.owner_confirmed),
            )
            .await
            {
                Ok(RoutineInputSettlement::Withdrawn) => (
                    "interrupted",
                    "routine interrupted by expired lease before the first key",
                ),
                Ok(RoutineInputSettlement::Submitted | RoutineInputSettlement::Unconfirmed) => (
                    "completion_unconfirmed",
                    "routine interrupted by expired lease after input approval",
                ),
                Err(error) => {
                    tracing::warn!(run_id=run.identity().0, %error, "routine recovery awaits exact owner settlement");
                    continue;
                }
            };
            match store.finish_owner_recovery(run, (status, error)).await {
                Ok(Some(run)) => recovered.push(run),
                Ok(None) => {}
                Err(error) => {
                    tracing::warn!(%run_id, %error, "routine owner recovery awaits the database commit")
                }
            }
        }
        Ok(recovered)
    }
}
