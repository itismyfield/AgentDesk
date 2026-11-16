//! Physical termination consumes its owner's proof and preserves the close reply.

use super::{HostKind, HostRefusal};
use crate::services::termination_audit::host_terminate::{
    HostTerminateEffect, HostTerminateWarrant, TERMINATE_OP,
};

pub(super) fn terminate(kind: HostKind, warrant: HostTerminateWarrant<'_>) -> HostTerminateEffect {
    if warrant.kind() != kind {
        return HostTerminateEffect::Refused(HostRefusal::Precondition(format!(
            "warrant targets a {} session, host is {}",
            warrant.kind().as_str(),
            kind.as_str()
        )));
    }
    #[cfg(unix)]
    if let Some(operator) = warrant.into_operator() {
        return HostTerminateEffect::Close(close_operator(operator));
    }
    HostTerminateEffect::Refused(HostRefusal::Unsupported {
        kind,
        op: TERMINATE_OP,
    })
}

#[cfg(unix)]
fn close_operator(
    warrant: crate::services::termination_audit::host_terminate::herdr_terminate::OperatorTerminateWarrant<'_>,
) -> crate::services::termination_audit::host_terminate::herdr_terminate::HerdrTerminateResult {
    use crate::services::termination_audit::host_terminate::herdr_terminate::{
        HerdrTerminateResult, TerminateRefusal,
    };

    let (target, fence) = warrant.into_parts();
    if !crate::config_live_reload::current()
        .is_some_and(|config| config.runtime.herdr_terminate_enabled == Some(true))
    {
        target.discard_pin();
        return HerdrTerminateResult::Refused(TerminateRefusal::Disabled);
    }
    let judged = match fence.as_ref() {
        Some(fence) => target.pin_terminate_fenced(fence),
        None => target.pin_terminate(),
    };
    if let Err(why) = judged {
        return HerdrTerminateResult::Refused(TerminateRefusal::Gate(why));
    }
    target.send_close_pinned().into()
}
