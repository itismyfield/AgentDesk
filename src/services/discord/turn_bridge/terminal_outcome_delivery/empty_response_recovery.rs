//! Empty-response recovery module boundary.

mod guidance;
mod handler;

pub(super) use handler::{
    EmptyResponseRecoveryContext, EmptyResponseRecoveryMessage, EmptyResponseRecoveryOutcome,
    EmptyResponseRecoveryState, handle_empty_response_recovery,
};
