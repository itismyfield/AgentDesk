//! A Pending execution from an earlier turn: attached once its own start is logged, or refused
//! with a typed cause the owner waits on.
use super::*;

/// A Pending execution from an earlier turn, attached once the log holds its own start; a pane
/// without evidence is proven first, and each refusal is typed by its cause for the owner's wait.
pub(super) fn pending_source(
    turn: &HerdrTurn<'_>,
    runtime: &Handle,
    ports: &dyn HerdrTurnPorts,
    record: &HostedExecution,
) -> Result<Attached, String> {
    use crate::domain::retry_wait::RetryCause as Cause;
    let nonce = &record.execution_nonce;
    let adopted = record.location.is_none() && record.expected.is_none();
    if adopted && unplaced_resolved(turn, runtime, ports, nonce)? == Resolution::Retired {
        return launched_source(turn, runtime, ports);
    }
    let session_id = execution_context("claude", nonce)?
        .expected_native_session_id
        .ok_or("herdr turn: the pending launch named no session")?;
    let refused =
        |cause: Cause, detail: String| retry_probe::refused(turn, &session_id, cause, &detail);
    let unproven = || Cause::PendingUnproven {
        nonce: nonce.clone(),
    };
    let proven;
    let record = match record.location {
        Some(_) if record.expected.is_none() => {
            if let Err(gap) = never_attached::probe_again(turn, runtime, ports) {
                let detail = never_attached::refusal(turn, record, &session_id, Err(gap));
                return Err(refused(unproven(), detail));
            }
            proven = reloaded(turn, runtime, nonce).map_err(|e| refused(unproven(), e))?;
            &proven
        }
        _ => record,
    };
    let logical = turn.owner.logical_key.as_str();
    if !start_logged(turn.channel_id, logical, nonce, &session_id) {
        let (cause, detail) = match adopted {
            true => (
                Cause::PendingUnplaced {
                    nonce: nonce.clone(),
                },
                format!(
                    "herdr turn: pending execution {nonce} took its launched pane; no start of \
                     {session_id} is logged yet"
                ),
            ),
            false => {
                let evidence = record
                    .expected
                    .as_ref()
                    .map(drop)
                    .ok_or("no evidence recorded");
                let evidence = evidence.map_err(str::to_owned);
                let detail = never_attached::refusal(turn, record, &session_id, evidence);
                (
                    Cause::PendingUnstarted {
                        nonce: nonce.clone(),
                    },
                    detail,
                )
            }
        };
        return Err(refused(cause, detail));
    }
    attach(turn, runtime, ports, nonce, &session_id)
}

/// Pending execution `nonce` read again once its probe recorded the evidence it lacked.
fn reloaded(
    turn: &HerdrTurn<'_>,
    runtime: &Handle,
    nonce: &str,
) -> Result<HostedExecution, String> {
    match runtime.block_on(load(turn.pool, &turn.owner)) {
        HostedLookup::Found(found) => match found.record {
            HostedRecord::Known(record)
                if record.execution_nonce == nonce && record.state == HostedState::Pending =>
            {
                Ok(record)
            }
            other => Err(format!("herdr turn: row moved on: {other:?}")),
        },
        other => Err(format!("herdr turn: row unreadable: {other:?}")),
    }
}

/// The turn's resolution of its unplaced Pending `nonce`: adopted, or retired for a fresh launch.
fn unplaced_resolved(
    turn: &HerdrTurn<'_>,
    runtime: &Handle,
    ports: &dyn HerdrTurnPorts,
    nonce: &str,
) -> Result<Resolution, String> {
    let host = ports.launch_host().ok_or("herdr turn: no launch host")?;
    let target = Unplaced {
        pool: turn.pool,
        owner: &turn.owner,
        nonce,
        endpoint: &turn.endpoint,
        host,
        quiet: unplaced::quiet(),
        close_unrecorded: ports.owner_port().close_unrecorded,
    };
    unplaced::resolve_for_turn(runtime, ports.unplaced_lease(&turn.owner), target)
}
