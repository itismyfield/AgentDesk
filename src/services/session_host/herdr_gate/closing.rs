//! Whether a termination chose an execution: the mark every writer of a live start reads under
//! the pane's source authority before it records, publishes or binds that execution.

use std::sync::PoisonError;

use super::execution_fences;

/// Whether a termination shut execution `nonce`'s fence; the mark outlives the fence's targets
/// and is lost only with the process.
pub fn execution_closing(nonce: &str) -> bool {
    let fences: Vec<_> = execution_fences()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .filter(|((.., execution), _)| execution == nonce)
        .map(|(_, fence)| fence.clone())
        .collect();
    fences
        .iter()
        .any(|fence| fence.state.lock().unwrap_or_else(PoisonError::into_inner).1)
}

/// Test seam: leaves execution `nonce` shut, as a termination that sent its close does.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn shut_execution_for_test(nonce: &str) {
    let key = |part: &str| part.to_string();
    let fence = super::fence_at((
        key("node"),
        key("key"),
        key("socket"),
        key("session"),
        key("pane"),
        key(nonce),
    ));
    fence.state.lock().unwrap_or_else(PoisonError::into_inner).1 = true;
}
