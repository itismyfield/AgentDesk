//! One exhaustive start consumer; owner custody stays running until an exact observer is bound.
use super::*;
use crate::services::discord::HeadlessTurnStartStatus;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum AgentStartDisposition {
    Pending,
    Accepted,
    Started,
    Consumed,
}

pub(super) fn disposition(status: HeadlessTurnStartStatus) -> AgentStartDisposition {
    match status {
        #[cfg(test)]
        HeadlessTurnStartStatus::Accepted => AgentStartDisposition::Accepted,
        HeadlessTurnStartStatus::Started => AgentStartDisposition::Started,
        HeadlessTurnStartStatus::Consumed => AgentStartDisposition::Consumed,
    }
}

fn stored_disposition(result: Option<&Value>) -> Option<AgentStartDisposition> {
    match result?.get("status")?.as_str()? {
        "input_pending" => Some(AgentStartDisposition::Pending),
        "accepted" => Some(AgentStartDisposition::Accepted),
        _ => None,
    }
}

pub(super) fn needs_owner(run: &RunningAgentRoutineRun) -> bool {
    stored_disposition(run.result_json.as_ref()).is_some()
        || run
            .result_json
            .as_ref()
            .is_some_and(|r| r.get("owner_input").is_some())
}

pub(super) async fn reconcile_run(
    executor: &RoutineAgentExecutor,
    store: &RoutineStore,
    run: RunningAgentRoutineRun,
) -> Result<Option<RunningAgentRoutineRun>> {
    if !needs_owner(&run) {
        return Ok(Some(run));
    }
    #[cfg(test)]
    {
        match custody::may_observe(executor, store, &run).await {
            Ok(true) => {}
            Ok(false) => return Ok(None),
            Err(error) => {
                tracing::warn!(run_id=%run.run_id,%error,"owner run held without blocking the remaining poll batch");
                return Ok(None);
            }
        }
        let mut bound = run;
        bound.started_at = intent_started_at(&bound);
        Ok(Some(bound))
    }
    #[cfg(not(test))]
    {
        let _ = (executor, store);
        Ok(None)
    }
}

#[cfg(test)]
fn intent_started_at(run: &RunningAgentRoutineRun) -> DateTime<Utc> {
    run.attempts
        .as_array()
        .and_then(|events| {
            events.iter().rev().find(|event| {
                event["event"] == "input_pending"
                    && event["turn_id"].as_str() == run.turn_id.as_deref()
            })
        })
        .and_then(|event| event["at"].as_str())
        .and_then(|at| at.parse::<DateTime<Utc>>().ok())
        .unwrap_or(run.started_at)
}

pub(super) async fn retry_allowed(
    executor: &RoutineAgentExecutor,
    run: &RunningAgentRoutineRun,
) -> bool {
    if !needs_owner(run) {
        return true;
    }
    #[cfg(test)]
    {
        match custody::retry_allowed(executor, run).await {
            Ok(allowed) => allowed,
            Err(error) => {
                tracing::warn!(run_id=%run.run_id,%error,"owner retry held until the exact attempt settles");
                false
            }
        }
    }
    #[cfg(not(test))]
    {
        let _ = executor;
        false
    }
}

pub(super) struct ConsumedRunState<'a> {
    pub(super) checkpoint: Option<Value>,
    pub(super) next_due_at: Option<DateTime<Utc>>,
    pub(super) last_result: Option<&'a str>,
}

impl RoutineAgentExecutor {
    pub(super) async fn consume_start(
        &self,
        store: &RoutineStore,
        claimed: &ClaimedRoutineRun,
        start: StartedAgentTurn,
        error: Option<String>,
        completion: ConsumedRunState<'_>,
    ) -> Result<Option<RoutineRunOutcome>> {
        let status =
            match start.disposition {
                AgentStartDisposition::Pending
                | AgentStartDisposition::Accepted
                | AgentStartDisposition::Started => "running",
                AgentStartDisposition::Consumed => {
                    if !store
                        .complete_agent_run(
                            &claimed.run_id,
                            Some(start.result_json.clone()),
                            completion.checkpoint,
                            Some(completion.last_result.unwrap_or(
                                "headless command consumed without starting an agent turn",
                            )),
                            completion
                                .next_due_at
                                .map_or(NextDueAtUpdate::Preserve, NextDueAtUpdate::Set),
                        )
                        .await?
                    {
                        return Ok(None);
                    }
                    "succeeded"
                }
            };
        Ok(Some(RoutineRunOutcome {
            run_id: claimed.run_id.clone(),
            routine_id: claimed.routine_id.clone(),
            script_ref: claimed.script_ref.clone(),
            action: "agent".into(),
            status: status.into(),
            result_json: Some(start.result_json),
            error,
            fresh_context_guaranteed: start.fresh_context_guaranteed,
        }))
    }
}

#[cfg(test)]
pub(super) mod custody {
    use super::*;
    use crate::services::turn_orchestrator::{ChannelMailboxHandle, PendingQueueItem};

    async fn handle(
        executor: &RoutineAgentExecutor,
        provider: &str,
        channel: u64,
    ) -> Result<ChannelMailboxHandle> {
        if let Ok(handle) = super::super::owner_input_tests::OWNER.try_with(Clone::clone) {
            return Ok(handle);
        }
        let registry = executor
            .health_registry
            .as_deref()
            .ok_or_else(|| anyhow!("owner runtime unavailable"))?;
        ChannelMailboxHandle::owner_input_mailbox(registry, provider, channel)
            .await
            .map_err(anyhow::Error::msg)
    }

    pub(in crate::services::routines::agent_executor) async fn start(
        store: &RoutineStore,
        start: &start_deferral::PendingAgentStart<'_>,
        handle: &ChannelMailboxHandle,
        original: PendingQueueItem,
        namespace: &str,
        mut planned: Value,
    ) -> Result<StartedAgentTurn> {
        if start.execution_mode == execution::AgentExecutionMode::Process {
            return Err(anyhow!("process start keeps its legacy route"));
        }
        planned["status"] = json!("input_pending");
        planned["origin"] = json!("routine");
        planned["fresh_context_guaranteed"] = json!(false);
        let turn_id = planned["turn_id"]
            .as_str()
            .ok_or_else(|| anyhow!("resolved turn missing"))?
            .to_owned();
        let provider = planned["provider"]
            .as_str()
            .ok_or_else(|| anyhow!("resolved provider missing"))?
            .to_owned();
        let (previous_turn, previous) = store.agent_input_intent(&start.claimed.run_id).await?;
        let (mut result, input) = if let Some(previous) = previous.filter(|v| {
            v.get("owner_input").is_some()
                && !(v["status"] == "started"
                    && v.get("owner_input")
                        .and_then(|i| i.get("bind"))
                        .is_some_and(|bind| !bind.is_null())
                    && v["attempt_kind"].as_str() != Some(start.attempt_kind))
        }) {
            let input = ChannelMailboxHandle::restore_owner_input(previous["owner_input"].clone())?;
            (previous, input)
        } else {
            let claimed = start.claimed;
            let result = planned;
            let input = ChannelMailboxHandle::new_owner_input(
                &original,
                namespace,
                &provider,
                result.clone(),
            );
            let mut intent = result;
            intent["owner_input"] = serde_json::to_value(&input)?;
            if !store
                .record_agent_input_intent(
                    &claimed.run_id,
                    previous_turn.as_deref(),
                    &turn_id,
                    &intent,
                )
                .await?
            {
                return Err(anyhow!("routine input intent lost its exact-run fence"));
            }
            (intent, input)
        };
        let disposition = match input.confirm_admission(handle).await {
            Ok(true) => {
                result["status"] = json!("accepted");
                if let Err(error) = store
                    .project_agent_input(
                        &start.claimed.run_id,
                        result["turn_id"].as_str().unwrap_or_default(),
                        &input.ticket,
                        &result,
                    )
                    .await
                {
                    tracing::warn!(%error,"owner accepted; routine projection will reconcile the same ticket");
                }
                AgentStartDisposition::Accepted
            }
            Ok(false) => AgentStartDisposition::Pending,
            Err(reason) => {
                tracing::warn!(%reason,"routine intent held after owner refusal");
                AgentStartDisposition::Pending
            }
        };
        Ok(StartedAgentTurn {
            result_json: result,
            disposition,
            fresh_context_guaranteed: false,
        })
    }

    pub(in crate::services::routines::agent_executor) async fn may_observe(
        executor: &RoutineAgentExecutor,
        store: &RoutineStore,
        run: &RunningAgentRoutineRun,
    ) -> Result<bool> {
        let Some(result) = run.result_json.as_ref() else {
            return Ok(false);
        };
        let Some(raw) = result.get("owner_input") else {
            return Ok(false);
        };
        let input = ChannelMailboxHandle::restore_owner_input(raw.clone())?;
        if input.context["run_id"] != run.run_id
            || input.context["turn_id"].as_str() != run.turn_id.as_deref()
        {
            return Err(anyhow!("routine owner run/turn mismatch"));
        }
        let handle = handle(executor, &input.provider, input.channel).await?;
        // The observer connects to the owner's exact attempt first; a failed projection is
        // repaired under the same ticket on the next poll.
        if let Some((attempt, source)) = input
            .unbound_attempt(&handle)
            .await
            .map_err(anyhow::Error::msg)?
        {
            bind_observer(store, run, &handle, &attempt, source).await?;
            return Ok(false);
        }
        let may_observe = input
            .may_observe(&handle, result["status"] == "input_pending")
            .await
            .map_err(anyhow::Error::msg)?;
        if result["status"] == "input_pending"
            && handle
                .v2_admission_status(input.ticket.clone())
                .await
                .map_err(anyhow::Error::msg)?
                .committed()
        {
            let mut accepted = result.clone();
            accepted["status"] = json!("accepted");
            store
                .project_agent_input(
                    &run.run_id,
                    run.turn_id.as_deref().unwrap_or_default(),
                    &input.ticket,
                    &accepted,
                )
                .await?;
        }
        Ok(may_observe)
    }

    pub(in crate::services::routines::agent_executor) async fn retry_allowed(
        executor: &RoutineAgentExecutor,
        run: &RunningAgentRoutineRun,
    ) -> Result<bool> {
        let input = ChannelMailboxHandle::restore_owner_input(
            run.result_json
                .as_ref()
                .ok_or_else(|| anyhow!("missing input intent"))?["owner_input"]
                .clone(),
        )?;
        let handle = handle(executor, &input.provider, input.channel).await?;
        input.may_retry(&handle).await.map_err(anyhow::Error::msg)
    }

    pub(in crate::services::routines::agent_executor) async fn bind_observer(
        store: &RoutineStore,
        run: &RunningAgentRoutineRun,
        handle: &ChannelMailboxHandle,
        attempt: &str,
        source: Option<Value>,
    ) -> Result<bool> {
        let mut result = run
            .result_json
            .clone()
            .ok_or_else(|| anyhow!("missing input intent"))?;
        let mut input = ChannelMailboxHandle::restore_owner_input(result["owner_input"].clone())?;
        let turn_id = run
            .turn_id
            .as_deref()
            .ok_or_else(|| anyhow!("missing logical turn"))?;
        input
            .bind_observer(handle, attempt, source, turn_id)
            .await
            .map_err(anyhow::Error::msg)?;
        result["owner_input"] = serde_json::to_value(&input)?;
        result["status"] = json!("started");
        store
            .project_agent_input(&run.run_id, turn_id, &input.ticket, &result)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn headless_outcomes_preserve_all_three_dispositions() {
        assert_eq!(
            disposition(HeadlessTurnStartStatus::Accepted),
            AgentStartDisposition::Accepted
        );
        assert_eq!(
            disposition(HeadlessTurnStartStatus::Started),
            AgentStartDisposition::Started
        );
        assert_eq!(
            disposition(HeadlessTurnStartStatus::Consumed),
            AgentStartDisposition::Consumed
        );
    }
    #[test]
    fn old_started_runs_keep_the_legacy_poll_path() {
        let mut run = super::super::tests::running_run(None);
        run.result_json = Some(json!({"status":"started","turn_id":"old-turn"}));
        assert!(!needs_owner(&run));
        for status in ["input_pending", "accepted"] {
            run.result_json = Some(json!({"status":status,"turn_id":"held-turn"}));
            assert!(
                needs_owner(&run),
                "an absent owner association cannot become a legacy launch"
            );
        }
    }
}
