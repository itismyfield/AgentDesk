use super::*;

fn close_if_idle_observed_verdict(
    state: &mut ChannelMailboxState,
    expected_claim: Option<ClaimObservation>,
) -> Result<(), &'static str> {
    if expected_claim.is_some_and(|observed| state.claim_log.observation() != observed) {
        return Err("claim_changed");
    }
    close_if_idle_verdict(state)
}

pub(in crate::services::turn_orchestrator) fn answer_close_if_idle(
    state: &mut ChannelMailboxState,
    msg: ChannelMailboxMsg,
    signal: &crate::services::turn_orchestrator::RecoveryDoneSignal,
) {
    let ChannelMailboxMsg::CloseIfIdle {
        expected_claim,
        reply,
    } = msg
    else {
        unreachable!("only CloseIfIdle reaches its reply handler");
    };
    let verdict = close_if_idle_observed_verdict(state, expected_claim);
    if verdict.is_ok() {
        signal.retirement.retire();
    }
    let _ = reply.send(verdict);
}
