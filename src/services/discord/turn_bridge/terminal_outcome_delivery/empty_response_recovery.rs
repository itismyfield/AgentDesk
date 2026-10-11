//! Empty-response recovery module boundary.

mod guidance;
pub(super) use guidance::empty_response_guidance;
mod handler;
#[cfg(test)]
pub(in crate::services::discord::turn_bridge) use handler::output_growth;

pub(super) use handler::{
    EmptyResponseRecoveryContext, EmptyResponseRecoveryMessage, EmptyResponseRecoveryOutcome,
    EmptyResponseRecoveryState, handle_empty_response_recovery,
};
