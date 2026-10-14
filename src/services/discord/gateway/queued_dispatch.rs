use super::super::{queue_dispatch, queue_io};
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::services::discord) enum QueuedDispatchSuccess {
    Legacy,
    Ledger,
}

pub(super) async fn dispatch(
    gateway: &DiscordGateway,
    channel_id: ChannelId,
    intervention: &Intervention,
    request_owner_name: &str,
    has_more_queued_turns: bool,
    dispatch_lease: Option<Arc<crate::services::turn_orchestrator::DispatchLease>>,
) -> Result<QueuedDispatchSuccess, super::super::Error> {
    let candidate =
        router::QueuedAdmissionDisposition::ledger_candidate(&gateway.provider, channel_id);
    let mut admission_owner = candidate.then(|| {
        queue_io::QueuedAdmissionOwner::new(
            &gateway.shared,
            &gateway.provider,
            channel_id,
            intervention,
            dispatch_lease.clone(),
        )
    });
    // Legacy busy promotion restores the head before UI teardown and avoids a fast retry loop.
    if !candidate
        && router::defer_promoted_dispatch_if_hosted_tui_busy(
            &gateway.shared,
            &gateway.provider,
            channel_id,
            intervention,
            dispatch_lease
                .clone()
                .ok_or("queued dispatch is missing its dispatch lease")?,
        )
        .await
    {
        return Ok(QueuedDispatchSuccess::Legacy);
    }

    let Some(live_turn) = gateway.live_turn.as_ref() else {
        return Err(match admission_owner.take() {
            Some(owner) => Box::new(owner.fail("missing live Discord context".into())),
            None => "missing live Discord context".into(),
        });
    };

    // Voice queue items keep their original author; ordinary items keep the live turn owner.
    let dispatch_request_owner =
        queued_intervention_request_owner(intervention, live_turn.request_owner);
    let deps = router::IntakeDeps {
        http: &live_turn.ctx.http,
        cache: Some(&live_turn.ctx.cache),
        ctx_for_chained_dispatch: Some(&live_turn.ctx),
        shared: &gateway.shared,
        token: &live_turn.token,
    };
    if admission_owner.is_none()
        && router::QueuedAdmissionDisposition::ledger_candidate(&gateway.provider, channel_id)
    {
        admission_owner = Some(queue_io::QueuedAdmissionOwner::new(
            &gateway.shared,
            &gateway.provider,
            channel_id,
            intervention,
            dispatch_lease.clone(),
        ));
    }
    let disposition = router::admit_queued_intake(
        &deps,
        gateway.provider.clone(),
        channel_id,
        intervention,
        dispatch_request_owner,
        request_owner_name.to_string(),
        has_more_queued_turns,
        true,
        "intake_admission_pre_drain_defer",
        dispatch_lease.clone(),
    )
    .await;
    if !matches!(&disposition, router::QueuedAdmissionDisposition::Admitted(admitted) if admitted.is_ledger())
        && let Some(owner) = admission_owner.take()
    {
        return Err(Box::new(
            owner.fail("ledger candidate lost its typed admission".into()),
        ));
    }
    let admitted = match disposition {
        router::QueuedAdmissionDisposition::Admitted(admitted) => admitted,
        router::QueuedAdmissionDisposition::Deferred
        | router::QueuedAdmissionDisposition::RejectedNonPortableAttachment
        | router::QueuedAdmissionDisposition::ConsumedToHold => {
            return Ok(QueuedDispatchSuccess::Legacy);
        }
        router::QueuedAdmissionDisposition::RejectedRestore => {
            return Err("queued admission failed to restore dequeued head".into());
        }
    };

    let late_settlement = router::checkpoint_waits_for_admission(channel_id);
    if !late_settlement && !admitted.is_ledger() {
        let source_message_generations = intervention.source_message_queued_generations();
        queue_marker::drain_dispatched_queue_markers(
            &gateway.shared,
            &gateway.http,
            channel_id,
            intervention.message_id,
            &source_message_generations,
        )
        .await;

        // Dormant queued dispatch keeps the legacy marker/card handoff order.
        let drained = drain_merged_queued_placeholders(
            &gateway.shared,
            channel_id,
            intervention.message_id,
            &intervention.source_message_ids,
        )
        .await;
        for teardown in drained {
            let placeholder_msg_id = teardown.card();
            let result =
                queued_card_gate::teardown_delete(&gateway.http, &gateway.shared, teardown).await;
            // Record the result of deleting the released merged card.
            crate::services::observability::emit_relay_delete_result(
                gateway.provider.as_str(),
                channel_id.get(),
                placeholder_msg_id.get(),
                "gateway_queued_placeholder_drain",
                "delete_nonterminal",
                &result,
            );
        }
    }

    let result = match router::finish_admitted_queued_intake(&deps, admitted).await {
        Ok(result) => result,
        Err(error) => match error.downcast::<router::QueuedLedgerOutcome>() {
            Ok(outcome) => {
                let result =
                    queue_io::consume_queued_custody(&deps, outcome, admission_owner).await;
                #[cfg(all(test, unix))]
                let result = queue_dispatch::kickoff::ledger_tests::gateway_custody_result(result);
                result?;
                return Ok(QueuedDispatchSuccess::Ledger);
            }
            Err(error) => {
                return Err(match admission_owner {
                    Some(owner) => Box::new(owner.fail(error.to_string())),
                    None => error,
                });
            }
        },
    };
    if late_settlement && result == router::IntakeExecution::Executed {
        queue_dispatch::settle_executed_queued_intake(
            &gateway.shared,
            &gateway.http,
            &gateway.provider,
            channel_id,
            intervention,
            "gateway_queued_placeholder_drain",
        )
        .await;
    }
    Ok(QueuedDispatchSuccess::Legacy)
}
