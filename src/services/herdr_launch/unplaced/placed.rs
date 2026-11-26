use super::*;
use crate::db::dispatched_sessions::hosted_execution::{HostedObservation, retire_pg};
use crate::services::herdr_launch::retire_own_pending;
use crate::services::session_host::HostPresence;

pub(super) async fn resolve(
    target: &Unplaced<'_>,
    observed: &HostedObservation,
) -> Result<Resolution, String> {
    let HostedRecord::Known(record) = &observed.record else {
        return Err("unknown pending execution".into());
    };
    let location = record.location.as_ref().ok_or("pending pane not placed")?;
    if record.state != HostedState::Pending
        || &record.owner != target.owner
        || record.execution_nonce != target.nonce
        || *location != pane_location(target.endpoint, location.pane_id.clone())
    {
        return Err("the row is no longer this launch's Pending".into());
    }
    let context = execution_context(&target.owner.provider, target.nonce)?;
    let created = context.created_at.into();
    quiet_since(target.nonce, created, target.quiet, true)?;

    let (witness, plan, presence) = observe(target, location, created).await?;
    match plan {
        Plan::Held(why) => Err(why),
        Plan::Orphans(_) => Err("unrecorded executions share the channel's labels".into()),
        Plan::Adopt { pane_id } | Plan::Dispose { pane_id, .. } if pane_id != location.pane_id => {
            Err("another pane claims the placed launch".into())
        }
        _ if presence == HostPresence::Missing => {
            retire_missing(target, observed, record, location).await
        }
        Plan::Adopt { pane_id }
            if presence == HostPresence::Present && record.expected.is_none() =>
        {
            let (location, expected) =
                probe(target, witness, pane_id, target.nonce, created).await?;
            match record_launch_evidence_pg(
                target.pool,
                observed,
                target.owner,
                target.nonce,
                location,
                expected,
            )
            .await
            {
                Ok(HostedCasOutcome::Written) => Ok(Resolution::Adopted),
                other => Err(format!("adoption not recorded: {other:?}")),
            }
        }
        Plan::Dispose {
            pane_id,
            label,
            root,
        } if presence == HostPresence::Present && record.expected.is_none() => {
            let current = load_row(target.pool, target.owner)
                .await
                .map_err(|error| format!("{error:?}"))?;
            if &current != observed {
                return Err("the Pending row changed before residue close".into());
            }
            let residue = Residue {
                pane_id,
                label,
                root,
                witness,
            };
            close_residue(target, location, &residue).await?;
            retire_missing(target, observed, record, location).await
        }
        _ => Err(format!("placed pane presence: {presence:?}")),
    }
}

async fn observe(
    target: &Unplaced<'_>,
    location: &HostedLocation,
    created: SystemTime,
) -> Result<(ServerWitness, Plan, HostPresence), String> {
    let saved = if target.host.uses_launch_grants(target.endpoint) {
        let grant = target
            .host
            .grants()
            .load(target.endpoint, target.nonce)
            .await?
            .ok_or("pending launch grant missing")?;
        Some(
            grant
                .witness_for(target.endpoint, target.nonce)
                .map_err(|why| format!("pending grant: {why:?}"))?
                .clone(),
        )
    } else {
        None
    };
    let (endpoint, owner, nonce, pane) = (
        target.endpoint.clone(),
        target.owner.clone(),
        target.nonce.to_owned(),
        location.pane_id.clone(),
    );
    on_blocking_thread(&target.host, move |host| {
        let witness = match saved {
            Some(witness) => {
                if host.current_witness(&endpoint)? != witness {
                    return Err("pending grant witness changed".into());
                }
                witness
            }
            None => host.unplaced_witness(&endpoint, &nonce)?,
        };
        let panes = host.channel_panes(&endpoint, &owner.logical_key, &witness)?;
        let plan = judge(&panes, &owner, &nonce, created, &context_of_env_path);
        let presence = host.pane_presence(&endpoint, &pane, &witness)?;
        Ok((witness, plan, presence))
    })
    .await
    .unwrap_or_else(|| Err("placed pane read panicked".into()))
}

async fn close_residue(
    target: &Unplaced<'_>,
    location: &HostedLocation,
    residue: &Residue,
) -> Result<(), String> {
    let (endpoint, nonce, pane, residue) = (
        target.endpoint.clone(),
        target.nonce.to_owned(),
        location.pane_id.clone(),
        residue.clone(),
    );
    on_blocking_thread(&target.host, move |host| {
        host.dispose_residue(&endpoint, &nonce, &residue)?;
        match host.pane_presence(&endpoint, &pane, &residue.witness)? {
            HostPresence::Missing => Ok(()),
            presence => Err(format!("residue presence after close: {presence:?}")),
        }
    })
    .await
    .unwrap_or_else(|| Err("residue removal panicked".into()))
}

async fn retire_missing(
    target: &Unplaced<'_>,
    observed: &HostedObservation,
    record: &HostedExecution,
    location: &HostedLocation,
) -> Result<Resolution, String> {
    let outcome = if record.expected.is_some() {
        retire_pg(target.pool, observed, target.owner, target.nonce)
            .await
            .map_err(|error| format!("{error:?}"))?
    } else {
        retire_own_pending(target.pool, target.owner, target.nonce, Some(location)).await?
    };
    match outcome {
        HostedCasOutcome::Written if record.expected.is_some() => {
            Ok(Resolution::RetiredWithEvidence)
        }
        HostedCasOutcome::Written => Ok(Resolution::Retired),
        HostedCasOutcome::Stale => Err("the Pending row changed since the pane read".into()),
    }
}
