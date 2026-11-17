//! Discord capture and observation for engine-owned fence values.

use crate::config::ExecutionIdentityMode;

/// The `.spawn_nonce` read model lives under `super::super::tmux`, which is
/// `cfg(unix)`; this module is not.
#[cfg(unix)]
use super::super::tmux::execution_identity::{
    capture_spawn_nonce as capture_session_spawn_nonce, destruction_permitted_under_identity,
};

/// A platform that cannot host a tmux session has no marker to capture, so the
/// comparison is permanently `Unknown` there — the same "absence of evidence is
/// not proof" answer a missing marker gets on unix, and only `Enforce` turns it
/// into a deny. There is no counter to move because there is no marker store.
#[cfg(not(unix))]
fn capture_session_spawn_nonce(_tmux_session_name: &str) -> Option<String> {
    None
}

#[cfg(not(unix))]
fn destruction_permitted_under_identity(
    mode: ExecutionIdentityMode,
    _site: &'static str,
    _tmux_session_name: &str,
    _captured_spawn_nonce: Option<&str>,
) -> bool {
    !mode.denies_on_incarnation_mismatch()
}

#[cfg(test)]
static EXECUTION_IDENTITY_MODE_OVERRIDE: std::sync::Mutex<Option<ExecutionIdentityMode>> =
    std::sync::Mutex::new(None);

/// The live rollout stage for the #5071 T3 identity fence.
///
/// Read per decision through `config_live_reload::current()` so an
/// `agentdesk.yaml` edit applies without a restart, and `Legacy` before the boot
/// config is installed. `Legacy` is also the compiled-in default, so an
/// untouched deployment keeps each converted call site's pre-existing registry
/// CAS verbatim — the channel binding, session name, output path and cancel
/// pointer for `cancel_and_remove_channel_if_current`; the session key and the
/// cancel pointer alone for `remove_tmux_session_if_current`.
///
/// `Legacy` is NOT "this PR's behaviour reverted": #5071 T3-A1 deleted the two
/// #5067 in-flight emission fences outright, in every mode, and no value of this
/// switch brings them back. The switch only chooses whether the identity
/// conjuncts below are ignored, counted, or enforced. Restoring the fenced
/// baseline means reverting the T3-A1 PR.
pub(in crate::services::discord) fn execution_identity_mode() -> ExecutionIdentityMode {
    #[cfg(test)]
    if let Some(mode) = ExecutionIdentityModeGuard::current(&EXECUTION_IDENTITY_MODE_OVERRIDE) {
        return mode;
    }
    crate::config_live_reload::current()
        .map(|config| config.runtime.execution_identity_mode)
        .unwrap_or_default()
}

#[cfg(test)]
pub(in crate::services::discord) type ExecutionIdentityModeGuard =
    crate::services::discord::health::test_support::TestOverrideGuard<ExecutionIdentityMode>;

/// Override [`execution_identity_mode`] for the lifetime of the returned guard.
/// Process-global, so callers must already hold whichever test lock serializes
/// their suite.
#[cfg(test)]
pub(in crate::services::discord) fn set_execution_identity_mode_for_tests(
    mode: ExecutionIdentityMode,
) -> ExecutionIdentityModeGuard {
    ExecutionIdentityModeGuard::replace(&EXECUTION_IDENTITY_MODE_OVERRIDE, mode)
}

#[cfg(test)]
pub(in crate::services::discord) use crate::services::engine_runtime::relay_state::registry::set_delivery_fence_permitted_hook_for_tests;
pub(in crate::services::discord) use crate::services::engine_runtime::relay_state::registry::{
    TerminalDeliveryFence, WatcherIdentityFence,
};

impl WatcherIdentityFence {
    pub(in crate::services::discord) fn capture(
        mode: ExecutionIdentityMode,
        site: &'static str,
        tmux_session_name: &str,
    ) -> Self {
        let captured_spawn_nonce = mode
            .consults_spawn_nonce()
            .then(|| capture_session_spawn_nonce(tmux_session_name))
            .flatten();
        Self::from_capture(mode, site, tmux_session_name, captured_spawn_nonce)
    }
}

pub(super) fn observe_identity(
    mode: ExecutionIdentityMode,
    site: &'static str,
    session: &str,
    captured: Option<&str>,
) -> bool {
    destruction_permitted_under_identity(mode, site, session, captured)
}
