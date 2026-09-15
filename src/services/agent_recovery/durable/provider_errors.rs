//! Error takeover uses the same fenced transaction as watchdog recovery.
use super::*;

pub(crate) async fn observe_provider_error(
    lease: &RecoveryLease,
    input: ObserveInput,
    checkpoint: CheckpointPayload,
    workspace: Option<String>,
) -> Result<bool, RecoveryStoreError> {
    Ok(coordinator()
        .transition(&lease.channel_id, |runtime| {
            apply_provider_error(runtime, lease, input, checkpoint, workspace)
        })
        .await?
        .is_some())
}

pub(super) fn apply_provider_error(
    runtime: &mut RecoveryRuntime,
    lease: &RecoveryLease,
    input: ObserveInput,
    checkpoint: CheckpointPayload,
    workspace: Option<String>,
) -> Result<Option<()>, RecoveryStoreError> {
    let Some(binding) = runtime.catalog.channels.get(&lease.channel_id).cloned() else {
        return Ok(None);
    };
    if input.channel_id != lease.channel_id
        || binding.owner_agent_id != lease.active_writer_agent_id
        || classify_trigger(&input.signal, binding.policy.as_ref()).is_none()
    {
        return Ok(None);
    }
    if let Some(state) = runtime.states.get(&lease.channel_id) {
        if state.lock_held() || RecoveryLease::from_state(state) != *lease {
            return Err(conflict(
                "provider error no longer owns its original turn lease",
            ));
        }
        // Bound account ping-pong if the owner is still exhausted after restore.
        if state
            .context
            .as_ref()
            .and_then(|context| context.last_provider_error_at)
            .is_some_and(|at| chrono::Utc::now().signed_duration_since(at).num_seconds() < 60)
        {
            return Ok(None);
        }
    } else if lease.generation != 0 {
        return Err(conflict("provider error lease state missing"));
    }
    runtime
        .note_owner_progress(&lease.channel_id, checkpoint)
        .map_err(|error| conflict(error.message()))?;
    if runtime.observe(input).spawn.is_none() {
        return Ok(None);
    }
    if let Some(context) = runtime
        .states
        .get_mut(&lease.channel_id)
        .and_then(|state| state.context.as_mut())
    {
        context.last_provider_error_at = Some(chrono::Utc::now());
        if let Some(workspace) = workspace {
            context.workspace = workspace;
        }
    }
    Ok(Some(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::agent_recovery::tests::{CHANNEL, enabled_runtime};
    #[test]
    fn provider_error_preserves_context_and_rejects_duplicates_and_restore_ping_pong() {
        let mut runtime = enabled_runtime();
        let lease = RecoveryLease {
            channel_id: CHANNEL.into(),
            generation: 0,
            active_writer_agent_id: "claude".into(),
        };
        let input = ObserveInput {
            channel_id: CHANNEL.into(),
            primary_turn_id: "turn".into(),
            signal: DetectorSignal::TurnRateLimit,
        };
        let payload = CheckpointPayload::compact(
            "claude",
            "user request",
            "partial answer",
            "",
            vec![],
            "continue",
            "user request",
        );
        assert!(
            apply_provider_error(
                &mut runtime,
                &lease,
                input.clone(),
                payload.clone(),
                Some("/actual/worktree".into())
            )
            .unwrap()
            .is_some()
        );
        let state = &runtime.states[CHANNEL];
        assert_eq!(state.status, ChannelRecoveryStatus::TakeoverPending);
        assert_eq!(
            state.context.as_ref().unwrap().workspace,
            "/actual/worktree"
        );
        assert!(
            runtime
                .events(CHANNEL)
                .iter()
                .any(|event| event.payload.goal == "user request")
        );
        assert!(
            apply_provider_error(&mut runtime, &lease, input.clone(), payload.clone(), None)
                .is_err()
        );
        runtime.restore_owner(CHANNEL, true, "done").unwrap();
        let restored = RecoveryLease::from_state(&runtime.states[CHANNEL]);
        runtime.acknowledge_start(&restored).unwrap();
        assert!(
            apply_provider_error(&mut runtime, &restored, input, payload, None)
                .unwrap()
                .is_none()
        );
        assert_eq!(runtime.spawned().len(), 1);
    }
}
